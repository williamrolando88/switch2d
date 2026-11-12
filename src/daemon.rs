//! Daemon orchestration: one hub thread, one worker task per pad, one status
//! supervisor task (architecture §10, task 09B).
//!
//! ```text
//! main (tokio)
//!  ├─ supervisor task   : 1.5 s tick + activate/teardown -> state.json
//!  ├─ Hub thread (std)  : poll_once / dial_pending, owns ScanControl + AdvertSource
//!  │     └─ Dialer::dial: controller::connect_with (blocking, single dialer)
//!  │           └─ sends SwitchController over a per-pad channel to the worker
//!  └─ worker task/pad   : owns uinput node, Worker state, idle sleep
//!        └─ ATT reader thread -> try_send(report) on a bounded(64) channel
//! ```
//!
//! Invariants (see `docs/architecture.md` §10):
//!
//! * **One dialer.** Only the hub thread calls `connect_with`, so two LE
//!   create-connections are impossible (HANDOVER Bug 2) and no worker-side
//!   connect lock is needed.
//! * **The ATT reader never blocks.** Its callback does exactly one `try_send`
//!   into a bounded channel and bumps a drop counter; a full channel drops the
//!   frame instead of stalling command responses (analysis §5.6).
//! * **Workers never dial.** When a session ends they tell the hub over a std
//!   channel; the hub re-arms the pad so the next *live advert* dials again.
//! * **The node survives sessions.** Teardown releases keys/axes (rumble stop
//!   is not wired yet) and keeps the uinput node; the next session reuses it.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver as StdReceiver, Sender as StdSender, TryRecvError};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::bluetooth::bluez::BluezAdvertSource;
use crate::bluetooth::scan::{AdvertSource, ScanControl, ShellScanControl};
use crate::config::{BondedController, DaemonConfig, DaemonMode};
use crate::controller::{BondPolicy, CalibratedInput, ConnectOptions, SwitchController};
use crate::dsu_bridge::DsuBridge;
use crate::hub::{DialResult, Dialer, Hub, HubConfig, HubOutcome};
use crate::players::{path_in, LedPlayers};
use crate::protocol::constants::PRO_CONTROLLER2_PID;
use crate::protocol::report::InputReport;
use crate::protocol::vibration::SUSTAIN_INTERVAL;
use crate::rumble::{RumbleAction, RumbleEngine};
use crate::status::{
    battery_pct_from_mv, ControllerStatus, DaemonStatus, ServiceState, StatusPublisher,
};
use crate::uinput::ff::FfEvent;
use crate::uinput::{CombinedSide, MotionNode, PadSink, Side, VirtualGamepad};
use crate::worker::{Worker, WorkerConfig};

/// Bounded input-report channel: enough to absorb a scheduling hiccup without
/// letting a stalled worker hold the ATT reader (analysis §5.6).
const REPORT_CHANNEL_CAPACITY: usize = 64;

/// How often a live session re-checks liveness and the idle timer.
const SESSION_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Hub error backoff floor/ceiling (architecture §10, sequencing rule 2).
const HUB_BACKOFF_MIN: Duration = Duration::from_millis(250);
const HUB_BACKOFF_MAX: Duration = Duration::from_millis(2000);

/// Status publication cadence (legacy `_STATUS_INTERVAL_S`).
const STATUS_INTERVAL: Duration = Duration::from_millis(1500);

pub struct Daemon {
    config: DaemonConfig,
    running: Arc<AtomicBool>,
}

impl Daemon {
    pub fn new(config: DaemonConfig) -> Self {
        Self {
            config,
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Run the daemon: spawn the supervisor, one worker per configured pad and
    /// the hub thread, then park (the daemon only stops when the process does).
    pub async fn run(&self) -> std::io::Result<()> {
        // Single instance guard (install-report finding 1): two daemons would
        // fight over the adapter and the uinput nodes. The guard is held for
        // the whole process lifetime.
        let _instance = match acquire_instance_lock(&instance_lock_path()) {
            Ok(guard) => guard,
            Err(err) => {
                error!("{err}");
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another switch2d is already running",
                ));
            }
        };

        let adapter_mac = self.config.adapter_mac.clone();
        if adapter_mac.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No Bluetooth adapter MAC configured. Run 'switch2d pair' first.",
            ));
        }

        info!("Starting switch2d with adapter {}", adapter_mac);
        info!("Active mode: {:?}", self.config.mode);

        if self.config.controllers.is_empty() {
            warn!("No controllers paired yet. Run 'switch2d pair' to bond Joy-Cons.");
            return Ok(());
        }

        let pads: Vec<PadSpec> = self
            .config
            .controllers
            .iter()
            .map(PadSpec::from_config)
            .collect();
        let idle_timeout = Duration::from_secs(self.config.idle_sleep_seconds.max(1));
        let horizontal = self.config.mode == DaemonMode::Split;
        let face_layout = self.config.face_button_layout;

        // ADR-4 player identity: the supervisor re-reads `led-players.json` on
        // every publish (1.5 s tick and session events) and stores the per-pad
        // plan here; workers follow it on their 500 ms liveness tick, so no
        // thread ever caches the file across ticks.
        let planner = Arc::new(PlayerPlanner::new(pads.clone(), self.config.mode));
        let led_plan: Arc<StdMutex<HashMap<String, u8>>> =
            Arc::new(StdMutex::new(planner.read_plan()));

        // Status plane: the supervisor is the only writer.
        let (sup_tx, sup_rx) = tokio_mpsc::unbounded_channel();
        let publisher = StatusPublisher::new();
        let supervisor = tokio::spawn(supervisor_loop(
            sup_rx,
            publisher,
            pads.clone(),
            Arc::clone(&planner),
            Arc::clone(&led_plan),
        ));

        // Worker -> hub notifications (low rate, std channel: the hub thread is
        // synchronous and polls it once per cycle).
        let (hub_tx, hub_rx) = mpsc::channel::<HubMsg>();

        // Per-pad channels: sessions (hub -> worker) and reports (reader thread
        // -> worker). The report Sender is cloned into the dialer's callback.
        let mut session_senders: HashMap<String, SessionSender> = HashMap::new();
        let mut links: HashMap<String, PadLink> = HashMap::new();
        let mut receivers = Vec::with_capacity(pads.len());
        for pad in &pads {
            let (session_tx, session_rx) = tokio_mpsc::channel::<SwitchController>(1);
            let (report_tx, report_rx) =
                tokio_mpsc::channel::<InputReport>(REPORT_CHANNEL_CAPACITY);
            let drops = Arc::new(AtomicU64::new(0));

            session_senders.insert(pad.mac.clone(), SessionSender(session_tx));
            links.insert(
                pad.mac.clone(),
                PadLink {
                    reports: report_tx,
                    drops: Arc::clone(&drops),
                },
            );
            receivers.push((pad.clone(), session_rx, report_rx, drops));
        }

        // DSU motion (ADR-3). Optional: a failed bind/spawn leaves a no-op
        // bridge, and neither outcome may abort startup.
        let motion = start_motion();

        // Sinks. Combined mode maps both pads into one node; split mode gives
        // each pad its own node (created once, reused by every session).
        let combined_gamepad = match self.config.mode {
            DaemonMode::Combined => Some(Arc::new(StdMutex::new(VirtualGamepad::new(
                "Nintendo Switch 2 Combined Joy-Cons",
                PRO_CONTROLLER2_PID,
                "switch2-combined",
            )?))),
            DaemonMode::Split => None,
        };

        for (pad, session_rx, report_rx, drops) in receivers {
            let hub_tx = hub_tx.clone();
            let sup_tx = sup_tx.clone();
            match &combined_gamepad {
                Some(gamepad) => {
                    // `uinput::combined::CombinedSide` is the single owner of
                    // the per-side mapping (one table in uinput::mapping).
                    let mut sink = CombinedSide::new(Arc::clone(gamepad), pad.side(), horizontal);
                    sink.set_face_layout(face_layout);
                    tokio::spawn(run_pad_worker(
                        pad,
                        sink,
                        session_rx,
                        report_rx,
                        drops,
                        hub_tx,
                        sup_tx,
                        idle_timeout,
                        Arc::clone(&planner),
                        Arc::clone(&led_plan),
                        Arc::clone(&motion),
                        RealMotionFactory,
                    ));
                }
                None => {
                    let name = format!("Nintendo Switch 2 {}", pad.name);
                    let mut node = VirtualGamepad::new(&name, pad.product_id, &pad.mac)?;
                    node.set_face_layout(face_layout);
                    let gamepad = Arc::new(StdMutex::new(node));
                    let sink = SharedPadSink { gamepad };
                    tokio::spawn(run_pad_worker(
                        pad,
                        sink,
                        session_rx,
                        report_rx,
                        drops,
                        hub_tx,
                        sup_tx,
                        idle_timeout,
                        Arc::clone(&planner),
                        Arc::clone(&led_plan),
                        Arc::clone(&motion),
                        RealMotionFactory,
                    ));
                }
            }
        }

        // One OS thread owns the Hub (it shells out and sleeps, so it must not
        // occupy a tokio worker).
        let queue = SessionQueue::default();
        let dialer = ControllerDialer::new(adapter_mac.clone(), links, &pads, queue.clone());
        let mut hub = Hub::new(
            BluezAdvertSource::new(),
            ShellScanControl::from_env(),
            dialer,
            HubConfig {
                // The install report showed BlueZ advertising Device1 objects
                // with no RSSI and no manufacturer data on some machines, which
                // made the advert-only hub mute. Keep adverts authoritative but
                // fall back to a timed dial when a known pad is invisible.
                fallback_after: Some(Duration::from_secs(4)),
                fallback_retry: Duration::from_secs(3),
                ..HubConfig::default()
            },
        );
        hub.set_adapter_mac(&adapter_mac);
        for pad in &pads {
            hub.register_known(&pad.mac);
        }

        // A second handle on the same HCI index, used only to undo the
        // `prepare_global` stop after a dial batch (same thread as the hub).
        let discovery_restorer = ShellScanControl::from_env();
        let running = Arc::clone(&self.running);
        std::thread::Builder::new()
            .name("switch2d-hub".to_string())
            .spawn(move || {
                hub_thread_loop(
                    hub,
                    queue,
                    session_senders,
                    hub_rx,
                    sup_tx,
                    running,
                    discovery_restorer,
                )
            })?;

        // The supervisor and the workers run for the lifetime of the process.
        let _ = supervisor.await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hub thread
// ---------------------------------------------------------------------------

/// Notification from a worker to the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HubMsg {
    /// The worker's session ended (link loss, idle sleep or teardown).
    SessionEnded(String),
}

/// Hub -> worker session delivery handle.
#[derive(Clone)]
struct SessionSender(tokio_mpsc::Sender<SwitchController>);

impl SessionSender {
    /// Deliver an established session to its worker.
    ///
    /// Blocks only until the worker takes it (the channel holds one session):
    /// dial success must reach the worker *before* the hub polls again, or the
    /// still-advertising pad queues a second dial for a link that is already up
    /// (architecture §10, sequencing rule 3).
    fn send(&self, controller: SwitchController) -> bool {
        self.0.blocking_send(controller).is_ok()
    }
}

