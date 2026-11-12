//! Scan/connect scheduler for the daemon.
//!
//! The hub is deliberately synchronous and clock-injected: the daemon (task 09)
//! owns all timing and drives [`Hub::poll_once`] / [`Hub::dial_pending`]. There
//! are no threads, no sleeps and no tokio here, so the whole scheduler is
//! unit-testable with fakes.
//!
//! # Deviations from legacy `_scan_loop` (documented per instruction)
//!
//! Legacy started and stopped the bleak scanner every cycle (500 ms collection
//! window, 100 ms settle) because bleak is window-based. We read BlueZ's live
//! cached object table through `GetManagedObjects` (ADR-6): there is no window,
//! no settle delay and no start/stop per cycle. Discovery is started once per
//! hub lifetime and the cycle sleep is uniform (see [`HubConfig`]).
//!
//! # Connected-state model
//!
//! Connection state is **explicit**: only [`Hub::mark_connected`],
//! [`Hub::mark_disconnected`], [`Hub::unregister_known`], a successful
//! [`Hub::dial_pending`] and the re-arm inside [`Hub::register_known`] change
//! it. Advertisements never do: BlueZ keeps `ManufacturerData`/`RSSI` for a
//! while after a link is up, so treating "still advertising" as "disconnected"
//! would re-open scanning while every pad is connected (CPU/battery cost plus
//! `StopDiscovery` churn).
//!
//! The early-out therefore means "every known pad is connected" exactly like
//! legacy `disconnected == []`, and it must never trigger just because *some*
//! pad is connected (HANDOVER Bug 1: that locked out player 2). The daemon
//! calls [`Hub::register_known`] when a pad becomes expected again, which
//! re-arms it as expected-and-disconnected.
//!
//! # Timed fallback (advert-source reliability)
//!
//! Adverts stay authoritative, but the BlueZ backend is not always useful: an
//! install-report machine kept `Device1` objects with `rssi = None` and no
//! manufacturer data, which made an advert-only hub mute and pads never
//! reconnected. With [`HubConfig::fallback_after`] set, a known-but-disconnected
//! pad that has had no acceptable advert for that long is appended to the
//! dial batch anyway (advert-driven candidates keep priority). A failed attempt
//! suppresses that pad for [`HubConfig::fallback_retry`]. `fallback_after: None`
//! (the default, and what the 06B/06C tests pin) keeps the exact advert-only
//! semantics.
//!
//! # Connect serialization
//!
//! [`Hub::dial_pending`] is the only code path that calls [`Dialer::dial`] and it
//! holds an internal lock for the whole batch. Linux/hardware can only process
//! one outgoing LE create-connection at a time; a second concurrent attempt
//! fails with `EBUSY` (os error 16). This lock plus the single-dialer design is
//! why the hub replaces the old per-controller worker loops.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::bluetooth::scan::{AdvertMode, AdvertSource, PendingConnect, ScanControl, SeenAdverts};
use crate::protocol::advert::{AcceptPolicy, WAKE_TTL};

/// Performs the actual LE connection for one MAC.
pub trait Dialer {
    fn dial(&mut self, mac: &str) -> io::Result<()>;
}

/// Timing knobs the daemon applies between cycles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubConfig {
    /// Sleep after a cycle that produced no pending pads.
    pub cycle_sleep: Duration,
    /// Sleep when every known pad is connected (legacy 1.5 s).
    pub all_connected_sleep: Duration,
    /// Pause before dialing the pending batch (legacy 80 ms).
    pub dial_pause: Duration,
    /// Idle cycles before the seen table is cleared / discovery re-armed.
    pub idle_restart_cycles: u32,
    /// Timed-fallback window: dial a known-but-disconnected pad even without an
    /// acceptable advert once nothing was recorded for it for this long.
    ///
    /// `None` disables the behaviour entirely (advert-only semantics, exactly
    /// what the 06B/06C tests pin with the default).
    pub fallback_after: Option<Duration>,
    /// After a dial attempt, that MAC is not a fallback candidate again until
    /// this long has passed (keeps a blind pad from being dialled every cycle).
    pub fallback_retry: Duration,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            cycle_sleep: Duration::from_millis(250),
            all_connected_sleep: Duration::from_millis(1500),
            dial_pause: Duration::from_millis(80),
            idle_restart_cycles: 80,
            // Off by default: the advert-driven semantics are the contract the
            // hub tests pin. The daemon opts in explicitly.
            fallback_after: None,
            fallback_retry: Duration::from_secs(3),
        }
    }
}

/// Result of one scheduler cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubOutcome {
    /// Every known pad is connected: no source poll, no discovery, no idle tick.
    AllConnected,
    /// Nothing eligible this cycle.
    Idle,
    /// Pads to dial, freshest first.
    Pending(Vec<PendingConnect>),
}

/// Outcome of dialing one pending MAC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialResult {
    pub mac: String,
    pub outcome: Result<(), String>,
}

/// Scan/connect scheduler owning the seen table and discovery lifecycle.
pub struct Hub<S, C, D>
where
    S: AdvertSource,
    C: ScanControl,
    D: Dialer,
{
    source: S,
    control: C,
    dialer: D,
    config: HubConfig,
    seen: SeenAdverts,
    known: HashSet<String>,
    connected: HashSet<String>,
    adapter_mac: String,
    discovery_started: bool,
    idle_cycles: u32,
    warned_missing_adapter: bool,
    connect_lock: Mutex<()>,
    /// Last acceptable advert recorded per MAC (fallback clock).
    recorded_at: HashMap<String, Instant>,
    /// Last dial attempt per MAC (fallback retry window).
    attempted_at: HashMap<String, Instant>,
    /// Known MACs already reported as seen-without-advert (INFO once each).
    warned_blind: HashSet<String>,
}