/// The scheduler loop. Never exits on error: hub failures back off and retry
/// (architecture §10, sequencing rule 2).
#[allow(clippy::too_many_arguments)]
fn hub_thread_loop(
    mut hub: Hub<BluezAdvertSource, ShellScanControl, ControllerDialer>,
    queue: SessionQueue,
    sessions: HashMap<String, SessionSender>,
    hub_rx: StdReceiver<HubMsg>,
    sup_tx: UnboundedSender<SupervisorMsg>,
    running: Arc<AtomicBool>,
    discovery_restorer: ShellScanControl,
) {
    let config = HubConfig::default();
    let mut backoff = HUB_BACKOFF_MIN;

    while running.load(Ordering::Relaxed) {
        drain_hub_messages(&mut hub, &hub_rx);

        match hub.poll_once(Instant::now()) {
            Ok(HubOutcome::AllConnected) => {
                sup_tx.send(SupervisorMsg::Scanning(false)).ok();
                std::thread::sleep(config.all_connected_sleep);
                backoff = HUB_BACKOFF_MIN;
            }
            Ok(HubOutcome::Idle) => {
                sup_tx.send(SupervisorMsg::Scanning(true)).ok();
                std::thread::sleep(config.cycle_sleep);
                backoff = HUB_BACKOFF_MIN;
            }
            Ok(HubOutcome::Pending(pending)) => {
                std::thread::sleep(config.dial_pause);
                let results = hub.dial_pending(&pending);
                // `dial_pending` stops discovery before dialing so the LE
                // create-connection succeeds, but the hub still believes
                // discovery is running and will not restart it until
                // `idle_restart_cycles` (~20 s). Restore it now or a second
                // pad's wake advert would go unseen for that long.
                if let Err(err) = discovery_restorer.start_le_scan() {
                    warn!("Could not restart discovery after dialing: {err}");
                }
                for result in &results {
                    if let Err(err) = &result.outcome {
                        log_dial_failure(&result.mac, err);
                    }
                    let controller = queue.take(&result.mac);
                    let delivered = match controller {
                        Some(controller) => sessions
                            .get(&result.mac)
                            .map(|sender| sender.send(controller))
                            .unwrap_or(false),
                        None => false,
                    };
                    if handoff_session(&mut hub, result, || delivered) {
                        info!("{}: session handed to its worker", result.mac);
                    }
                }
                sup_tx.send(SupervisorMsg::Scanning(true)).ok();
                backoff = HUB_BACKOFF_MIN;
            }
            Err(err) => {
                warn!("Hub cycle failed: {err}");
                sup_tx
                    .send(SupervisorMsg::HubError(Some(err.to_string())))
                    .ok();
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(HUB_BACKOFF_MAX);
            }
        }
    }
}

/// Apply worker notifications before polling (re-arming pads whose session ended).
fn drain_hub_messages<S, C, D>(hub: &mut Hub<S, C, D>, rx: &StdReceiver<HubMsg>)
where
    S: AdvertSource,
    C: ScanControl,
    D: Dialer,
{
    loop {
        match rx.try_recv() {
            Ok(HubMsg::SessionEnded(mac)) => {
                debug!("{mac}: session ended, re-arming the pad");
                rearm_pad(hub, &mac);
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
}

/// Hand a freshly dialled session to its worker, then tell the hub the pad is
/// up. Returns whether the pad was marked connected.
///
/// `deliver` is the actual handoff (channel send); marking connected only
/// happens when the worker really accepted the session, so a dead worker can
/// never leave a pad permanently "connected" and invisible to the scanner.
fn handoff_session<S, C, D>(
    hub: &mut Hub<S, C, D>,
    result: &DialResult,
    deliver: impl FnOnce() -> bool,
) -> bool
where
    S: AdvertSource,
    C: ScanControl,
    D: Dialer,
{
    if let Err(err) = &result.outcome {
        debug!("Dial {} failed: {}", result.mac, err);
        return false;
    }
    if !deliver() {
        warn!(
            "worker for {} is gone; dropping the established session",
            result.mac
        );
        // A successful dial already marks the pad connected; without a worker
        // that session is lost, so put the pad straight back into the dialable
        // state instead of leaving it "connected" and invisible to the scanner.
        rearm_pad(hub, &result.mac);
        return false;
    }
    hub.mark_connected(&result.mac);
    true
}

/// A worker's session ended: re-arm the pad so the hub dials it again on its
/// next *live advert* (`register_known` also clears the connected flag).
fn rearm_pad<S, C, D>(hub: &mut Hub<S, C, D>, mac: &str)
where
    S: AdvertSource,
    C: ScanControl,
    D: Dialer,
{
    hub.register_known(mac);
}

/// Warn once per pad slot about a failed dial: the hub only dials pads that are
/// advertising, so a failure is worth seeing once and then only at debug level.
fn log_dial_failure(mac: &str, err: &str) {
    use std::sync::atomic::AtomicBool;
    static WARNED: [AtomicBool; 8] = [
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    ];
    let slot = mac.chars().last().and_then(|c| c.to_digit(16)).unwrap_or(0);
    let slot = (slot as usize) % WARNED.len();

    if !WARNED[slot].swap(true, Ordering::Relaxed) {
        warn!("Connect to {mac} failed: {err}");
    } else {
        debug!("Connect to {mac} failed (retrying): {err}");
    }
}

// ---------------------------------------------------------------------------
// Dialer
// ---------------------------------------------------------------------------

/// Report channel + drop counter for one pad (cloned into the ATT callback).
#[derive(Clone)]
struct PadLink {
    reports: tokio_mpsc::Sender<InputReport>,
    drops: Arc<AtomicU64>,
}

/// Sessions the dialer established, waiting to be handed to their workers.
///
/// The hub owns the dialer, so the daemon cannot read a result out of it after
/// `dial_pending`; the dialer therefore parks each new controller here and the
/// hub thread drains it immediately afterwards. This keeps `src/hub.rs`
/// untouched.
#[derive(Clone, Default)]
struct SessionQueue(Arc<StdMutex<Vec<(String, SwitchController)>>>);

impl SessionQueue {
    fn push(&self, mac: &str, controller: SwitchController) {
        let mut queue = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.push((mac.to_ascii_uppercase(), controller));
    }

    fn take(&self, mac: &str) -> Option<SwitchController> {
        let mut queue = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = mac.to_ascii_uppercase();
        let index = queue.iter().position(|(entry, _)| *entry == key)?;
        Some(queue.remove(index).1)
    }
}

/// The only code path that dials a controller.
struct ControllerDialer {
    adapter_mac: String,
    links: HashMap<String, PadLink>,
    players: HashMap<String, (u8, BondPolicy)>,
    queue: SessionQueue,
}

impl ControllerDialer {
    fn new(
        adapter_mac: String,
        links: HashMap<String, PadLink>,
        pads: &[PadSpec],
        queue: SessionQueue,
    ) -> Self {
        let players = pads
            .iter()
            .map(|pad| {
                let bond = if pad.bonded {
                    BondPolicy::Skip
                } else {
                    BondPolicy::IfNeeded
                };
                (pad.mac.clone(), (pad.player, bond))
            })
            .collect();
        Self {
            adapter_mac,
            links,
            players,
            queue,
        }
    }
}

impl Dialer for ControllerDialer {
    fn dial(&mut self, mac: &str) -> io::Result<()> {
        let key = mac.to_ascii_uppercase();
        let link = self.links.get(&key).cloned().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("{mac} is not configured"))
        })?;
        let (player, bond) = *self.players.get(&key).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("{mac} is not configured"))
        })?;

        // The ATT reader thread only ever does one non-blocking send per report
        // and one atomic increment on drop: it must never block on the worker
        // (analysis §5.6).
        let reports = link.reports;
        let drops = link.drops;
        let input_cb: crate::controller::ControllerInputCallback = Box::new(move |report| {
            if reports.try_send(report.clone()).is_err() {
                drops.fetch_add(1, Ordering::Relaxed);
            }
        });

        // The hub has already stopped discovery (`prepare_global`) and dropped
        // the cached BlueZ object for this MAC; one connect attempt is enough
        // because the pad is advertising right now. A failure simply re-arms on
        // the next advert.
        let controller = SwitchController::connect_with(
            mac,
            &self.adapter_mac,
            ConnectOptions { player, bond },
            input_cb,
        )?;

        self.queue.push(&key, controller);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Workers
// ---------------------------------------------------------------------------

/// Static per-pad identity from the config (never mutated at runtime).
#[derive(Debug, Clone)]
struct PadSpec {
    /// Upper-case MAC.
    mac: String,
    player: u8,
    name: String,
    bonded: bool,
    product_id: u16,
    /// `true` when the config name says this pad is a left Joy-Con (legacy
    /// heuristic, kept as-is: combined/split mapping needs it).
    is_left: bool,
}

impl PadSpec {
    fn from_config(entry: &BondedController) -> Self {
        Self {
            mac: entry.mac.to_ascii_uppercase(),
            player: entry.player,
            name: entry.name.clone(),
            bonded: entry.bonded,
            product_id: entry.product_id,
            is_left: entry.name.to_lowercase().contains("left"),
        }
    }

    fn side(&self) -> Side {
        if self.is_left {
            Side::Left
        } else {
            Side::Right
        }
    }
}

/// One worker task: waits for sessions and runs them until the link drops or
/// the pad goes idle. The uinput node lives in the sink and survives sessions.
#[allow(clippy::too_many_arguments)]
async fn run_pad_worker<S, F>(
    pad: PadSpec,
    mut sink: S,
    mut sessions: tokio_mpsc::Receiver<SwitchController>,
    mut reports: tokio_mpsc::Receiver<InputReport>,
    drops: Arc<AtomicU64>,
    hub_tx: StdSender<HubMsg>,
    sup_tx: UnboundedSender<SupervisorMsg>,
    idle_timeout: Duration,
    planner: Arc<PlayerPlanner>,
    led_plan: Arc<StdMutex<HashMap<String, u8>>>,
    motion: SharedMotion,
    motion_factory: F,
) where
    S: PadSink + FfPad + Clone,
    F: MotionNodeFactory,
{
    // The IMU node has the same lifetime rule as the uinput gamepad: created
    // once per worker, kept across reconnects, dropped only when the daemon
    // (or this task) ends. A failure disables motion for this pad for good and
    // is logged once, never fatal.
    let mut imu = create_imu_node(&motion_factory, &pad);

    while let Some(controller) = sessions.recv().await {
        run_session(
            &pad,
            &mut sink,
            controller,
            &mut reports,
            &drops,
            &hub_tx,
            &sup_tx,
            idle_timeout,
            &planner,
            &led_plan,
            &motion,
            &mut imu,
        )
        .await;
    }
}

/// Drive one established session until it ends.
#[allow(clippy::too_many_arguments)]
async fn run_session<S>(
    pad: &PadSpec,
    sink: &mut S,
    mut controller: SwitchController,
    reports: &mut tokio_mpsc::Receiver<InputReport>,
    drops: &AtomicU64,
    hub_tx: &StdSender<HubMsg>,
    sup_tx: &UnboundedSender<SupervisorMsg>,
    idle_timeout: Duration,
    planner: &PlayerPlanner,
    led_plan: &Arc<StdMutex<HashMap<String, u8>>>,
    motion_sink: &SharedMotion,
    imu: &mut Option<Box<dyn MotionOut>>,
) where
    S: PadSink + FfPad + Clone,
{
    // Frames queued for a previous session must not be applied to this one.
    while reports.try_recv().is_ok() {}

    let mut worker = Worker::new(
        WorkerConfig {
            mac: pad.mac.clone(),
            player: pad.player,
            idle_timeout,
        },
        sink.clone(),
    );
    info!(
        "{}: session active (player {}) - calibration from this session",
        pad.mac, pad.player
    );
    sup_tx
        .send(SupervisorMsg::Activated {
            mac: pad.mac.clone(),
        })
        .ok();

    // ADR-4 player identity. The connect handshake already set the configured
    // player's LED; from here on the effective number comes from
    // `led-players.json` (re-read now, at session start). Applied only when it
    // differs, and only after the handshake is done.
    let mut led_player = pad.player;
    let wanted = planner.player_for(&pad.mac);
    sync_player_led(&mut controller, &pad.mac, wanted, &mut led_player);

    // DSU slot uses the same effective player as the LED, so the two views can
    // never disagree. Battery is still unknown at handshake end: the bridge
    // gets the initial value 0 and is not re-notified on the first report.
    let motion = MotionSession::start(motion_sink.as_ref(), wanted, &pad.mac, 0);
    debug!(
        "{}: DSU slot {} (effective player {})",
        pad.mac,
        motion.slot(),
        wanted
    );

    // Rumble: profile-routed engine (HD sustainer or GameCube presets), fed by
    // the uinput FF queue drained from the pad sink.
    let mut rumble = RumbleEngine::new(controller_profile(&controller));
    // Kernel-space level last handed to the engine. Comparing the level (not
    // the event alone) keeps the two halves of a combined node coherent: either
    // worker may drain the shared node's events, and `ff_target()` is the
    // authoritative state afterwards.
    let mut last_ff = (0u16, 0u16);
    // Absolute 16 ms rumble grid. The timer arm below wakes exactly at the next
    // slot, so a silent pad costs one wake-up per slot and a rumbling one can
    // never burst (the sustainer also refuses more than one packet per slot).
    let mut next_rumble = Instant::now();

    let mut battery_mv = 0u16;
    let mut idle_sleep = false;

    loop {
        let rumble_deadline = tokio::time::Instant::from_std(next_rumble);
        tokio::select! {
            maybe = reports.recv() => {
                let Some(report) = maybe else { break };
                // The controller owns the calibration read during its connect
                // handshake; use it rather than any daemon-side default.
                let calibrated = controller.calibrated_input(&report);
                if let Err(err) = worker.on_report(&report, &calibrated, Instant::now()) {
                    warn!("{}: uinput update failed: {err}", pad.mac);
                }
                if worker.battery_mv() != battery_mv {
                    battery_mv = worker.battery_mv();
                    sup_tx
                        .send(SupervisorMsg::Battery {
                            mac: pad.mac.clone(),
                            mv: battery_mv,
                        })
                        .ok();
                }
                let now = Instant::now();
                if rumble_due(&mut next_rumble, now) {
                    pump_rumble(sink, &mut rumble, &mut controller, now, &mut last_ff);
                }
                // IMU node first (local consumers), then the DSU frame
                // (network): both are fire-and-forget and the pad mutex is not
                // held here. Raw i16 samples only - the node publishes the
                // scale through its axis resolution.
                publish_imu(imu.as_deref_mut(), &report);

                // Fire-and-forget: the bridge never blocks or fails the
                // session.
                motion.update(&report, &calibrated);

                let sleep_now = worker.should_sleep(now);
                if !controller.is_connected() {
                    break;
                }
                if sleep_now {
                    idle_sleep = true;
                    break;
                }
            }
            _ = tokio::time::sleep_until(rumble_deadline) => {
                // No report is needed to keep a sustained rumble alive: the
                // pad only ever sees the 16 ms sustain packets from here.
                let now = Instant::now();
                next_rumble = now + SUSTAIN_INTERVAL;
                pump_rumble(sink, &mut rumble, &mut controller, now, &mut last_ff);
            }
            _ = sleep(SESSION_POLL_INTERVAL) => {
                // Follow the supervisor's plan (refreshed every 1.5 s): the
                // override file can change while the daemon runs.
                apply_planned_led(&mut controller, &pad.mac, led_plan, &mut led_player);
                if !controller.is_connected() {
                    break;
                }
                if worker.should_sleep(Instant::now()) {
                    idle_sleep = true;
                    break;
                }
            }
        }
    }

    if idle_sleep {
        info!(
            "{}: idle for {:?}, putting the pad to sleep",
            pad.mac, idle_timeout
        );
    }
    // Teardown order (deviation 6): silence rumble first (exactly one stop
    // packet on HD pads; the GameCube path cancels its queued clip), then
    // release every key/axis. The uinput node stays open for the next session.
    teardown_with_rumble(
        &mut worker,
        &mut rumble,
        &mut controller,
        &pad.mac,
        Instant::now(),
    );
    if idle_sleep {
        // Waits (<= 1 s) for the HCI-state reply, then closes the link.
        controller.sleep();
    }
    drop(controller);

    let dropped = drops.swap(0, Ordering::Relaxed);
    if dropped > 0 {
        warn!(
            "{}: dropped {dropped} input report(s) (worker channel full)",
            pad.mac
        );
    }

    // Drop the DSU slot *before* the hub re-arms the pad: a stale slot must
    // never advertise motion for a pad that is gone.
    finish_session(&motion, || {
        hub_tx.send(HubMsg::SessionEnded(pad.mac.clone())).ok();
        sup_tx
            .send(SupervisorMsg::TornDown {
                mac: pad.mac.clone(),
            })
            .ok();
    });
}

/// Release input without closing the node (deviation 6).
fn teardown_session<S: PadSink>(worker: &mut Worker<S>, mac: &str) {
    if let Err(err) = worker.teardown() {
        warn!("{mac}: teardown failed: {err}");
    }
}

// ---------------------------------------------------------------------------
// Rumble routing
// ---------------------------------------------------------------------------

/// Pad-side force-feedback source: the uinput node owned by the worker's sink.
trait FfPad {
    fn poll_ff(&mut self) -> io::Result<Vec<FfEvent>>;
    fn ff_target(&self) -> (u16, u16);
}

/// Destination for [`RumbleAction`]s: the session's controller, or a test fake.
trait RumbleSink {
    fn deliver(&mut self, action: &RumbleAction) -> io::Result<()>;
}

impl RumbleSink for SwitchController {
    fn deliver(&mut self, action: &RumbleAction) -> io::Result<()> {
        match action {
            RumbleAction::HdPacket(payload) => self.write_vibration(payload),
            // Blocks up to 2 s for the ACK on the GameCube path: called from
            // the session task, never while a pad mutex is held.
            RumbleAction::Preset(preset) => self.play_vibration_preset(*preset),
        }
    }
}

/// Profile of the controller that is actually connected (PID from its info).
fn controller_profile(
    controller: &SwitchController,
) -> &'static crate::profiles::ControllerProfile {
    let pid = controller
        .info
        .as_ref()
        .map(|info| info.product_id)
        .unwrap_or(PRO_CONTROLLER2_PID);
    crate::profiles::profile_or_default(pid)
}

/// Should the rumble pump run now? Advances the absolute 16 ms grid.
///
/// `false` means the caller arrived early and must not pump: this is what keeps
/// a stream of reports from turning into a packet burst.
fn rumble_due(next: &mut Instant, now: Instant) -> bool {
    if now < *next {
        return false;
    }
    *next = now + SUSTAIN_INTERVAL;
    true
}

/// Drain the uinput FF queue and dispatch at most one rumble action.
///
/// Draining is mandatory even when nothing is playing: upload/erase requests
/// are acknowledged through this queue and would otherwise block the
/// application that issued them.
fn pump_rumble<P: FfPad, R: RumbleSink>(
    pad: &mut P,
    engine: &mut RumbleEngine,
    out: &mut R,
    now: Instant,
    last_ff: &mut (u16, u16),
) {
    if let Err(err) = pad.poll_ff() {
        debug!("uinput FF poll failed: {err}");
    }

    // `Played`/`Stopped` change this level, global gain changes do not (the
    // kernel freezes the gained magnitudes of the running effect), so a level
    // comparison is exactly the required set_target trigger.
    let target = pad.ff_target();
    if target != *last_ff {
        *last_ff = target;
        engine.set_target(target.0, target.1, now);
    }

    deliver_action(engine.tick(now), out);
}

/// Ordered teardown (deviation 6): rumble stop, then key/axis release.
///
/// Kept as one function so the order cannot be flipped by accident, and so the
/// order itself is testable without a socket or a uinput node.
fn teardown_with_rumble<S: PadSink, R: RumbleSink>(
    worker: &mut Worker<S>,
    engine: &mut RumbleEngine,
    out: &mut R,
    mac: &str,
    now: Instant,
) {
    stop_rumble(engine, out, now);
    teardown_session(worker, mac);
}

/// Stop rumble: one silent target, one tick (deviation 6, before releasing keys).
fn stop_rumble<R: RumbleSink>(engine: &mut RumbleEngine, out: &mut R, now: Instant) {
    engine.set_target(0, 0, now);
    deliver_action(engine.tick(now), out);
}

/// Send one action, swallowing (but logging) write errors: a pad that refuses a
/// vibration packet still streams input, so the session must survive.
fn deliver_action<R: RumbleSink>(action: Option<RumbleAction>, out: &mut R) {
    if let Some(action) = action {
        if let Err(err) = out.deliver(&action) {
            debug!("rumble write failed (session continues): {err}");
        }
    }
}

fn lock_gamepad(
    gamepad: &Arc<StdMutex<VirtualGamepad>>,
) -> std::sync::MutexGuard<'_, VirtualGamepad> {
    gamepad
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sink for split mode: one node per pad, full dual-stick mapping.
#[derive(Clone)]
struct SharedPadSink {
    gamepad: Arc<StdMutex<VirtualGamepad>>,
}

impl FfPad for SharedPadSink {
    fn poll_ff(&mut self) -> io::Result<Vec<FfEvent>> {
        let mut gamepad = lock_gamepad(&self.gamepad);
        gamepad.poll_ff()
    }

    fn ff_target(&self) -> (u16, u16) {
        lock_gamepad(&self.gamepad).ff_target()
    }
}

impl PadSink for SharedPadSink {
    fn update(
        &mut self,
        buttons: u32,
        sticks: (f32, f32, f32, f32),
        triggers: (u8, u8),
    ) -> io::Result<()> {
        let mut gamepad = lock_gamepad(&self.gamepad);
        PadSink::update(&mut *gamepad, buttons, sticks, triggers)
    }

    fn release_all(&mut self) -> io::Result<()> {
        let mut gamepad = lock_gamepad(&self.gamepad);
        PadSink::release_all(&mut *gamepad)
    }

    fn set_player(&mut self, player: u8) -> io::Result<()> {
        let mut gamepad = lock_gamepad(&self.gamepad);
        PadSink::set_player(&mut *gamepad, player)
    }
}