impl<S, C, D> Hub<S, C, D>
where
    S: AdvertSource,
    C: ScanControl,
    D: Dialer,
{
    pub fn new(source: S, control: C, dialer: D, config: HubConfig) -> Self {
        Self {
            source,
            control,
            dialer,
            config,
            seen: SeenAdverts::new(),
            known: HashSet::new(),
            connected: HashSet::new(),
            adapter_mac: String::new(),
            discovery_started: false,
            idle_cycles: 0,
            warned_missing_adapter: false,
            connect_lock: Mutex::new(()),
            recorded_at: HashMap::new(),
            attempted_at: HashMap::new(),
            warned_blind: HashSet::new(),
        }
    }

    /// Register a pad the hub is allowed to wake.
    ///
    /// The MAC is stored uppercase and re-armed as expected-and-disconnected,
    /// so the daemon can call this again after a session drops.
    pub fn register_known(&mut self, mac: &str) {
        let mac = mac.to_ascii_uppercase();
        self.connected.remove(&mac);
        // A (re)armed pad starts a fresh fallback window: the clock is seeded
        // with "now" on the next poll and any earlier retry suppression is
        // dropped, because this is a new session cycle.
        self.recorded_at.remove(&mac);
        self.attempted_at.remove(&mac);
        self.known.insert(mac);
    }

    /// Set the adapter MAC used to vet `Host(_)` advert bindings.
    ///
    /// An empty value means unknown: `Host(_)` adverts are rejected (we cannot
    /// tell whether they belong to us), `Pairing`/`Unknown` are accepted.
    pub fn set_adapter_mac(&mut self, mac: &str) {
        self.adapter_mac = mac.to_string();
    }

    /// Mark a pad as connected (daemon: session activated).
    pub fn mark_connected(&mut self, mac: &str) {
        self.connected.insert(mac.to_ascii_uppercase());
    }

    /// Mark a pad as disconnected again (daemon: session torn down). The pad
    /// must also be registered as known for scanning to resume for it.
    pub fn mark_disconnected(&mut self, mac: &str) {
        self.connected.remove(&mac.to_ascii_uppercase());
    }

    pub fn is_connected(&self, mac: &str) -> bool {
        self.connected.contains(&mac.to_ascii_uppercase())
    }

    /// Remove a pad entirely: no longer expected to wake and no longer counted
    /// as connected.
    pub fn unregister_known(&mut self, mac: &str) {
        let mac = mac.to_ascii_uppercase();
        self.known.remove(&mac);
        self.connected.remove(&mac);
    }

    pub fn seen(&self) -> &SeenAdverts {
        &self.seen
    }

    pub fn idle_cycles(&self) -> u32 {
        self.idle_cycles
    }

    /// Analyse 9.1 steps 1-5: early-out, discovery, poll, record, select.
    pub fn poll_once(&mut self, now: Instant) -> io::Result<HubOutcome> {
        // 1. Every known pad connected (or nothing to wake): do not poll, do
        //    not start discovery, and leave the idle counter frozen (legacy).
        if self.known.is_empty() || self.known.iter().all(|mac| self.connected.contains(mac)) {
            return Ok(HubOutcome::AllConnected);
        }

        // 2. Discovery is started exactly once per hub lifetime.
        if !self.discovery_started {
            self.control.start_le_scan()?;
            self.discovery_started = true;
        }

        // 2b. Seed the timed-fallback clock for pads we have not seen yet: the
        // window starts at the first poll after (re)registration, not at boot.
        if self.config.fallback_after.is_some() {
            for mac in &self.known {
                self.recorded_at.entry(mac.clone()).or_insert(now);
            }
        }

        // 3. One snapshot, record every live acceptable advert.
        let devices = self.source.devices()?;
        if self.adapter_mac.is_empty() && !self.warned_missing_adapter {
            self.warned_missing_adapter = true;
            warn!("Hub has no adapter MAC: host-bound adverts will be rejected until it is set");
        }

        // Without an adapter identity we cannot vet Host(_) bindings. Recording
        // with AcceptPolicy::Pairing gives exactly the required fallback:
        // Pairing/Unknown are accepted, Host(_) fails the adapter comparison
        // (no adapter string can match it). With an adapter known, normal wake
        // rules apply.
        let policy = if self.adapter_mac.is_empty() {
            AcceptPolicy::Pairing
        } else {
            AcceptPolicy::Wake
        };

        let mut skipped_liveness = 0usize;
        let mut skipped_no_advert = 0usize;
        let mut recorded = 0usize;

        for device in &devices {
            let mac = device.address.to_ascii_uppercase();

            if !device.is_live() {
                skipped_liveness += 1;
                // A known pad that the backend reports without RSSI (and often
                // without a manufacturer block) is why the hub can look idle
                // while a pad is right there. Say so once per MAC at INFO.
                if self.known.contains(&mac) && self.note_blind_once(&mac) {
                    info!(
                        "{mac}: seen without RSSI/manufacturer data; \
                         adverts for this pad are unusable"
                    );
                }
                continue;
            }

            // An absent block has no PID/host to vet, so it is wake-compatible
            // only for a known bonded MAC (see advert::accepts).
            if device.advert.is_none() && !self.known.contains(&mac) {
                skipped_no_advert += 1;
                continue;
            }

            // Recording never touches connection state: a pad that is still
            // advertising may already be linked (BlueZ caches adverts).
            if self
                .seen
                .record(device.advert.as_ref(), &mac, &self.adapter_mac, policy, now)
            {
                recorded += 1;
                self.recorded_at.insert(mac, now);
            }
        }

        // 4. Connection state comes from this snapshot.
        let connected: HashSet<String> = devices
            .iter()
            .filter(|device| device.connected)
            .map(|device| device.address.to_ascii_uppercase())
            .collect();

        let mut pending = self
            .seen
            .select_pending(now, &mut |mac| connected.contains(mac));
        // Timed fallback: append known-but-invisible pads after the
        // advert-driven ones (adverts keep priority, the batch is still dialled
        // serially).
        let fallback = self.fallback_candidates(now, &connected, &pending);
        let fallback_count = fallback.len();
        pending.extend(fallback);

        debug!(
            devices = devices.len(),
            skipped_liveness,
            skipped_no_advert,
            recorded,
            pending = pending.len(),
            fallback = fallback_count,
            candidates = ?pending.iter().map(|entry| &entry.mac).collect::<Vec<_>>(),
            "hub poll"
        );

        // 5. Idle bookkeeping.
        if pending.is_empty() {
            self.idle_cycles = self.idle_cycles.saturating_add(1);
            if self.idle_cycles >= self.config.idle_restart_cycles {
                self.seen = SeenAdverts::new();
                self.idle_cycles = 0;
                // Analogue of legacy recreating a wedged BleakScanner: if the
                // discovery session died externally (Steam, bluetoothctl) the
                // hub would otherwise stay mute with no visible symptom.
                self.discovery_started = false;
            }
            return Ok(HubOutcome::Idle);
        }

        self.idle_cycles = 0;
        Ok(HubOutcome::Pending(pending))
    }

    /// Whether this is the first time `mac` is seen without a live advert.
    ///
    /// Returns `true` the first time only, so `poll_once` can log the
    /// blind-source reason at INFO once per MAC instead of every cycle.
    fn note_blind_once(&mut self, mac: &str) -> bool {
        self.warned_blind.insert(mac.to_ascii_uppercase())
    }

    /// Known, disconnected pads that have had no acceptable advert for
    /// [`HubConfig::fallback_after`] and were not dialled within
    /// [`HubConfig::fallback_retry`].
    ///
    /// Pure with respect to the clock: `now` decides both windows. Disabled
    /// when `fallback_after` is `None`.
    fn fallback_candidates(
        &self,
        now: Instant,
        connected: &HashSet<String>,
        already_pending: &[PendingConnect],
    ) -> Vec<PendingConnect> {
        let Some(after) = self.config.fallback_after else {
            return Vec::new();
        };

        let mut candidates: Vec<String> = self
            .known
            .iter()
            .filter(|mac| !connected.contains(*mac))
            .filter(|mac| !already_pending.iter().any(|entry| entry.mac == **mac))
            .filter(|mac| {
                self.recorded_at
                    .get(*mac)
                    .map(|recorded| now.saturating_duration_since(*recorded) >= after)
                    .unwrap_or(true)
            })
            .filter(|mac| {
                self.attempted_at
                    .get(*mac)
                    .map(|attempted| {
                        now.saturating_duration_since(*attempted) >= self.config.fallback_retry
                    })
                    .unwrap_or(true)
            })
            .cloned()
            .collect();

        // Deterministic order (the advert-driven part is already sorted).
        candidates.sort();
        candidates
            .into_iter()
            .map(|mac| PendingConnect {
                mac,
                mode: AdvertMode::Wake,
                seen_at: now,
                ttl: WAKE_TTL,
            })
            .collect()
    }

    /// Analyse 9.1 steps 6-7: prepare, remove stale BlueZ objects, dial.
    ///
    /// The caller sleeps [`HubConfig::dial_pause`] before invoking this. Dialing
    /// is strictly serial and a failure never aborts the batch.
    pub fn dial_pending(&mut self, pending: &[PendingConnect]) -> Vec<DialResult> {
        self.dial_pending_at(pending, Instant::now())
    }

    /// [`Self::dial_pending`] with an explicit clock (fallback retry windows).
    pub fn dial_pending_at(&mut self, pending: &[PendingConnect], now: Instant) -> Vec<DialResult> {
        if pending.is_empty() {
            return Vec::new();
        }

        // Only one LE create-connection may be outstanding; a poisoned lock
        // must degrade to "still serialized", never panic the connect path.
        let _guard = self
            .connect_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Err(err) = self.control.prepare_global() {
            debug!("prepare_global failed before dialing: {}", err);
        }

        for entry in pending {
            if let Err(err) = self.control.remove_bluez_device(&entry.mac) {
                debug!("remove_bluez_device({}) failed: {}", entry.mac, err);
            }
        }

        let mut results = Vec::with_capacity(pending.len());
        for entry in pending {
            // Remember the attempt so the fallback retry window applies even
            // when the pad never advertises.
            self.attempted_at.insert(entry.mac.clone(), now);
            match self.dialer.dial(&entry.mac) {
                Ok(()) => {
                    debug!("Dialed {}", entry.mac);
                    self.seen.forget(&entry.mac);
                    self.connected.insert(entry.mac.clone());
                    results.push(DialResult {
                        mac: entry.mac.clone(),
                        outcome: Ok(()),
                    });
                }
                Err(err) => {
                    debug!("Dialing {} failed: {}", entry.mac, err);
                    // Legacy clears the stuck scan before the next MAC so the
                    // next create-connection is not blocked by discovery.
                    if let Err(stop_err) = self.control.stop_le_scan(true) {
                        debug!("Forced scan-off after failure failed: {}", stop_err);
                    }
                    results.push(DialResult {
                        mac: entry.mac.clone(),
                        outcome: Err(err.to_string()),
                    });
                }
            }
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};

    use crate::bluetooth::scan::{AdvertMode, DeviceAdvert};
    use crate::protocol::advert::{host_field_from_adapter, AdvertInfo, HostBinding, WAKE_TTL};
    use crate::protocol::constants::{NINTENDO_VENDOR_ID, PRO_CONTROLLER2_PID};

    const ADAPTER: &str = "AA:BB:CC:DD:EE:FF";
    const OTHER_ADAPTER: &str = "11:22:33:44:55:66";
    const PAD_A: &str = "AA:BB:CC:DD:EE:01";
    const PAD_B: &str = "AA:BB:CC:DD:EE:02";

    /// Shared call log. `Rc<RefCell<..>>` is impossible here because
    /// `AdvertSource: Send` and `ScanControl: Send + Sync`.
    #[derive(Clone, Default)]
    struct Recorder(Arc<StdMutex<Vec<String>>>);

    impl Recorder {
        fn push(&self, entry: impl Into<String>) {
            self.0.lock().unwrap().push(entry.into());
        }

        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }

        fn count(&self, entry: &str) -> usize {
            self.entries().iter().filter(|line| line == &entry).count()
        }

        fn index_of(&self, entry: &str) -> Option<usize> {
            self.entries().iter().position(|line| line == entry)
        }
    }

    struct FakeSource {
        snapshots: VecDeque<Vec<DeviceAdvert>>,
        polls: Arc<StdMutex<usize>>,
    }

    impl AdvertSource for FakeSource {
        fn devices(&mut self) -> io::Result<Vec<DeviceAdvert>> {
            *self.polls.lock().unwrap() += 1;
            Ok(self.snapshots.pop_front().unwrap_or_default())
        }
    }

    struct FakeControl {
        calls: Recorder,
    }

    impl ScanControl for FakeControl {
        fn prepare_global(&self) -> io::Result<()> {
            self.calls.push("prepare");
            Ok(())
        }

        fn start_le_scan(&self) -> io::Result<()> {
            self.calls.push("start");
            Ok(())
        }

        fn stop_le_scan(&self, force: bool) -> io::Result<()> {
            self.calls.push(if force { "stop:force" } else { "stop" });
            Ok(())
        }

        fn remove_bluez_device(&self, mac: &str) -> io::Result<()> {
            self.calls.push(format!("remove:{mac}"));
            Ok(())
        }
    }

    struct FakeDialer {
        calls: Recorder,
        fail: HashSet<String>,
    }

    impl Dialer for FakeDialer {
        fn dial(&mut self, mac: &str) -> io::Result<()> {
            self.calls.push(format!("dial:{mac}"));
            if self.fail.contains(mac) {
                Err(io::Error::other(format!("dial failed for {mac}")))
            } else {
                Ok(())
            }
        }
    }

    fn make_hub(
        config: HubConfig,
        snapshots: Vec<Vec<DeviceAdvert>>,
        failing: &[&str],
    ) -> (
        Hub<FakeSource, FakeControl, FakeDialer>,
        Recorder,
        Arc<StdMutex<usize>>,
    ) {
        let calls = Recorder::default();
        let polls = Arc::new(StdMutex::new(0));
        let source = FakeSource {
            snapshots: snapshots.into(),
            polls: polls.clone(),
        };
        let control = FakeControl {
            calls: calls.clone(),
        };
        let dialer = FakeDialer {
            calls: calls.clone(),
            fail: failing.iter().map(|mac| mac.to_string()).collect(),
        };
        (Hub::new(source, control, dialer, config), calls, polls)
    }

    fn poll_count(polls: &Arc<StdMutex<usize>>) -> usize {
        *polls.lock().unwrap()
    }

    fn info(mac: &str, host: HostBinding) -> AdvertInfo {
        AdvertInfo {
            address: mac.to_ascii_uppercase(),
            vid: NINTENDO_VENDOR_ID,
            pid: PRO_CONTROLLER2_PID,
            host,
        }
    }

    fn bonded_to(adapter: &str) -> HostBinding {
        HostBinding::Host(host_field_from_adapter(adapter).expect("valid adapter MAC"))
    }

    fn device(
        mac: &str,
        connected: bool,
        rssi: Option<i16>,
        advert: Option<AdvertInfo>,
    ) -> DeviceAdvert {
        DeviceAdvert {
            address: mac.to_string(),
            connected,
            rssi,
            advert,
        }
    }

    fn live_pad(mac: &str) -> DeviceAdvert {
        device(mac, false, Some(-50), Some(info(mac, bonded_to(ADAPTER))))
    }

    fn pending(mac: &str, seen_at: Instant) -> PendingConnect {
        PendingConnect {
            mac: mac.to_string(),
            mode: AdvertMode::Wake,
            seen_at,
            ttl: WAKE_TTL,
        }
    }

    fn register_default_pads(hub: &mut Hub<FakeSource, FakeControl, FakeDialer>) {
        hub.register_known(PAD_A);
        hub.register_known(PAD_B);
        hub.set_adapter_mac(ADAPTER);
    }

    #[test]
    fn all_connected_early_out_does_not_poll_or_touch_counter() {
        let base = Instant::now();
        // First cycle: non-live pad -> Idle counter 1. Then dial successfully.
        let (mut hub, calls, polls) = make_hub(
            HubConfig::default(),
            vec![vec![device(PAD_A, false, Some(-127), None)]],
            &[],
        );
        register_default_pads(&mut hub);

        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);
        assert_eq!(hub.idle_cycles(), 1);
        assert_eq!(poll_count(&polls), 1);

        let dials = hub.dial_pending(&[pending(PAD_A, base)]);
        assert_eq!(dials.len(), 1);
        assert!(dials[0].outcome.is_ok());
        let calls_before = calls.entries().len();

        // Every known pad is not yet connected (B has not dialed), so this is
        // still a real cycle; dial B too.
        let dials = hub.dial_pending(&[pending(PAD_B, base)]);
        assert!(dials[0].outcome.is_ok());
        let calls_before = calls_before.max(calls.entries().len());

        let polls_before = poll_count(&polls);
        assert_eq!(
            hub.poll_once(base + Duration::from_secs(1)).expect("poll"),
            HubOutcome::AllConnected
        );
        assert_eq!(poll_count(&polls), polls_before, "must not poll the source");
        assert_eq!(hub.idle_cycles(), 1, "counter must stay frozen");
        assert_eq!(calls.entries().len(), calls_before, "no control traffic");
    }

    #[test]
    fn start_discovery_only_once_per_lifetime() {
        let base = Instant::now();
        let (mut hub, calls, polls) = make_hub(
            HubConfig::default(),
            vec![
                vec![device(PAD_A, false, Some(-127), None)],
                vec![device(PAD_A, false, Some(-127), None)],
            ],
            &[],
        );
        register_default_pads(&mut hub);

        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);
        assert_eq!(
            hub.poll_once(base + Duration::from_millis(250))
                .expect("poll"),
            HubOutcome::Idle
        );
        assert_eq!(poll_count(&polls), 2);
        assert_eq!(calls.count("start"), 1, "discovery starts once");
    }

    #[test]
    fn records_live_accepted_advert() {
        let base = Instant::now();
        let (mut hub, _calls, _polls) =
            make_hub(HubConfig::default(), vec![vec![live_pad(PAD_A)]], &[]);
        register_default_pads(&mut hub);

        match hub.poll_once(base).expect("poll") {
            HubOutcome::Pending(pending) => {
                assert_eq!(pending.len(), 1);
                assert_eq!(pending[0].mac, PAD_A);
                assert_eq!(pending[0].mode, AdvertMode::Wake);
                assert_eq!(pending[0].ttl, WAKE_TTL);
            }
            other => panic!("expected Pending, got {other:?}"),
        }
        assert_eq!(hub.seen().len(), 1);
    }

    #[test]
    fn rejects_non_live_device() {
        let base = Instant::now();
        let (mut hub, _calls, _polls) = make_hub(
            HubConfig::default(),
            vec![vec![device(
                PAD_A,
                false,
                Some(-127),
                Some(info(PAD_A, bonded_to(ADAPTER))),
            )]],
            &[],
        );
        register_default_pads(&mut hub);

        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);
        assert!(hub.seen().is_empty());
    }

    #[test]
    fn rejects_foreign_host_advert() {
        let base = Instant::now();
        let (mut hub, _calls, _polls) = make_hub(
            HubConfig::default(),
            vec![vec![device(
                PAD_A,
                false,
                Some(-50),
                Some(info(PAD_A, bonded_to(OTHER_ADAPTER))),
            )]],
            &[],
        );
        register_default_pads(&mut hub);

        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);
        assert!(hub.seen().is_empty());
    }

    #[test]
    fn unknown_address_with_absent_advert_is_not_recorded() {
        let base = Instant::now();
        let (mut hub, _calls, _polls) = make_hub(
            HubConfig::default(),
            vec![
                // Unknown MAC, no manufacturer block: never wake-compatible.
                vec![device("AA:BB:CC:DD:EE:99", false, Some(-50), None)],
                // Known MAC, same absent block: legacy wake-compatible.
                vec![device(PAD_A, false, Some(-50), None)],
            ],
            &[],
        );
        register_default_pads(&mut hub);

        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);
        assert!(hub.seen().is_empty());

        match hub
            .poll_once(base + Duration::from_millis(250))
            .expect("poll")
        {
            HubOutcome::Pending(pending) => assert_eq!(pending[0].mac, PAD_A),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn pending_sorted_newest_first_and_dialed_serially() {
        let base = Instant::now();
        let (mut hub, calls, _polls) = make_hub(
            HubConfig::default(),
            vec![
                vec![live_pad(PAD_A)],
                vec![live_pad(PAD_B)],
                // Nothing new: pending comes from the seen table.
                Vec::new(),
            ],
            &[],
        );
        register_default_pads(&mut hub);

        hub.poll_once(base).expect("poll A");
        hub.poll_once(base + Duration::from_secs(1))
            .expect("poll B");

        let pending = match hub.poll_once(base + Duration::from_secs(2)).expect("poll") {
            HubOutcome::Pending(pending) => pending,
            other => panic!("expected Pending, got {other:?}"),
        };
        let order: Vec<&str> = pending.iter().map(|entry| entry.mac.as_str()).collect();
        assert_eq!(order, vec![PAD_B, PAD_A], "newest first");

        let results = hub.dial_pending(&pending);
        assert_eq!(results.len(), 2);
        let dials: Vec<String> = calls
            .entries()
            .into_iter()
            .filter(|line| line.starts_with("dial:"))
            .collect();
        assert_eq!(
            dials,
            vec![format!("dial:{PAD_B}"), format!("dial:{PAD_A}")]
        );
    }

    #[test]
    fn failed_dial_forces_scan_off_and_continues() {
        let base = Instant::now();
        let (mut hub, calls, _polls) = make_hub(HubConfig::default(), Vec::new(), &[PAD_A]);
        register_default_pads(&mut hub);

        let results = hub.dial_pending(&[pending(PAD_A, base), pending(PAD_B, base)]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].mac, PAD_A);
        let error = results[0].outcome.as_ref().expect_err("first dial fails");
        assert!(error.contains(PAD_A), "error: {error}");
        assert_eq!(results[1].outcome, Ok(()), "batch continues after failure");

        assert_eq!(calls.count("stop:force"), 1);
        let stop = calls.index_of("stop:force").expect("scan off called");
        let dial_a = calls.index_of(&format!("dial:{PAD_A}")).expect("dialled A");
        let dial_b = calls.index_of(&format!("dial:{PAD_B}")).expect("dialled B");
        assert!(
            dial_a < stop && stop < dial_b,
            "stop sits between the dials"
        );
    }

    #[test]
    fn successful_dial_forgets_entry() {
        let base = Instant::now();
        let (mut hub, _calls, _polls) =
            make_hub(HubConfig::default(), vec![vec![live_pad(PAD_A)]], &[]);
        register_default_pads(&mut hub);

        let pending = match hub.poll_once(base).expect("poll") {
            HubOutcome::Pending(pending) => pending,
            other => panic!("expected Pending, got {other:?}"),
        };
        assert_eq!(hub.seen().len(), 1);

        let results = hub.dial_pending(&pending);
        assert_eq!(results[0].outcome, Ok(()));
        assert!(hub.seen().is_empty(), "successful dial forgets the entry");
    }

    #[test]
    fn connected_pad_that_still_advertises_does_not_reopen_scanning() {
        let base = Instant::now();
        // A is connected but BlueZ still reports its advert/RSSI.
        let connected_advert = device(
            PAD_A,
            true,
            Some(-50),
            Some(info(PAD_A, bonded_to(ADAPTER))),
        );
        let (mut hub, _calls, polls) = make_hub(
            HubConfig::default(),
            vec![vec![connected_advert, live_pad(PAD_B)]],
            &[],
        );
        hub.register_known(PAD_A);
        hub.set_adapter_mac(ADAPTER);
        hub.mark_connected(PAD_A);
        assert!(hub.is_connected(PAD_A));

        // Single connected pad: early-out, source untouched even though the
        // snapshot it would have returned contains a live advert.
        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::AllConnected);
        assert_eq!(poll_count(&polls), 0);
        assert!(hub.is_connected(PAD_A));

        // With a second, disconnected pad registered the record loop does run
        // and must not flip A to disconnected.
        hub.register_known(PAD_B);
        match hub
            .poll_once(base + Duration::from_millis(250))
            .expect("poll")
        {
            HubOutcome::Pending(pending) => {
                let macs: Vec<&str> = pending.iter().map(|entry| entry.mac.as_str()).collect();
                assert_eq!(macs, vec![PAD_B]);
            }
            other => panic!("expected Pending for {PAD_B}, got {other:?}"),
        }
        assert_eq!(poll_count(&polls), 1);
        assert!(
            hub.is_connected(PAD_A),
            "recording an advert must not change explicit connection state"
        );
    }

    #[test]
    fn mark_disconnected_reopens_scanning() {
        let base = Instant::now();
        let (mut hub, _calls, polls) =
            make_hub(HubConfig::default(), vec![vec![live_pad(PAD_B)]], &[]);
        register_default_pads(&mut hub);
        hub.mark_connected(PAD_A);
        hub.mark_connected(PAD_B);

        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::AllConnected);
        assert_eq!(poll_count(&polls), 0);

        hub.mark_disconnected(PAD_B);
        assert!(!hub.is_connected(PAD_B));

        match hub
            .poll_once(base + Duration::from_millis(250))
            .expect("poll")
        {
            HubOutcome::Pending(pending) => {
                let macs: Vec<&str> = pending.iter().map(|entry| entry.mac.as_str()).collect();
                assert_eq!(macs, vec![PAD_B]);
            }
            other => panic!("expected Pending, got {other:?}"),
        }
        assert_eq!(poll_count(&polls), 1, "scanning resumed");
        assert!(hub.is_connected(PAD_A));
    }

    #[test]
    fn unregister_known_removes_from_early_out() {
        let base = Instant::now();
        let (mut hub, _calls, polls) = make_hub(
            HubConfig::default(),
            vec![vec![device(PAD_B, false, Some(-127), None)]],
            &[],
        );
        register_default_pads(&mut hub);
        hub.mark_connected(PAD_A);

        // B is known and disconnected, so this cycle polls.
        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);
        assert_eq!(poll_count(&polls), 1);

        hub.unregister_known(PAD_B);
        assert!(!hub.is_connected(PAD_B));

        // Only A remains and it is connected: early-out without polling.
        assert_eq!(
            hub.poll_once(base + Duration::from_millis(250))
                .expect("poll"),
            HubOutcome::AllConnected
        );
        assert_eq!(
            poll_count(&polls),
            1,
            "unregistered pad no longer forces a poll"
        );
    }

    #[test]
    fn idle_restart_reissues_start_discovery() {
        let base = Instant::now();
        let config = HubConfig {
            idle_restart_cycles: 1,
            ..HubConfig::default()
        };
        let (mut hub, calls, _polls) = make_hub(
            config,
            vec![
                vec![device(PAD_A, false, Some(-127), None)],
                vec![device(PAD_A, false, Some(-127), None)],
            ],
            &[],
        );
        register_default_pads(&mut hub);

        // First cycle starts discovery, then hits the idle threshold.
        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);
        assert_eq!(calls.count("start"), 1);
        assert_eq!(hub.idle_cycles(), 0, "threshold reset the counter");

        // Restart must re-issue StartDiscovery (the session may have died).
        assert_eq!(
            hub.poll_once(base + Duration::from_millis(250))
                .expect("poll"),
            HubOutcome::Idle
        );
        assert_eq!(
            calls.count("start"),
            2,
            "start re-issued across the restart"
        );
    }

    #[test]
    fn remove_bluez_device_called_for_every_pending_before_any_dial() {
        let base = Instant::now();
        let (mut hub, calls, _polls) = make_hub(HubConfig::default(), Vec::new(), &[]);
        register_default_pads(&mut hub);

        let results = hub.dial_pending(&[pending(PAD_A, base), pending(PAD_B, base)]);
        assert_eq!(results.len(), 2);

        let log = calls.entries();
        let first_dial = log
            .iter()
            .position(|line| line.starts_with("dial:"))
            .expect("dialled");
        let removes: Vec<usize> = log
            .iter()
            .enumerate()
            .filter(|(_, line)| line.starts_with("remove:"))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(removes.len(), 2);
        assert!(removes.iter().all(|index| *index < first_dial));
        assert_eq!(calls.count(&format!("remove:{PAD_A}")), 1);
        assert_eq!(calls.count(&format!("remove:{PAD_B}")), 1);
    }

    #[test]
    fn idle_restart_clears_seen_at_threshold() {
        let base = Instant::now();
        let config = HubConfig {
            idle_restart_cycles: 2,
            ..HubConfig::default()
        };
        let connected_pad = device(
            PAD_A,
            true,
            Some(-50),
            Some(info(PAD_A, bonded_to(ADAPTER))),
        );
        let (mut hub, _calls, _polls) = make_hub(
            config,
            vec![
                vec![live_pad(PAD_A)],
                vec![connected_pad.clone()],
                vec![connected_pad],
            ],
            &[],
        );
        register_default_pads(&mut hub);

        assert!(matches!(
            hub.poll_once(base).expect("poll"),
            HubOutcome::Pending(_)
        ));
        assert_eq!(hub.seen().len(), 1);

        assert_eq!(
            hub.poll_once(base + Duration::from_millis(250))
                .expect("poll"),
            HubOutcome::Idle
        );
        assert_eq!(hub.idle_cycles(), 1);

        assert_eq!(
            hub.poll_once(base + Duration::from_millis(500))
                .expect("poll"),
            HubOutcome::Idle
        );
        assert_eq!(hub.idle_cycles(), 0, "counter resets at the threshold");
        assert!(hub.seen().is_empty(), "idle restart clears the seen table");
    }

    #[test]
    fn idle_counter_frozen_while_all_connected() {
        let base = Instant::now();
        let (mut hub, _calls, polls) = make_hub(
            HubConfig::default(),
            vec![vec![device(PAD_A, false, Some(-127), None)]],
            &[],
        );
        register_default_pads(&mut hub);

        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);
        assert_eq!(hub.idle_cycles(), 1);

        assert!(hub.dial_pending(&[pending(PAD_A, base)])[0].outcome.is_ok());
        assert!(hub.dial_pending(&[pending(PAD_B, base)])[0].outcome.is_ok());

        let polls_before = poll_count(&polls);
        for tick in 1..=3 {
            assert_eq!(
                hub.poll_once(base + Duration::from_millis(250 * tick))
                    .expect("poll"),
                HubOutcome::AllConnected
            );
        }
        assert_eq!(hub.idle_cycles(), 1, "frozen across all-connected cycles");
        assert_eq!(poll_count(&polls), polls_before);
    }

    #[test]
    fn empty_adapter_mac_rejects_host_bound_adverts() {
        let base = Instant::now();
        let (mut hub, _calls, _polls) = make_hub(
            HubConfig::default(),
            vec![
                vec![device(
                    PAD_A,
                    false,
                    Some(-50),
                    Some(info(PAD_A, bonded_to(ADAPTER))),
                )],
                vec![device(
                    PAD_A,
                    false,
                    Some(-50),
                    Some(info(PAD_A, HostBinding::Pairing)),
                )],
                vec![device(PAD_A, false, Some(-50), None)],
            ],
            &[],
        );
        // Deliberately no set_adapter_mac: adapter identity unknown.
        hub.register_known(PAD_A);
        hub.register_known(PAD_B);

        // Host-bound advert cannot be vetted -> rejected.
        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);
        assert!(hub.seen().is_empty());

        // Pairing-mode advert is still acceptable.
        assert!(matches!(
            hub.poll_once(base + Duration::from_millis(250)).expect("poll"),
            HubOutcome::Pending(pending) if pending[0].mac == PAD_A
        ));

        // And so is an absent block for a known MAC.
        assert!(matches!(
            hub.poll_once(base + Duration::from_millis(500)).expect("poll"),
            HubOutcome::Pending(pending) if pending[0].mac == PAD_A
        ));
    }

    #[test]
    fn scanning_continues_while_any_pad_is_disconnected() {
        let base = Instant::now();
        // PAD_A is connected, PAD_B is advertising: the early-out must not fire
        // (HANDOVER Bug 1: `if connected_count: stop scanning` locked out P2).
        let (mut hub, _calls, polls) = make_hub(
            HubConfig::default(),
            vec![vec![device(PAD_A, true, Some(-50), None), live_pad(PAD_B)]],
            &[],
        );
        register_default_pads(&mut hub);

        // Mark A connected through a real dial so the hub knows it is up.
        assert!(hub.dial_pending(&[pending(PAD_A, base)])[0].outcome.is_ok());

        let polls_before = poll_count(&polls);
        let outcome = hub
            .poll_once(base + Duration::from_millis(250))
            .expect("poll");
        assert_eq!(
            poll_count(&polls),
            polls_before + 1,
            "a disconnected known pad must keep the source polled"
        );
        match outcome {
            HubOutcome::Pending(pending) => {
                let macs: Vec<&str> = pending.iter().map(|entry| entry.mac.as_str()).collect();
                assert_eq!(macs, vec![PAD_B]);
            }
            other => panic!("expected Pending for {PAD_B}, got {other:?}"),
        }
    }

    fn blind_pad(mac: &str) -> DeviceAdvert {
        // BlueZ reports the Device1 object (optionally Connected) but without
        // the live-advert fields: no RSSI, no ManufacturerData.
        device(mac, false, None, None)
    }

    fn fallback_config(after: Option<Duration>, retry: Duration) -> HubConfig {
        HubConfig {
            fallback_after: after,
            fallback_retry: retry,
            ..HubConfig::default()
        }
    }

    fn pending_macs(outcome: HubOutcome) -> Vec<String> {
        match outcome {
            HubOutcome::Pending(pending) => pending.iter().map(|entry| entry.mac.clone()).collect(),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn fallback_dials_a_known_pad_that_never_advertises() {
        let base = Instant::now();
        let (mut hub, calls, _) = make_hub(
            fallback_config(Some(Duration::from_secs(4)), Duration::from_secs(3)),
            vec![
                vec![blind_pad(PAD_A)],
                vec![blind_pad(PAD_A)],
                vec![blind_pad(PAD_A)],
                vec![blind_pad(PAD_A)],
            ],
            &[],
        );
        register_default_pads(&mut hub);
        hub.unregister_known(PAD_B);
        calls.entries();

        // A blind pad is not a candidate inside the fallback window.
        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);
        assert_eq!(
            hub.poll_once(base + Duration::from_millis(3999))
                .expect("poll"),
            HubOutcome::Idle
        );

        let macs = pending_macs(hub.poll_once(base + Duration::from_secs(4)).expect("poll"));
        assert_eq!(macs, vec![PAD_A.to_string()]);

        let batch = [pending(PAD_A, base + Duration::from_secs(4))];
        let results = hub.dial_pending_at(&batch, base + Duration::from_secs(4));
        assert_eq!(results.len(), 1);
        assert!(results[0].outcome.is_ok());
        assert_eq!(calls.count("dial:AA:BB:CC:DD:EE:01"), 1);
    }

    #[test]
    fn fallback_is_suppressed_for_the_retry_window() {
        let base = Instant::now();
        let (mut hub, calls, _) = make_hub(
            fallback_config(Some(Duration::from_secs(4)), Duration::from_secs(3)),
            (0..6).map(|_| vec![blind_pad(PAD_A)]).collect(),
            &[PAD_A],
        );
        register_default_pads(&mut hub);
        hub.unregister_known(PAD_B);

        // First poll seeds the fallback clock; nothing is due yet.
        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);

        let window = base + Duration::from_secs(4);
        let macs = pending_macs(hub.poll_once(window).expect("poll"));
        assert_eq!(macs, vec![PAD_A.to_string()]);

        // The dial fails; the attempt is remembered.
        let batch = [pending(PAD_A, window)];
        let results = hub.dial_pending_at(&batch, window);
        assert!(results[0].outcome.is_err());
        assert_eq!(calls.count("dial:AA:BB:CC:DD:EE:01"), 1);
        assert!(!hub.is_connected(PAD_A));

        // Inside the retry window the pad is not offered again.
        assert_eq!(
            hub.poll_once(window + Duration::from_secs(1))
                .expect("poll"),
            HubOutcome::Idle
        );
        assert_eq!(
            hub.poll_once(window + Duration::from_millis(2999))
                .expect("poll"),
            HubOutcome::Idle
        );

        // Retry window elapsed -> candidate again.
        assert_eq!(
            pending_macs(
                hub.poll_once(window + Duration::from_secs(3))
                    .expect("poll")
            ),
            vec![PAD_A.to_string()]
        );
    }

    #[test]
    fn fallback_off_keeps_advert_only_semantics() {
        let base = Instant::now();
        let (mut hub, calls, _) = make_hub(
            HubConfig::default(),
            (0..3).map(|_| vec![blind_pad(PAD_A)]).collect(),
            &[],
        );
        hub.register_known(PAD_A);
        hub.set_adapter_mac(ADAPTER);

        assert_eq!(hub.poll_once(base).expect("poll"), HubOutcome::Idle);
        assert_eq!(
            hub.poll_once(base + Duration::from_secs(100))
                .expect("poll"),
            HubOutcome::Idle,
            "fallback_after: None must never dial on a timer"
        );
        assert_eq!(calls.count("dial:AA:BB:CC:DD:EE:01"), 0);
    }

    #[test]
    fn hub_logs_a_blind_source_once() {
        let (mut hub, _, _) = make_hub(HubConfig::default(), Vec::new(), &[]);
        hub.register_known(PAD_A);

        assert!(hub.note_blind_once(PAD_A), "first sighting is logged");
        assert!(!hub.note_blind_once(PAD_A), "later sightings are silent");
        assert!(!hub.note_blind_once(&PAD_A.to_ascii_lowercase()));
        assert!(hub.note_blind_once(PAD_B), "per MAC, not global");
    }
}