impl FfPad for CombinedSide {
    // The combined node has one kernel FF queue shared by both halves; either
    // worker may drain it, and `ff_target()` afterwards reports the same level
    // to both, which the level comparison in `pump_rumble` relies on.
    fn poll_ff(&mut self) -> io::Result<Vec<FfEvent>> {
        CombinedSide::poll_ff(self)
    }

    fn ff_target(&self) -> (u16, u16) {
        CombinedSide::ff_target(self)
    }
}

// ---------------------------------------------------------------------------
// DSU motion (ADR-3)
// ---------------------------------------------------------------------------

/// DSU port from the protocol (`DSU_PORT`, dsu.py).
const DSU_PORT: u16 = 26760;

/// The DSU motion sink: the real bridge, or a test fake.
///
/// Local to the daemon so the session loop can be tested without sockets; the
/// bridge itself is unchanged.
trait MotionSink: Send + Sync {
    fn session_started(&self, player: u8, mac: &str, battery_mv: u16) -> u8;
    fn update(&self, slot: u8, report: &InputReport, cal: &CalibratedInput);
    fn session_ended(&self, slot: u8);
}

impl MotionSink for DsuBridge {
    fn session_started(&self, player: u8, mac: &str, battery_mv: u16) -> u8 {
        DsuBridge::session_started(self, player, mac, battery_mv)
    }

    fn update(&self, slot: u8, report: &InputReport, cal: &CalibratedInput) {
        DsuBridge::update(self, slot, report, cal)
    }

    fn session_ended(&self, slot: u8) {
        DsuBridge::session_ended(self, slot)
    }
}

/// No-op sink for the (unexpected) case where even the no-op bridge cannot be
/// constructed: the daemon still runs without motion.
struct NullMotion;

impl MotionSink for NullMotion {
    fn session_started(&self, player: u8, _mac: &str, _battery_mv: u16) -> u8 {
        crate::dsu::slot_for_player(player)
    }

    fn update(&self, _slot: u8, _report: &InputReport, _cal: &CalibratedInput) {}

    fn session_ended(&self, _slot: u8) {}
}

type SharedMotion = Arc<dyn MotionSink>;

/// Start the DSU bridge once; motion failure is logged, never fatal.
fn start_motion() -> SharedMotion {
    let bind = SocketAddr::from(([127, 0, 0, 1], DSU_PORT));
    match DsuBridge::start(bind) {
        Ok(bridge) => match bridge.local_addr() {
            Ok(addr) => {
                info!("DSU motion listening on {addr}");
                Arc::new(bridge)
            }
            Err(err) => {
                warn!("DSU motion unavailable ({err}); continuing without motion");
                Arc::new(bridge)
            }
        },
        Err(err) => {
            warn!("DSU motion unavailable ({err}); continuing without motion");
            Arc::new(NullMotion)
        }
    }
}

// ---------------------------------------------------------------------------
// IMU evdev node
// ---------------------------------------------------------------------------

/// Everything the worker needs from the IMU node (real node, or a test fake).
///
/// `update` takes the raw `i16` samples: the node carries the LSB-per-unit
/// scale in its axis resolution, so nothing is converted here.
trait MotionOut: Send {
    fn update(&mut self, accel_raw: (i16, i16, i16), gyro_raw: (i16, i16, i16)) -> io::Result<()>;
}

impl MotionOut for MotionNode {
    fn update(&mut self, accel_raw: (i16, i16, i16), gyro_raw: (i16, i16, i16)) -> io::Result<()> {
        MotionNode::update(self, accel_raw, gyro_raw)
    }
}

/// Creates the per-worker IMU node (production: `/dev/uinput`).
trait MotionNodeFactory: Send {
    fn create(&self, name: &str, mac: &str, product_id: u16) -> io::Result<Box<dyn MotionOut>>;
}

/// Production factory: a real companion evdev node.
struct RealMotionFactory;

impl MotionNodeFactory for RealMotionFactory {
    fn create(&self, name: &str, mac: &str, product_id: u16) -> io::Result<Box<dyn MotionOut>> {
        // `MotionNode::new` appends " IMU" to the gamepad name and tags the node
        // with the controller MAC; the PID is the controller's (bridge.py:556).
        Ok(Box::new(MotionNode::new(name, mac, product_id)?))
    }
}

/// Create the node once per worker; a failure is warned once and disables
/// motion for that pad without affecting input (same rule as the DSU bridge).
fn create_imu_node(factory: &dyn MotionNodeFactory, pad: &PadSpec) -> Option<Box<dyn MotionOut>> {
    match factory.create(&pad.name, &pad.mac, pad.product_id) {
        Ok(node) => Some(node),
        Err(err) => {
            warn!(
                "{}: IMU node unavailable, continuing without motion: {err}",
                pad.mac
            );
            None
        }
    }
}

/// Publish one raw IMU sample. A failed write is a debug log and is not
/// retried specially; it must never end the session.
fn publish_imu<N: MotionOut + ?Sized>(node: Option<&mut N>, report: &InputReport) {
    let Some(node) = node else {
        return;
    };
    if let Err(err) = node.update(report.accel, report.gyro) {
        debug!("IMU node update failed (session continues): {err}");
    }
}

/// One session's DSU slot.
struct MotionSession<'a> {
    sink: &'a dyn MotionSink,
    slot: u8,
}

impl<'a> MotionSession<'a> {
    fn start(sink: &'a dyn MotionSink, player: u8, mac: &str, battery_mv: u16) -> Self {
        Self {
            sink,
            slot: sink.session_started(player, mac, battery_mv),
        }
    }

    /// Publish one frame. Fire-and-forget: motion is optional and must never
    /// block or fail the input session.
    fn update(&self, report: &InputReport, cal: &CalibratedInput) {
        self.sink.update(self.slot, report, cal);
    }

    fn slot(&self) -> u8 {
        self.slot
    }
}

/// End the slot, then let the caller re-arm the pad. Kept as one function so
/// the order (DSU first, hub second) cannot be flipped by accident.
fn finish_session<F: FnOnce()>(motion: &MotionSession<'_>, after_motion_end: F) {
    motion.sink.session_ended(motion.slot);
    after_motion_end();
}

// ---------------------------------------------------------------------------
// Single instance guard
// ---------------------------------------------------------------------------

/// Exclusive lock file held for the process lifetime.
///
/// The file is removed on drop, but only because the guard owns the lock:
/// `flock` is released implicitly when the descriptor closes.
#[derive(Debug)]
struct InstanceLock {
    path: PathBuf,
    // Kept open for the lifetime of the guard; closing it releases the lock.
    _file: File,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // We still hold the lock here, so removing the path cannot steal a lock
        // from another process (that process would have failed to acquire).
        let _ = fs::remove_file(&self.path);
    }
}

/// `$XDG_RUNTIME_DIR/switch2d.lock`, falling back to `~/.run/switch2d.lock`.
fn instance_lock_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir.join("switch2d.lock");
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".run/switch2d.lock")
}

/// Take an exclusive non-blocking `flock` on `path` and write our pid into it.
///
/// On Linux `flock` locks belong to the open file description, so a second
/// `open`+`flock` in the *same* process also conflicts - which is what makes
/// this testable without spawning a helper.
fn acquire_instance_lock(path: &Path) -> io::Result<InstanceLock> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    // std's exclusive advisory lock, so this needs no libc and no unsafe:
    // the unsafe in this project lives only in the uinput/L2CAP modules.
    if let Err(err) = file.try_lock() {
        let already_held = match &err {
            fs::TryLockError::WouldBlock => true,
            fs::TryLockError::Error(io_err) => io_err.kind() == io::ErrorKind::WouldBlock,
        };
        if already_held {
            let pid = read_lock_pid(&file);
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                match pid {
                    Some(pid) => {
                        format!("another switch2d is already running (pid {pid}); stopping")
                    }
                    None => "another switch2d is already running; stopping".to_string(),
                },
            ));
        }
        return Err(match err {
            fs::TryLockError::WouldBlock => {
                io::Error::new(io::ErrorKind::WouldBlock, "lock is held")
            }
            fs::TryLockError::Error(io_err) => io_err,
        });
    }

    // Record the owner pid so a conflicting instance can name it in the log.
    file.set_len(0)?;
    let mut writer = &file;
    writeln!(writer, "{}", std::process::id())?;
    writer.flush()?;

    Ok(InstanceLock {
        path: path.to_path_buf(),
        _file: file,
    })
}

fn read_lock_pid(file: &File) -> Option<u32> {
    use std::io::{Read, Seek, SeekFrom};
    let mut reader = file;
    reader.seek(SeekFrom::Start(0)).ok()?;
    let mut text = String::new();
    reader.read_to_string(&mut text).ok()?;
    text.trim().parse::<u32>().ok()
}

// ---------------------------------------------------------------------------
// Player identity (ADR-4)
// ---------------------------------------------------------------------------

/// Resolves the effective player per pad and, in combined mode, the one number
/// the pair shares.
///
/// `led-players.json` is re-read on every call from the config directory: the
/// GUI tool rewrites it while the daemon runs, so nothing may be cached here
/// (architecture ADR-4, legacy `bridge.py:763-780`).
#[derive(Debug, Clone)]
struct PlayerPlanner {
    pads: Vec<PadSpec>,
    mode: DaemonMode,
    path: PathBuf,
}

impl PlayerPlanner {
    fn new(pads: Vec<PadSpec>, mode: DaemonMode) -> Self {
        let path = DaemonConfig::config_path()
            .parent()
            .map(path_in)
            .unwrap_or_else(|| path_in(Path::new(".")));
        Self::with_path(pads, mode, path)
    }

    /// Explicit path (tests, and a future config-dir override).
    fn with_path(pads: Vec<PadSpec>, mode: DaemonMode, path: PathBuf) -> Self {
        Self { pads, mode, path }
    }

    /// Fresh read of the override file plus the per-pad plan.
    fn read_plan(&self) -> HashMap<String, u8> {
        self.plan_with(&LedPlayers::load(&self.path))
    }

    /// Plan for an already-parsed override map (pure).
    ///
    /// Combined mode gives every configured pad the same number: the lowest
    /// effective player of the pair (one player, one LED pattern). Split mode
    /// keeps each pad's own effective player.
    fn plan_with(&self, overrides: &LedPlayers) -> HashMap<String, u8> {
        match self.mode {
            DaemonMode::Combined => {
                let pair = combined_player(
                    self.pads
                        .iter()
                        .map(|pad| overrides.resolve(&pad.mac, pad.player)),
                );
                self.pads
                    .iter()
                    .map(|pad| (pad.mac.clone(), pair))
                    .collect()
            }
            DaemonMode::Split => self
                .pads
                .iter()
                .map(|pad| (pad.mac.clone(), overrides.resolve(&pad.mac, pad.player)))
                .collect(),
        }
    }

    /// Number for one pad from a fresh read (used at session start).
    fn player_for(&self, mac: &str) -> u8 {
        let key = mac.to_ascii_uppercase();
        if let Some(player) = self.read_plan().get(&key) {
            return *player;
        }
        self.pads
            .iter()
            .find(|pad| pad.mac == key)
            .map(|pad| pad.player)
            .unwrap_or(1)
    }
}

/// Combined-mode player: the minimum effective player of the pair, or `1` when
/// nothing is configured (legacy pair semantics).
fn combined_player(effective: impl IntoIterator<Item = u8>) -> u8 {
    effective.into_iter().min().unwrap_or(1)
}

/// Applies the numeric player LED pattern (the controller, or a test fake).
trait LedOutput {
    fn apply_player_leds(&mut self, player: u8) -> io::Result<()>;
}

impl LedOutput for SwitchController {
    fn apply_player_leds(&mut self, player: u8) -> io::Result<()> {
        self.set_player_leds(player)
    }
}

/// Apply the LED pattern only when the number changed.
///
/// Mirrors the legacy daemon's `_led_player` check: re-sending an unchanged
/// pattern makes the controller flash, so the comparison is load-bearing.
/// A failed write is logged and retried on the next tick; LEDs are cosmetic and
/// must never end a session.
fn sync_player_led<L: LedOutput>(out: &mut L, mac: &str, wanted: u8, applied: &mut u8) -> bool {
    if wanted == *applied {
        return false;
    }
    match out.apply_player_leds(wanted) {
        Ok(()) => {
            *applied = wanted;
            info!("{mac}: player LED set to Player {wanted}");
            true
        }
        Err(err) => {
            warn!("{mac}: player LED update failed (session continues): {err}");
            false
        }
    }
}

/// Follow the supervisor's latest plan (written on every publish).
fn apply_planned_led<L: LedOutput>(
    out: &mut L,
    mac: &str,
    plan: &Arc<StdMutex<HashMap<String, u8>>>,
    applied: &mut u8,
) {
    let wanted = plan
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&mac.to_ascii_uppercase())
        .copied();
    if let Some(wanted) = wanted {
        sync_player_led(out, mac, wanted, applied);
    }
}

/// Re-read the override file, store the plan for the workers and mirror the
/// effective player into the published status document.
fn refresh_led_plan(
    planner: &PlayerPlanner,
    plan: &Arc<StdMutex<HashMap<String, u8>>>,
    status: &mut DaemonStatus,
) {
    let fresh = planner.read_plan();
    for (mac, player) in &fresh {
        if let Some(entry) = find_controller(status, mac) {
            entry.effective_player = if *player == entry.player {
                None
            } else {
                Some(*player)
            };
        }
    }
    let mut shared = plan.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *shared = fresh;
}

// ---------------------------------------------------------------------------
// Supervisor / status plane
// ---------------------------------------------------------------------------

/// Events the supervisor folds into `state.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SupervisorMsg {
    /// A worker's session became active.
    Activated { mac: String },
    /// A session ended (link loss, idle sleep or teardown).
    TornDown { mac: String },
    /// Live battery value from a session.
    Battery { mac: String, mv: u16 },
    /// Discovery is running (or not).
    Scanning(bool),
    /// Latest hub error (`None` clears it).
    HubError(Option<String>),
}

/// The only writer of `state.json`: publishes on activate/teardown and every
/// 1.5 s (legacy behaviour).
async fn supervisor_loop(
    mut rx: UnboundedReceiver<SupervisorMsg>,
    publisher: StatusPublisher,
    pads: Vec<PadSpec>,
    planner: Arc<PlayerPlanner>,
    led_plan: Arc<StdMutex<HashMap<String, u8>>>,
) {
    let mut status = DaemonStatus::new();
    status.hub_alive = true;
    status.set_service(ServiceState::Running);
    for pad in &pads {
        status.upsert_controller(initial_controller_status(pad));
    }
    refresh_led_plan(&planner, &led_plan, &mut status);
    publish_status(&publisher, &mut status);

    loop {
        let publish_now = tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => apply_supervisor_msg(&mut status, msg),
                None => break,
            },
            _ = sleep(STATUS_INTERVAL) => true,
        };
        if publish_now {
            // Never cached across ticks: the GUI tool rewrites the file while
            // the daemon runs (ADR-4).
            refresh_led_plan(&planner, &led_plan, &mut status);
            publish_status(&publisher, &mut status);
        }
    }
}

/// Apply one message; returns `true` when the change must be published at once.
fn apply_supervisor_msg(status: &mut DaemonStatus, msg: SupervisorMsg) -> bool {
    match msg {
        SupervisorMsg::Activated { mac } => {
            set_connected(status, &mac, true);
            true
        }
        SupervisorMsg::TornDown { mac } => {
            set_connected(status, &mac, false);
            true
        }
        SupervisorMsg::Battery { mac, mv } => {
            if let Some(entry) = find_controller(status, &mac) {
                entry.battery_mv = Some(mv);
                entry.battery_pct = battery_pct_from_mv(mv);
            }
            false
        }
        SupervisorMsg::Scanning(scanning) => {
            status.hub_scanning = scanning;
            status.hub_alive = true;
            false
        }
        SupervisorMsg::HubError(err) => {
            let failed = err.is_some();
            status.hub_error = err;
            status.set_service(if failed {
                ServiceState::Error
            } else {
                ServiceState::Running
            });
            true
        }
    }
}

fn find_controller<'a>(
    status: &'a mut DaemonStatus,
    mac: &str,
) -> Option<&'a mut ControllerStatus> {
    status
        .controllers
        .iter_mut()
        .find(|entry| entry.mac.eq_ignore_ascii_case(mac))
}

fn set_connected(status: &mut DaemonStatus, mac: &str, connected: bool) {
    if let Some(entry) = find_controller(status, mac) {
        entry.connected = connected;
        if !connected {
            entry.battery_mv = None;
            entry.battery_pct = None;
        }
    }
}

fn initial_controller_status(pad: &PadSpec) -> ControllerStatus {
    ControllerStatus {
        mac: pad.mac.clone(),
        player: pad.player,
        effective_player: None,
        name: pad.name.clone(),
        bonded: pad.bonded,
        connected: false,
        battery_pct: None,
        battery_mv: None,
    }
}

/// Refresh timestamps/summary text and write the atomic state file.
fn publish_status(publisher: &StatusPublisher, status: &mut DaemonStatus) {
    refresh_summary(status);
    status.touch();
    if let Err(err) = publisher.publish(status) {
        debug!("state publish failed: {err}");
    }
}

fn refresh_summary(status: &mut DaemonStatus) {
    let headline = status_headline(status);
    let detail = status_detail(status);
    status.headline = headline;
    status.detail = detail;
}

/// Legacy headline precedence (analysis §7.2).
fn status_headline(status: &DaemonStatus) -> String {
    if status.service == ServiceState::Stopping {
        return "Stopping".to_string();
    }
    if status.hub_error.is_some() {
        return "Needs attention".to_string();
    }
    if status.controllers.is_empty() {
        return "Set up".to_string();
    }
    let connected = status.connected_count();
    if connected > 0 {
        return format!("{connected} connected");
    }
    if status.hub_scanning {
        return "Scanning".to_string();
    }
    "Ready".to_string()
}

/// Legacy detail precedence (analysis §7.2).
fn status_detail(status: &DaemonStatus) -> String {
    if let Some(err) = &status.hub_error {
        return err.chars().take(120).collect();
    }
    if status.controllers.is_empty() {
        return "Add a controller once with Sync.".to_string();
    }
    let connected: Vec<String> = status
        .controllers
        .iter()
        .filter(|entry| entry.connected)
        .map(|entry| format!("P{} {}", entry.player, entry.name))
        .collect();
    if !connected.is_empty() {
        return format!("{} - ready in Steam and emulators", connected.join(", "));
    }
    "Press a button or hold Sync on a saved controller.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    use crate::bluetooth::scan::{AdvertMode, DeviceAdvert};
    use crate::controller::CalibratedInput;
    use crate::hub::Dialer;
    use crate::protocol::advert::{host_field_from_adapter, AdvertInfo, HostBinding};
    use crate::protocol::constants::{button_mask, NINTENDO_VENDOR_ID, PRO_CONTROLLER2_PID};

    const PAD: &str = "AA:BB:CC:DD:EE:01";
    const ADAPTER: &str = "AA:BB:CC:DD:EE:FF";

    // --- Hub fakes -------------------------------------------------------

    /// Always reports the same live advert, so a re-armed pad is dialable again.
    struct FakeSource(Vec<DeviceAdvert>);

    impl AdvertSource for FakeSource {
        fn devices(&mut self) -> io::Result<Vec<DeviceAdvert>> {
            Ok(self.0.clone())
        }
    }

    struct FakeControl;

    impl ScanControl for FakeControl {
        fn prepare_global(&self) -> io::Result<()> {
            Ok(())
        }

        fn start_le_scan(&self) -> io::Result<()> {
            Ok(())
        }

        fn stop_le_scan(&self, _force: bool) -> io::Result<()> {
            Ok(())
        }

        fn remove_bluez_device(&self, _mac: &str) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeDialer {
        dialed: Arc<StdMutex<Vec<String>>>,
    }

    impl Dialer for FakeDialer {
        fn dial(&mut self, mac: &str) -> io::Result<()> {
            self.dialed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(mac.to_ascii_uppercase());
            Ok(())
        }
    }

    fn live_pad() -> DeviceAdvert {
        DeviceAdvert {
            address: PAD.to_string(),
            connected: false,
            rssi: Some(-45),
            advert: Some(AdvertInfo {
                address: PAD.to_string(),
                vid: NINTENDO_VENDOR_ID,
                pid: PRO_CONTROLLER2_PID,
                host: HostBinding::Host(host_field_from_adapter(ADAPTER).expect("adapter MAC")),
            }),
        }
    }

    fn fake_hub() -> Hub<FakeSource, FakeControl, FakeDialer> {
        let mut hub = Hub::new(
            FakeSource(vec![live_pad()]),
            FakeControl,
            FakeDialer::default(),
            HubConfig::default(),
        );
        hub.register_known(PAD);
        hub.set_adapter_mac(ADAPTER);
        hub
    }

    fn hub_poll(hub: &mut Hub<FakeSource, FakeControl, FakeDialer>, now: Instant) -> HubOutcome {
        hub.poll_once(now).expect("poll succeeds")
    }

    // --- Sink fake -------------------------------------------------------

    type CallLog = Arc<StdMutex<Vec<&'static str>>>;

    #[derive(Default)]
    struct FakeSink {
        updates: usize,
        buttons: u32,
        sticks: (f32, f32, f32, f32),
        releases: usize,
        log: Option<CallLog>,
    }

    impl PadSink for FakeSink {
        fn update(
            &mut self,
            buttons: u32,
            sticks: (f32, f32, f32, f32),
            _triggers: (u8, u8),
        ) -> io::Result<()> {
            self.updates += 1;
            self.buttons = buttons;
            self.sticks = sticks;
            Ok(())
        }

        fn release_all(&mut self) -> io::Result<()> {
            self.releases += 1;
            self.buttons = 0;
            self.sticks = (0.0, 0.0, 0.0, 0.0);
            if let Some(log) = &self.log {
                log.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push("release");
            }
            Ok(())
        }

        fn set_player(&mut self, _player: u8) -> io::Result<()> {
            Ok(())
        }
    }

    fn pad_spec() -> PadSpec {
        PadSpec {
            mac: PAD.to_string(),
            player: 2,
            name: "Joy-Con 2 (Left)".to_string(),
            bonded: true,
            product_id: 0x2067,
            is_left: true,
        }
    }

    fn report(buttons: u32) -> InputReport {
        InputReport {
            timestamp: 0,
            buttons,
            left_stick_raw: (2048, 2048),
            right_stick_raw: (2048, 2048),
            battery_mv: 4000,
            accel: (0, 0, 0),
            gyro: (0, 0, 0),
            left_trigger_raw: 0,
            right_trigger_raw: 0,
        }
    }

    fn calibrated() -> CalibratedInput {
        CalibratedInput {
            left: (0.75, -0.5),
            right: (0.0, 0.0),
            lt: 0,
            rt: 0,
        }
    }

    // --- Temp dir (tempfile is deliberately not a dependency) ------------

    struct TempDir {
        root: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "switch2d-daemon-{}-{}-{}",
                std::process::id(),
                seq,
                tag
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("create temp dir");
            Self { root }
        }

        fn state_path(&self) -> PathBuf {
            self.root.join("state.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn read_state(path: &PathBuf) -> DaemonStatus {
        let text = fs::read_to_string(path).expect("state.json exists");
        serde_json::from_str(&text).expect("state.json parses")
    }

    // --- Tests -----------------------------------------------------------

    #[test]
    fn hub_to_worker_handoff_marks_connected_and_disconnected() {
        let mut hub = fake_hub();
        let now = Instant::now();

        let pending = match hub_poll(&mut hub, now) {
            HubOutcome::Pending(pending) => pending,
            other => panic!("expected a pending pad, got {other:?}"),
        };
        assert_eq!(pending.len(), 1, "the live advert selects the pad");
        assert_eq!(pending[0].mode, AdvertMode::Wake);

        let results = hub.dial_pending(&pending);
        assert_eq!(results.len(), 1);
        assert!(results[0].outcome.is_ok());

        // Hand-off stand-in for the per-pad session channel: the session must
        // reach its worker before the hub polls again (sequencing rule 3).
        let (tx, rx) = std::sync::mpsc::channel::<&str>();
        assert!(handoff_session(&mut hub, &results[0], || tx
            .send(PAD)
            .is_ok()));
        assert_eq!(rx.recv().expect("worker received the session"), PAD);
        assert!(hub.is_connected(PAD), "session delivery marks it connected");

        // Link drop / idle sleep: the worker reports the session ended.
        rearm_pad(&mut hub, PAD);
        assert!(!hub.is_connected(PAD), "teardown clears the connected flag");

        // Re-armed: the next live advert dials the pad again.
        let again = hub_poll(&mut hub, now + Duration::from_secs(1));
        match again {
            HubOutcome::Pending(pending) => assert_eq!(pending[0].mac, PAD),
            other => panic!("re-armed pad is not dialable: {other:?}"),
        }

        // A lost worker must not leave the pad stuck as "connected": the
        // handoff re-arms it so a future advert can dial it again.
        let results = hub.dial_pending(&pending);
        assert!(results[0].outcome.is_ok());
        assert!(hub.is_connected(PAD), "hub marks a successful dial");
        assert!(!handoff_session(&mut hub, &results[0], || false));
        assert!(!hub.is_connected(PAD), "failed handoff re-arms the pad");
    }

    #[test]
    fn worker_session_teardown_releases_input() {
        let t0 = Instant::now();
        let mut worker = Worker::new(
            WorkerConfig {
                mac: PAD.to_string(),
                player: 2,
                idle_timeout: Duration::from_secs(300),
            },
            FakeSink::default(),
        );

        // A report as the session loop feeds it (controller-produced calibration).
        worker
            .on_report(&report(button_mask::A | button_mask::ZL), &calibrated(), t0)
            .expect("sink accepts the report");
        assert_ne!(worker.frames(), 0);

        // The daemon's teardown path (deviation 6) releases everything.
        teardown_session(&mut worker, PAD);

        // The worker stays usable: the node is not closed, so the next session
        // reuses it and a second teardown is harmless.
        teardown_session(&mut worker, PAD);
        assert_eq!(worker.frames(), 1, "teardown does not drop the worker");
    }

    #[test]
    fn status_is_published_on_activate_and_teardown() {
        let dir = TempDir::new("status");
        let path = dir.state_path();
        let publisher = StatusPublisher::with_path(path.clone());

        let mut status = DaemonStatus::new();
        status.hub_alive = true;
        status.upsert_controller(initial_controller_status(&pad_spec()));
        publish_status(&publisher, &mut status);

        let initial = read_state(&path);
        assert_eq!(initial.controllers.len(), 1);
        assert!(!initial.controllers[0].connected);
        assert_eq!(initial.pid, std::process::id());

        // Activate: published immediately.
        assert!(apply_supervisor_msg(
            &mut status,
            SupervisorMsg::Activated {
                mac: PAD.to_string()
            }
        ));
        publish_status(&publisher, &mut status);
        let activated = read_state(&path);
        assert!(activated.controllers[0].connected);
        assert_eq!(activated.headline, "1 connected");
        assert_eq!(
            activated.detail,
            "P2 Joy-Con 2 (Left) - ready in Steam and emulators"
        );

        // Battery from the session lands in the same document.
        apply_supervisor_msg(
            &mut status,
            SupervisorMsg::Battery {
                mac: PAD.to_string(),
                mv: 4000,
            },
        );
        publish_status(&publisher, &mut status);
        let with_battery = read_state(&path);
        assert_eq!(with_battery.controllers[0].battery_mv, Some(4000));
        assert_eq!(
            with_battery.controllers[0].battery_pct,
            battery_pct_from_mv(4000)
        );

        // Teardown: published immediately, flag cleared, battery dropped.
        assert!(apply_supervisor_msg(
            &mut status,
            SupervisorMsg::TornDown {
                mac: PAD.to_string()
            }
        ));
        publish_status(&publisher, &mut status);
        let torn_down = read_state(&path);
        assert!(!torn_down.controllers[0].connected);
        assert_eq!(torn_down.controllers[0].battery_mv, None);
        assert!(torn_down.updated_at_unix >= activated.updated_at_unix);
    }

    #[test]
    fn hub_errors_back_off_and_recover() {
        // The supervisor turns a hub error into a published error state and
        // clears it again on recovery (never fatal, architecture rule 2).
        let mut status = DaemonStatus::new();
        status.upsert_controller(initial_controller_status(&pad_spec()));

        assert!(apply_supervisor_msg(
            &mut status,
            SupervisorMsg::HubError(Some("StartDiscovery failed".to_string()))
        ));
        assert_eq!(status.service, ServiceState::Error);
        assert_eq!(status_headline(&status), "Needs attention");

        assert!(apply_supervisor_msg(
            &mut status,
            SupervisorMsg::HubError(None)
        ));
        assert_eq!(status.service, ServiceState::Running);
        assert!(status.hub_error.is_none());
    }

    #[test]
    fn scanning_status_is_updated_without_an_immediate_publish() {
        let mut status = DaemonStatus::new();
        status.upsert_controller(initial_controller_status(&pad_spec()));
        assert!(!apply_supervisor_msg(
            &mut status,
            SupervisorMsg::Scanning(true)
        ));
        assert!(status.hub_scanning);
        assert_eq!(status_headline(&status), "Scanning");
    }

    // --- Rumble ----------------------------------------------------------

    use crate::profiles::profile_for;
    use crate::protocol::constants::JOYCON2_LEFT_PID;
    use crate::protocol::vibration::{Preset, Sample};

    #[derive(Default)]
    struct FakePad {
        target: (u16, u16),
        events: Vec<FfEvent>,
        polls: usize,
    }

    impl PadSink for FakePad {
        fn update(
            &mut self,
            _buttons: u32,
            _sticks: (f32, f32, f32, f32),
            _triggers: (u8, u8),
        ) -> io::Result<()> {
            Ok(())
        }

        fn release_all(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn set_player(&mut self, _player: u8) -> io::Result<()> {
            Ok(())
        }
    }

    impl FfPad for FakePad {
        fn poll_ff(&mut self) -> io::Result<Vec<FfEvent>> {
            self.polls += 1;
            Ok(std::mem::take(&mut self.events))
        }

        fn ff_target(&self) -> (u16, u16) {
            self.target
        }
    }

    #[derive(Default)]
    struct FakeController {
        attempts: usize,
        hd: Vec<Vec<u8>>,
        presets: Vec<Preset>,
        fail: bool,
        log: Option<CallLog>,
    }

    impl RumbleSink for FakeController {
        fn deliver(&mut self, action: &RumbleAction) -> io::Result<()> {
            self.attempts += 1;
            if let Some(log) = &self.log {
                log.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push("rumble");
            }
            if self.fail {
                return Err(io::Error::other("pad refused the vibration packet"));
            }
            match action {
                RumbleAction::HdPacket(payload) => self.hd.push(payload.clone()),
                RumbleAction::Preset(preset) => self.presets.push(*preset),
            }
            Ok(())
        }
    }

    fn joycon_engine() -> RumbleEngine {
        RumbleEngine::new(profile_for(JOYCON2_LEFT_PID).expect("joycon profile"))
    }

    /// Payload sample bytes used by the vibration tests (after the 2-byte header).
    fn sample(payload: &[u8]) -> &[u8] {
        &payload[2..7]
    }

    #[test]
    fn worker_dispatches_hd_packets_on_a_16ms_cadence() {
        let t0 = Instant::now();
        let mut engine = joycon_engine();
        let mut pad = FakePad::default();
        let mut out = FakeController::default();
        let mut last_ff = (0, 0u16);
        let mut next = t0;

        // The application starts a full-strength effect: the kernel level moves.
        pad.target = (u16::MAX, 0);
        pad.events.push(FfEvent::Played { id: 1, gain: 1.0 });

        // Slot 0: due, one packet.
        assert!(rumble_due(&mut next, t0));
        pump_rumble(&mut pad, &mut engine, &mut out, t0, &mut last_ff);
        assert_eq!(out.hd.len(), 1, "first packet starts the rumble");
        assert_eq!(out.hd[0].len(), 17, "Joy-Con uses one 17-byte block");

        // Within the same slot the loop must not pump (no burst).
        assert!(!rumble_due(&mut next, t0 + Duration::from_millis(8)));
        assert!(!rumble_due(&mut next, t0 + Duration::from_millis(15)));

        // Exactly one sustained packet per 16 ms slot, with no reports at all.
        for slot in 1..=4u32 {
            let now = t0 + SUSTAIN_INTERVAL * slot;
            assert!(rumble_due(&mut next, now), "slot {slot} is due");
            pump_rumble(&mut pad, &mut engine, &mut out, now, &mut last_ff);
            assert_eq!(out.hd.len(), slot as usize + 1, "one packet per slot");

            // A redundant pump at the same instant cannot add another packet.
            pump_rumble(&mut pad, &mut engine, &mut out, now, &mut last_ff);
            assert_eq!(out.hd.len(), slot as usize + 1);
        }

        assert!(out.presets.is_empty(), "Joy-Con never uses the preset path");
        assert!(pad.polls >= 5, "every slot drained the FF queue");
    }

    #[test]
    fn worker_sends_one_stop_packet_on_teardown() {
        let t0 = Instant::now();
        let mut engine = joycon_engine();
        let mut pad = FakePad::default();
        let mut out = FakeController::default();
        let mut last_ff = (0, 0u16);

        pad.target = (u16::MAX, 0);
        pump_rumble(&mut pad, &mut engine, &mut out, t0, &mut last_ff);
        assert_eq!(out.hd.len(), 1, "rumble is running");

        let teardown_at = t0 + Duration::from_millis(5);
        stop_rumble(&mut engine, &mut out, teardown_at);
        assert_eq!(out.hd.len(), 2, "exactly one stop packet");
        assert_eq!(sample(&out.hd[1]), &Sample::default().to_u40());

        // A second teardown (or any later tick) sends nothing more.
        stop_rumble(&mut engine, &mut out, teardown_at + Duration::from_secs(1));
        pump_rumble(
            &mut pad,
            &mut engine,
            &mut out,
            teardown_at + Duration::from_secs(2),
            &mut last_ff,
        );
        assert_eq!(out.hd.len(), 2, "stop is emitted once and parks");
    }

    #[test]
    fn rumble_write_error_does_not_end_the_session() {
        let t0 = Instant::now();
        let mut engine = joycon_engine();
        let mut pad = FakePad::default();
        let mut out = FakeController {
            fail: true,
            ..FakeController::default()
        };
        let mut last_ff = (0, 0u16);
        let mut next = t0;

        pad.target = (u16::MAX, 0);
        assert!(rumble_due(&mut next, t0));
        pump_rumble(&mut pad, &mut engine, &mut out, t0, &mut last_ff);
        assert_eq!(out.attempts, 1, "the write was attempted");
        assert!(out.hd.is_empty(), "nothing was recorded");

        // The session keeps pumping on the next slot: the error was swallowed,
        // not propagated, so the input loop is still alive.
        let t16 = t0 + SUSTAIN_INTERVAL;
        assert!(rumble_due(&mut next, t16));
        pump_rumble(&mut pad, &mut engine, &mut out, t16, &mut last_ff);
        assert_eq!(out.attempts, 2, "the pump retried after the failure");
        assert!(out.hd.is_empty());

        // Teardown over a failing write must not panic either.
        stop_rumble(&mut engine, &mut out, t16 + Duration::from_millis(1));
        assert_eq!(out.attempts, 3);
    }

    #[test]
    fn no_rumble_when_the_pad_is_silent() {
        let t0 = Instant::now();
        let mut engine = joycon_engine();
        let mut pad = FakePad::default();
        let mut out = FakeController::default();
        let mut last_ff = (0, 0u16);
        let mut next = t0;

        for slot in 0..3u32 {
            let now = t0 + SUSTAIN_INTERVAL * slot;
            assert!(rumble_due(&mut next, now));
            pump_rumble(&mut pad, &mut engine, &mut out, now, &mut last_ff);
        }
        stop_rumble(&mut engine, &mut out, t0 + Duration::from_secs(1));

        assert!(out.hd.is_empty(), "silent pad: no sustain packets");
        assert!(out.presets.is_empty(), "silent pad: no presets");
        assert_eq!(out.attempts, 0);
        assert!(pad.polls >= 3, "the FF queue is drained even when silent");
    }

    #[test]
    fn preset_pads_cancel_instead_of_sending_a_stop_packet() {
        // The GameCube path has no stop subcommand: teardown cancels the queued
        // clip and must not emit an HD packet.
        let t0 = Instant::now();
        let profile =
            profile_for(crate::protocol::constants::NSO_GAMECUBE_PID).expect("gamecube profile");
        let mut engine = RumbleEngine::new(profile);
        let mut pad = FakePad::default();
        let mut out = FakeController::default();
        let mut last_ff = (0, 0u16);

        pad.target = (u16::MAX, 0);
        pump_rumble(&mut pad, &mut engine, &mut out, t0, &mut last_ff);
        assert_eq!(out.presets, vec![Preset::Strong]);
        assert!(out.hd.is_empty());

        // Release before the next tick cancels any queued clip.
        pad.target = (0, 0);
        pump_rumble(
            &mut pad,
            &mut engine,
            &mut out,
            t0 + Duration::from_millis(10),
            &mut last_ff,
        );
        stop_rumble(&mut engine, &mut out, t0 + Duration::from_millis(20));
        assert_eq!(out.presets, vec![Preset::Strong], "no second clip");
        assert!(out.hd.is_empty(), "never an HD packet on the GC path");
    }

    #[test]
    fn teardown_stops_rumble_before_releasing_input() {
        let t0 = Instant::now();
        let log: CallLog = Arc::new(StdMutex::new(Vec::new()));

        let mut worker = Worker::new(
            WorkerConfig {
                mac: PAD.to_string(),
                player: 2,
                idle_timeout: Duration::from_secs(300),
            },
            FakeSink {
                log: Some(Arc::clone(&log)),
                ..FakeSink::default()
            },
        );
        worker
            .on_report(&report(button_mask::A), &calibrated(), t0)
            .expect("sink accepts the report");

        let mut engine = joycon_engine();
        let mut out = FakeController {
            log: Some(Arc::clone(&log)),
            ..FakeController::default()
        };
        // Rumble is running when the session ends.
        engine.set_target(u16::MAX, 0, t0);
        let _ = engine.tick(t0);
        assert_eq!(out.attempts, 0, "the engine action is still queued");

        teardown_with_rumble(
            &mut worker,
            &mut engine,
            &mut out,
            PAD,
            t0 + Duration::from_millis(5),
        );

        assert_eq!(
            *log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["rumble", "release"],
            "rumble must stop before keys are released (deviation 6)"
        );
        assert_eq!(out.hd.len(), 1, "exactly one stop packet");
        assert_eq!(sample(&out.hd[0]), &Sample::default().to_u40());
    }

    // --- Player identity (ADR-4) ----------------------------------------

    use crate::players::LED_PLAYERS_FILE;

    struct FakeLeds {
        applied: Vec<u8>,
        fail: bool,
    }

    impl LedOutput for FakeLeds {
        fn apply_player_leds(&mut self, player: u8) -> io::Result<()> {
            if self.fail {
                return Err(io::Error::other("LED write refused"));
            }
            self.applied.push(player);
            Ok(())
        }
    }

    fn pads_pair() -> Vec<PadSpec> {
        vec![
            PadSpec {
                mac: PAD.to_string(),
                player: 1,
                name: "Joy-Con 2 (Left)".to_string(),
                bonded: true,
                product_id: 0x2067,
                is_left: true,
            },
            PadSpec {
                mac: "AA:BB:CC:DD:EE:02".to_string(),
                player: 2,
                name: "Joy-Con 2 (Right)".to_string(),
                bonded: true,
                product_id: 0x2066,
                is_left: false,
            },
        ]
    }

    fn write_led_players(dir: &TempDir, json: &str) -> PathBuf {
        let path = dir.root.join(LED_PLAYERS_FILE);
        fs::write(&path, json).expect("write led-players.json");
        path
    }

    #[test]
    fn combined_player_is_the_min_of_effective_players() {
        assert_eq!(combined_player([1u8, 2]), 1, "1 and 2 -> 1");
        assert_eq!(combined_player([2u8, 1]), 1);
        assert_eq!(combined_player([3u8, 1]), 1, "3 and 1 -> 1");
        assert_eq!(combined_player([4u8, 7]), 4);
        assert_eq!(combined_player(Vec::<u8>::new()), 1, "empty -> 1");
        assert_eq!(combined_player([5u8]), 5, "single pad keeps its number");
    }

    #[test]
    fn led_override_changes_only_the_number_not_the_mode() {
        let dir = TempDir::new("led-override");
        let pad_b = "AA:BB:CC:DD:EE:02";
        // Override for the left pad only.
        let path = write_led_players(&dir, &format!(r#"{{"{PAD}": 4}}"#));

        let planner = PlayerPlanner::with_path(pads_pair(), DaemonMode::Split, path.clone());
        let without = planner.plan_with(&LedPlayers::default());
        let with = planner.read_plan();

        assert_eq!(
            planner.mode,
            DaemonMode::Split,
            "override never changes the mode"
        );
        assert_eq!(
            without.keys().collect::<std::collections::BTreeSet<_>>(),
            with.keys().collect::<std::collections::BTreeSet<_>>(),
            "the override does not add or drop pads"
        );
        assert_eq!(with.get(PAD), Some(&4), "override wins for that pad");
        assert_eq!(with.get(pad_b), Some(&2), "the other pad is untouched");

        // Combined mode still reports ONE number for the pair, and it follows
        // the override (min of 4 and 2 -> 2).
        let combined = PlayerPlanner::with_path(pads_pair(), DaemonMode::Combined, path);
        let plan = combined.read_plan();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.get(PAD), Some(&2));
        assert_eq!(plan.get(pad_b), Some(&2));
    }

    #[test]
    fn led_is_reapplied_only_when_the_number_changes() {
        let mut leds = FakeLeds {
            applied: Vec::new(),
            fail: false,
        };
        let mut applied = 1u8;

        assert!(
            !sync_player_led(&mut leds, PAD, 1, &mut applied),
            "same number"
        );
        assert!(
            leds.applied.is_empty(),
            "no command for an unchanged pattern"
        );

        assert!(sync_player_led(&mut leds, PAD, 2, &mut applied), "changed");
        assert_eq!(leds.applied, vec![2]);
        assert_eq!(applied, 2);

        assert!(
            !sync_player_led(&mut leds, PAD, 2, &mut applied),
            "same again"
        );
        assert_eq!(leds.applied, vec![2], "no second command for 2");

        assert!(sync_player_led(&mut leds, PAD, 1, &mut applied));
        assert_eq!(leds.applied, vec![2, 1]);

        // A failed write is not recorded as applied, so the next tick retries.
        let mut failing = FakeLeds {
            applied: Vec::new(),
            fail: true,
        };
        let mut applied = 1u8;
        assert!(!sync_player_led(&mut failing, PAD, 3, &mut applied));
        assert_eq!(applied, 1, "failure keeps the old number for a retry");
    }

    #[test]
    fn split_mode_keeps_per_pad_players() {
        let dir = TempDir::new("split-players");
        let path = write_led_players(&dir, &format!(r#"{{"{PAD}": 3}}"#));
        let planner = PlayerPlanner::with_path(pads_pair(), DaemonMode::Split, path);

        let plan = planner.read_plan();
        assert_eq!(plan.get(PAD), Some(&3), "own effective player");
        assert_eq!(
            plan.get("AA:BB:CC:DD:EE:02"),
            Some(&2),
            "the second pad keeps its configured player"
        );

        // No override file content: configured players are used unchanged.
        let empty_dir = TempDir::new("split-players-empty");
        let empty_path = empty_dir.root.join(LED_PLAYERS_FILE);
        let planner = PlayerPlanner::with_path(pads_pair(), DaemonMode::Split, empty_path);
        let plan = planner.read_plan();
        assert_eq!(plan.get(PAD), Some(&1));
        assert_eq!(plan.get("AA:BB:CC:DD:EE:02"), Some(&2));
    }

    #[test]
    fn session_start_and_tick_read_the_override_file() {
        // The plan the workers follow is the supervisor's fresh read, and the
        // session-start lookup re-reads the file itself (never a cached copy).
        let dir = TempDir::new("led-live");
        let path = write_led_players(&dir, &format!(r#"{{"{PAD}": 6}}"#));
        let planner = PlayerPlanner::with_path(pads_pair(), DaemonMode::Split, path.clone());
        assert_eq!(planner.player_for(PAD), 6);

        // Rewrite while "running": the next lookup sees the new value.
        fs::write(&path, format!(r#"{{"{PAD}": 7}}"#)).expect("rewrite led-players.json");
        assert_eq!(planner.player_for(PAD), 7, "no caching across reads");

        // The shared plan mirrors the same values.
        let plan = Arc::new(StdMutex::new(HashMap::new()));
        let mut status = DaemonStatus::new();
        status.upsert_controller(initial_controller_status(&pads_pair()[0]));
        refresh_led_plan(&planner, &plan, &mut status);
        assert_eq!(
            plan.lock().unwrap().get(PAD).copied(),
            Some(7),
            "workers follow the supervisor's fresh plan"
        );
        assert_eq!(
            status.controllers[0].effective_player,
            Some(7),
            "status mirrors the effective player"
        );

        // Unreadable/missing file falls back to the configured player.
        let missing = PlayerPlanner::with_path(
            pads_pair(),
            DaemonMode::Split,
            dir.root.join("does-not-exist.json"),
        );
        assert_eq!(missing.player_for(PAD), 1);
    }

    // --- DSU motion ------------------------------------------------------

    use crate::dsu::slot_for_player;
    use crate::dsu_bridge::stick_to_dsu;

    #[derive(Debug, Clone, PartialEq)]
    struct MotionRecord {
        slot: u8,
        buttons: u32,
        /// Calibrated left/right X mapped exactly like the real bridge.
        left: u8,
        right: u8,
    }

    #[derive(Default)]
    struct FakeMotion {
        available: bool,
        log: Option<CallLog>,
        starts: Arc<StdMutex<Vec<(String, u8, u8)>>>,
        updates: Arc<StdMutex<Vec<MotionRecord>>>,
        ends: Arc<StdMutex<Vec<u8>>>,
    }

    impl FakeMotion {
        fn available() -> Self {
            Self {
                available: true,
                ..Self::default()
            }
        }
    }

    impl MotionSink for FakeMotion {
        fn session_started(&self, player: u8, mac: &str, _battery_mv: u16) -> u8 {
            let slot = slot_for_player(player);
            self.starts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((mac.to_string(), player, slot));
            slot
        }

        fn update(&self, slot: u8, report: &InputReport, cal: &CalibratedInput) {
            if !self.available {
                // Same early-return as the real bridge with no server.
                return;
            }
            self.updates
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(MotionRecord {
                    slot,
                    buttons: report.buttons,
                    left: stick_to_dsu(cal.left.0),
                    right: stick_to_dsu(cal.right.0),
                });
        }

        fn session_ended(&self, slot: u8) {
            if !self.available {
                return;
            }
            self.ends
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(slot);
            if let Some(log) = &self.log {
                log.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push("dsu_end");
            }
        }
    }

    fn pad(mac: &str, player: u8, is_left: bool) -> PadSpec {
        PadSpec {
            mac: mac.to_string(),
            player,
            name: if is_left {
                "Joy-Con 2 (Left)".to_string()
            } else {
                "Joy-Con 2 (Right)".to_string()
            },
            bonded: true,
            product_id: if is_left { 0x2067 } else { 0x2066 },
            is_left,
        }
    }

    #[test]
    fn dsu_slot_follows_the_combined_pair_number() {
        // Combined pair configured as players 3 and 5: the pair number is 3
        // (min), which is DSU slot 2 for *both* halves - never slot 4 + clamp.
        let dir = TempDir::new("dsu-slot");
        let other = "AA:BB:CC:DD:EE:02";
        let pads = vec![pad(PAD, 3, true), pad(other, 5, false)];
        let planner =
            PlayerPlanner::with_path(pads, DaemonMode::Combined, dir.root.join(LED_PLAYERS_FILE));
        let plan = planner.read_plan();
        assert_eq!(plan.get(PAD), Some(&3), "pair number is the min");
        assert_eq!(plan.get(other), Some(&3));

        let motion = FakeMotion::available();
        for mac in [PAD, other] {
            let effective = *plan.get(mac).expect("pad planned");
            let session = MotionSession::start(&motion, effective, mac, 0);
            assert_eq!(session.slot(), 2, "player 3 -> ADR-3 slot 2");
        }

        let starts = motion
            .starts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(
            starts,
            vec![(PAD.to_string(), 3, 2), (other.to_string(), 3, 2),],
            "both halves start the same player/slot as the LED uses"
        );
    }

    #[test]
    fn dsu_session_ended_is_called_before_rearm() {
        let log: CallLog = Arc::new(StdMutex::new(Vec::new()));
        let motion = FakeMotion {
            log: Some(Arc::clone(&log)),
            ..FakeMotion::available()
        };

        let session = MotionSession::start(&motion, 1, PAD, 0);
        finish_session(&session, || {
            log.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push("rearm")
        });

        assert_eq!(
            *log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["dsu_end", "rearm"],
            "the slot must be dropped before the hub re-arms the pad"
        );
        assert_eq!(
            *motion
                .ends
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![0]
        );
    }

    #[test]
    fn dsu_update_is_skipped_when_motion_is_unavailable() {
        // A no-op bridge (bind failed): start/update/end must all be silent and
        // harmless, and the session still finishes normally.
        let motion = FakeMotion {
            available: false,
            ..FakeMotion::default()
        };
        let session = MotionSession::start(&motion, 1, PAD, 0);
        assert_eq!(session.slot(), 0, "slot is still reported");

        session.update(&report(button_mask::A), &calibrated());
        finish_session(&session, || {});

        assert!(motion
            .updates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(motion
            .ends
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
    }

    #[test]
    fn dsu_update_carries_the_calibrated_sticks() {
        let motion = FakeMotion::available();
        let session = MotionSession::start(&motion, 2, PAD, 4000);

        let mut report = report(button_mask::A);
        // Raw sticks are centred-ish; the bridge must see the calibrated values.
        report.left_stick_raw = (3302, 2048);
        let cal = CalibratedInput {
            left: (-1.0, 0.0),
            right: (0.75, -0.25),
            lt: 10,
            rt: 20,
        };
        session.update(&report, &cal);

        let updates = motion
            .updates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(
            updates,
            vec![MotionRecord {
                slot: 1,
                buttons: button_mask::A,
                left: stick_to_dsu(-1.0),
                right: stick_to_dsu(0.75),
            }]
        );
        assert_ne!(updates[0].left, 128, "not the raw centre value");
        assert_eq!(updates[0].left, 1);
    }

    // --- IMU node --------------------------------------------------------

    type ImuSample = ((i16, i16, i16), (i16, i16, i16));

    struct FakeNode {
        updates: Arc<StdMutex<Vec<ImuSample>>>,
    }

    impl MotionOut for FakeNode {
        fn update(
            &mut self,
            accel_raw: (i16, i16, i16),
            gyro_raw: (i16, i16, i16),
        ) -> io::Result<()> {
            self.updates
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((accel_raw, gyro_raw));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeFactory {
        fail: bool,
        created: Arc<StdMutex<Vec<(String, String, u16)>>>,
        updates: Arc<StdMutex<Vec<ImuSample>>>,
    }

    impl MotionNodeFactory for FakeFactory {
        fn create(&self, name: &str, mac: &str, product_id: u16) -> io::Result<Box<dyn MotionOut>> {
            self.created
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((name.to_string(), mac.to_string(), product_id));
            if self.fail {
                return Err(io::Error::other("no /dev/uinput permission"));
            }
            Ok(Box::new(FakeNode {
                updates: Arc::clone(&self.updates),
            }))
        }
    }

    #[test]
    fn motion_node_is_created_once_per_worker_across_reconnects() {
        let factory = FakeFactory::default();
        let pad = pad_spec();

        // Worker start: exactly one node.
        let mut imu = create_imu_node(&factory, &pad);
        assert!(imu.is_some(), "node created");
        assert_eq!(
            *factory
                .created
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![(
                "Joy-Con 2 (Left)".to_string(),
                PAD.to_string(),
                pad.product_id
            )],
            "named from the config entry, tagged with the MAC, entry PID"
        );

        // Two sessions (reconnect): reports are published, node is not rebuilt.
        for _ in 0..2 {
            publish_imu(imu.as_deref_mut(), &report(button_mask::A));
        }
        assert_eq!(
            factory
                .created
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1,
            "created once per worker, never per session"
        );
        assert_eq!(
            factory
                .updates
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            2,
            "both sessions reached the same node"
        );
    }

    #[test]
    fn motion_update_is_skipped_when_the_node_is_missing() {
        let factory = FakeFactory {
            fail: true,
            ..FakeFactory::default()
        };
        let mut imu = create_imu_node(&factory, &pad_spec());

        // Creation failed -> motion is off, but the session path still runs.
        assert!(imu.is_none(), "failure disables motion, not the worker");
        publish_imu(imu.as_deref_mut(), &report(button_mask::A));
        finish_session_for_test(); // stand-in for "the session completed"

        assert!(factory
            .updates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert_eq!(
            factory
                .created
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1,
            "creation was attempted exactly once (warn once)"
        );
    }

    /// The session-completion path is otherwise covered by the handoff/teardown
    /// tests; the IMU tests only need to show nothing panics without a node.
    fn finish_session_for_test() {}

    #[test]
    fn raw_samples_reach_the_node_unchanged() {
        let factory = FakeFactory::default();
        let mut imu = create_imu_node(&factory, &pad_spec()).expect("node");

        let mut report = report(0);
        report.accel = (123, -456, 789);
        report.gyro = (i16::MIN, i16::MAX, 7);
        publish_imu(Some(imu.as_mut()), &report);

        let updates = factory
            .updates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(
            updates,
            vec![((123, -456, 789), (i16::MIN, i16::MAX, 7))],
            "raw i16 samples, no scaling or clamping"
        );
    }

    // --- Single instance guard -------------------------------------------

    #[test]
    fn lock_excludes_second_instance() {
        let dir = TempDir::new("instance-lock");
        let path = dir.root.join("switch2d.lock");

        let first = acquire_instance_lock(&path).expect("first acquire");
        assert!(path.is_file(), "the lock file exists while held");
        let written = fs::read_to_string(&path).expect("pid written");
        assert_eq!(
            written.trim(),
            std::process::id().to_string(),
            "the owner's pid is recorded for the conflict message"
        );

        // flock is per open file description on Linux, so a second open in the
        // same process conflicts exactly like another daemon would.
        let err = acquire_instance_lock(&path).expect_err("second acquire must fail");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(
            err.to_string()
                .contains("another switch2d is already running (pid"),
            "conflict message: {err}"
        );
        assert!(
            err.to_string().contains(&std::process::id().to_string()),
            "names the owner pid: {err}"
        );

        // Releasing the guard releases the lock and removes the file.
        drop(first);
        assert!(!path.exists(), "owner removes the lock file on drop");

        let third = acquire_instance_lock(&path).expect("lock released on drop");
        drop(third);
    }
}
