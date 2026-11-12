//! High-level controller abstraction managing handshake, calibration, and input streaming.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::bluetooth::att::{AttClient, ResolvedHandles};
use crate::bluetooth::l2cap::L2capSocket;
use crate::protocol::calibration::{
    normalize_gc_trigger, StickCalibration, GC_TRIGGER_DEFAULT_NEUTRAL,
};
use crate::protocol::command::{mac_to_le, ControllerInfo};
use crate::protocol::constants::*;
use crate::protocol::report::InputReport;
use crate::protocol::vibration::{preset_command_data, Preset};

pub type ControllerInputCallback = Box<dyn Fn(&InputReport) + Send + Sync + 'static>;

/// Retries per legacy `_retry` (device.py initialise).
const RETRY_ATTEMPTS: u32 = 3;
/// Delay between legacy retries.
const RETRY_DELAY: Duration = Duration::from_millis(100);
/// A 63-byte input report needs the 3-byte ATT notification header on top.
const MIN_ATT_MTU_FOR_REPORTS: u16 = 66;
/// Legacy `read_memory` limit (device.py).
const MAX_MEMORY_READ_LEN: u8 = 0x4F;
/// Legacy calibration reads 0x0B bytes (three packed 12-bit pairs plus slack).
const CALIBRATION_LEN: u8 = 0x0B;
/// Only the first three packed triples are consumed by `get_stick_xy`.
const CALIBRATION_MIN_PARSE_LEN: usize = 9;
/// Timeout for each bonding command ACK.
const BOND_TIMEOUT: Duration = Duration::from_millis(1500);

/// Where a stick's calibration came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationSource {
    /// User calibration area holds real values.
    User,
    /// User area unwritten/invalid; factory area used.
    Factory,
    /// Neither area usable; [`StickCalibration::default`] kept.
    Default,
}

/// Whether to run the pairing (bonding) command sequence during connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondPolicy {
    /// Never bond.
    Skip,
    /// Bond only when the caller does not already know the pad is bonded.
    IfNeeded,
    /// Always re-bond (explicit `switch2d pair`).
    Force,
}

/// Handshake options for [`SwitchController::connect_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectOptions {
    pub player: u8,
    pub bond: BondPolicy,
}

/// Decide whether to run the bonding sequence.
///
/// `already_bonded` is the persisted config flag (`entry.bonded`). The daemon
/// already collapses that to [`BondPolicy::Skip`]; this helper exists so the
/// policy stays a pure, testable decision.
pub fn should_bond(policy: BondPolicy, already_bonded: bool) -> bool {
    match policy {
        BondPolicy::Skip => false,
        BondPolicy::Force => true,
        BondPolicy::IfNeeded => !already_bonded,
    }
}

/// Pick stick calibration from the user and factory areas.
///
/// `StickCalibration::from_bytes` rejects short data and the unwritten
/// `FF FF FF` marker, so an all-FF user area falls through to the factory.
pub fn choose_calibration(
    user: Option<&[u8]>,
    factory: Option<&[u8]>,
) -> (StickCalibration, CalibrationSource) {
    if let Some(calibration) = user.and_then(StickCalibration::from_bytes) {
        return (calibration, CalibrationSource::User);
    }
    if let Some(calibration) = factory.and_then(StickCalibration::from_bytes) {
        return (calibration, CalibrationSource::Factory);
    }
    (StickCalibration::default(), CalibrationSource::Default)
}

/// GC trigger neutral byte, rejecting the `0x00`/`0xFF` sentinels.
pub fn gc_trigger_neutral(raw: u8) -> u8 {
    if raw == 0x00 || raw == 0xFF {
        GC_TRIGGER_DEFAULT_NEUTRAL
    } else {
        raw
    }
}

/// Stick deadzone used by every calibrated-input consumer. This matches the
/// uinput mapper (`uinput/combined.rs` uses 120) so the monitor, worker, and
/// daemon all agree on the neutral region.
pub const STICK_DEADZONE: i32 = 120;

/// Calibrated input mirroring legacy `SwitchController.calibrated_input()`
/// (`device.py:436-454`): left/right stick floats in `-1.0..=1.0`, triggers in
/// `0..=255`.
///
/// This is the single source of truth for stick/trigger conversion; the monitor,
/// worker, and daemon must all go through [`SwitchController::calibrated_input`]
/// rather than re-implementing the mapping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibratedInput {
    pub left: (f32, f32),
    pub right: (f32, f32),
    pub lt: u8,
    pub rt: u8,
}

impl Default for CalibratedInput {
    fn default() -> Self {
        Self {
            left: (0.0, 0.0),
            right: (0.0, 0.0),
            lt: 0,
            rt: 0,
        }
    }
}

/// Whether a product id exposes true analog L/R triggers (legacy
/// `protocol.py:54`): only the NSO GameCube pad does.
pub fn has_analog_triggers(product_id: u16) -> bool {
    product_id == NSO_GAMECUBE_PID
}

/// Pure core of [`SwitchController::calibrated_input`].
///
/// Mirrors legacy `device.py:436-454`:
/// * sticks use [`StickCalibration::apply`] with [`STICK_DEADZONE`];
/// * pads with analog triggers (NSO GameCube) normalize raw L/R against their
///   neutral value via [`normalize_gc_trigger`];
/// * all other pads map the digital ZL/ZR bits to 0 or 255;
/// * the digital L/R click forces its analog trigger to 255 (legacy
///   `device.py:443-447`) so the axis agrees with the shoulder button.
pub fn calibrate(
    left_calib: &StickCalibration,
    right_calib: &StickCalibration,
    trigger_neutral: (u8, u8),
    has_analog_triggers: bool,
    report: &InputReport,
) -> CalibratedInput {
    let left = left_calib.apply(report.left_stick_raw, STICK_DEADZONE);
    let right = right_calib.apply(report.right_stick_raw, STICK_DEADZONE);

    let (mut lt, mut rt) = if has_analog_triggers {
        (
            normalize_gc_trigger(report.left_trigger_raw, trigger_neutral.0),
            normalize_gc_trigger(report.right_trigger_raw, trigger_neutral.1),
        )
    } else {
        (
            u8::from(report.is_pressed(button_mask::ZL)) * 255,
            u8::from(report.is_pressed(button_mask::ZR)) * 255,
        )
    };

    // Digital L/R click fires at full pull; keep axes in sync with shoulders.
    if report.is_pressed(button_mask::L) {
        lt = 255;
    }
    if report.is_pressed(button_mask::R) {
        rt = 255;
    }

    CalibratedInput { left, right, lt, rt }
}

/// Run `f` up to `attempts` times, sleeping `delay` between attempts.
///
/// The final error keeps the last attempt's [`io::ErrorKind`] and names both
/// `label` and the attempt count (legacy `RuntimeError("<label> failed after 3
/// tries: …")`).
pub fn retry_with<T, F>(label: &str, attempts: u32, delay: Duration, mut f: F) -> io::Result<T>
where
    F: FnMut() -> io::Result<T>,
{
    let attempts = attempts.max(1);
    let mut last_error = None;

    for attempt in 1..=attempts {
        match f() {
            Ok(value) => return Ok(value),
            Err(err) => {
                debug!("{} attempt {}/{} failed: {}", label, attempt, attempts, err);
                last_error = Some(err);
                if attempt < attempts {
                    thread::sleep(delay);
                }
            }
        }
    }

    match last_error {
        Some(err) => Err(io::Error::new(
            err.kind(),
            format!("{label} failed after {attempts} tries: {err}"),
        )),
        None => Err(io::Error::other(format!(
            "{label} failed after {attempts} tries"
        ))),
    }
}

/// Total time budget for the LE create-connection (both address types).
const L2CAP_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// Request the legacy 247-byte MTU and insist it can carry a full report.
fn negotiate_mtu(att: &AttClient, requested: u16) -> io::Result<u16> {
    let mtu = att.exchange_mtu(requested)?;
    info!("Negotiated ATT MTU: {}", mtu);

    if mtu < MIN_ATT_MTU_FOR_REPORTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "negotiated ATT MTU {mtu} is below the {MIN_ATT_MTU_FOR_REPORTS}-byte minimum \
                 required for 63-byte input reports"
            ),
        ));
    }

    Ok(mtu)
}

// ---------------------------------------------------------------------------
// Dial envelope (legacy `_ConnectHub._connect_sync`, bridge.py:436-469)
// ---------------------------------------------------------------------------

/// Legacy dial envelope: many short attempts instead of one long one.
///
/// `bridge.py:52-53, 443-469`: 16 rounds, 0.80 s per *(round, address type)*,
/// 20 ms between rounds. Rationale in legacy: a short window fails fast when
/// something else (Steam, decky) keeps the adapter's LE scan busy, while a long
/// attempt can sit inside a pending create-connection and starve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialBudget {
    /// Rounds; each round attempts both address types.
    pub rounds: u32,
    /// Timeout for each address type within a round.
    pub per_type_timeout: Duration,
    /// Pause between rounds.
    pub between_rounds: Duration,
    /// Remove the BlueZ `Device` object once before the first round
    /// ("drop BlueZ Device object so it can't race our raw ATT connect",
    /// bridge.py:136-154).
    ///
    /// Must be **false** while we own a live session for the MAC: removing the
    /// object tears that link down.
    pub remove_bluez_device: bool,
}

impl Default for DialBudget {
    fn default() -> Self {
        Self {
            rounds: 16,
            per_type_timeout: Duration::from_millis(800),
            between_rounds: Duration::from_millis(20),
            remove_bluez_device: true,
        }
    }
}

/// Per-round view of a [`DialBudget`], shrinking as rounds are spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundBudget {
    /// 0-based round index.
    pub round: u32,
    /// Rounds left after this one (shrinks to 0).
    pub rounds_left: u32,
    /// Timeout for *each* address type in this round.
    pub per_type: Duration,
    /// Pause after this round before the next one (`ZERO` after the last).
    pub pause_after: Duration,
}

impl DialBudget {
    /// Plan for `round`, or `None` once the budget is spent.
    pub fn round(&self, round: u32) -> Option<RoundBudget> {
        if round >= self.rounds {
            return None;
        }
        Some(RoundBudget {
            round,
            rounds_left: self.rounds - round - 1,
            per_type: self.per_type_timeout,
            pause_after: if round + 1 < self.rounds {
                self.between_rounds
            } else {
                Duration::ZERO
            },
        })
    }

    /// Worst-case time spent dialling (both address types, every round).
    pub fn worst_case(&self) -> Duration {
        self.per_type_timeout * 2 * self.rounds
            + self.between_rounds * self.rounds.saturating_sub(1)
    }
}

/// Control-plane operations the envelope needs between attempts.
///
/// Blanket-implemented for anything that is a
/// [`crate::bluetooth::scan::ScanControl`] (production:
/// `ShellScanControl::from_env()`); tests inject a recorder.
pub trait DialControl {
    /// Forced global scan stop (also kills helper processes).
    fn prepare_global(&self) -> io::Result<()>;
    /// Rate-limited LE scan stop (0.35 s) used between rounds.
    fn stop_le_scan(&self, force: bool) -> io::Result<()>;
    /// Drop the BlueZ `Device` object for `mac`.
    fn remove_bluez_device(&self, mac: &str) -> io::Result<()>;
}

impl<T: crate::bluetooth::scan::ScanControl> DialControl for T {
    fn prepare_global(&self) -> io::Result<()> {
        crate::bluetooth::scan::ScanControl::prepare_global(self)
    }

    fn stop_le_scan(&self, force: bool) -> io::Result<()> {
        crate::bluetooth::scan::ScanControl::stop_le_scan(self, force)
    }

    fn remove_bluez_device(&self, mac: &str) -> io::Result<()> {
        crate::bluetooth::scan::ScanControl::remove_bluez_device(self, mac)
    }
}

/// What the envelope did, for logging and diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DialReport {
    pub rounds_attempted: u32,
    /// Address-type attempts (2 per round).
    pub attempts: u32,
    /// `"ok"` or `"failed: <err>"`.
    pub discovery_stop: String,
    /// `"ok"`, `"failed: <err>"` or `"skipped (live session)"`.
    pub device_removal: String,
    pub last_error: Option<String>,
}

fn note(result: io::Result<()>) -> String {
    match result {
        Ok(()) => "ok".to_string(),
        Err(err) => format!("failed: {err}"),
    }
}

/// Run the dial envelope with an injectable control plane and dial step.
///
/// The dial step is called once per round and is expected to attempt both
/// address types with `per_type` each (production: [`SwitchController::connect_with_timeout`]).
pub fn dial_with_budget<C, D, T>(
    control: &C,
    budget: DialBudget,
    mac: &str,
    dial: &mut D,
) -> (io::Result<T>, DialReport)
where
    C: DialControl,
    D: FnMut(Duration) -> io::Result<T>,
{
    let mut report = DialReport::default();

    // Forced global stop once, before the first attempt.
    report.discovery_stop = note(control.prepare_global());

    // Drop the BlueZ Device object **once**, before the first round: each
    // `bluetoothctl remove` is slow and races the attempt that follows.
    report.device_removal = if budget.remove_bluez_device {
        note(control.remove_bluez_device(mac))
    } else {
        "skipped (live session)".to_string()
    };

    let mut last_error = None;
    for round in 0..budget.rounds {
        // Non-forced, rate-limited scan stop immediately before every round
        // (legacy issues it before every address-type attempt; the 0.35 s
        // limiter coalesces the pair into one).
        if let Err(err) = control.stop_le_scan(false) {
            debug!("scan stop before round {round} for {mac} failed: {err}");
        }

        report.rounds_attempted = round + 1;
        report.attempts = report.rounds_attempted * 2;

        match dial(budget.per_type_timeout) {
            Ok(session) => {
                if round > 0 {
                    // How many rounds a real pad needed is the signal for
                    // whether this envelope is what fixes the hardware.
                    info!(
                        "Dial to {} succeeded after {} rounds ({} address-type attempts)",
                        mac,
                        round + 1,
                        (round + 1) * 2
                    );
                }
                return (Ok(session), report);
            }
            Err(err) => {
                debug!(
                    "Dial round {}/{} for {} failed: {}",
                    round + 1,
                    budget.rounds,
                    mac,
                    err
                );
                report.last_error = Some(err.to_string());
                last_error = Some(err);
                if let Some(plan) = budget.round(round) {
                    if !plan.pause_after.is_zero() {
                        thread::sleep(plan.pause_after);
                    }
                }
            }
        }
    }

    let err = last_error
        .unwrap_or_else(|| io::Error::other(format!("dial budget for {mac} has 0 rounds")));
    (Err(err), report)
}

pub struct SwitchController {
    pub mac: String,
    pub adapter_mac: String,
    pub info: Option<ControllerInfo>,
    pub left_calib: StickCalibration,
    pub right_calib: StickCalibration,
    pub trigger_neutral: (u8, u8),
    /// True only when the bond sequence ran and all four commands were ACKed.
    pub did_bond: bool,
    att: AttClient,
    handles: ResolvedHandles,
    cmd_resp_state: Arc<(Mutex<Option<Vec<u8>>>, Condvar)>,
    pub is_ready: Arc<AtomicBool>,
}

impl SwitchController {
    /// Connect and initialize a Switch 2 controller, bonding when unsure.
    pub fn connect(
        mac: &str,
        adapter_mac: &str,
        player: u8,
        input_cb: ControllerInputCallback,
    ) -> io::Result<Self> {
        Self::connect_with(
            mac,
            adapter_mac,
            ConnectOptions {
                player,
                bond: BondPolicy::IfNeeded,
            },
            input_cb,
        )
    }

    /// Dial a controller using the legacy envelope ([`DialBudget`]): many short
    /// rounds, fresh socket per address type, scan stopped before every round,
    /// BlueZ `Device` object dropped once up front.
    ///
    /// This is the entry point for monitor, the daemon dialer and `pair`;
    /// [`Self::connect_with`] stays the single-attempt primitive.
    pub fn connect_session(
        mac: &str,
        adapter_mac: &str,
        options: ConnectOptions,
        input_cb: ControllerInputCallback,
        budget: DialBudget,
    ) -> io::Result<Self> {
        let control = crate::bluetooth::scan::ShellScanControl::from_env();

        // Every round needs a fresh callback; share the caller's one.
        let input_cb: Arc<dyn Fn(&InputReport) + Send + Sync> = Arc::from(input_cb);
        let mut dial = |per_type: Duration| {
            let shared = Arc::clone(&input_cb);
            let cb: ControllerInputCallback = Box::new(move |report| shared(report));
            Self::connect_with_timeout(mac, adapter_mac, options, cb, per_type * 2)
        };

        let (result, report) = dial_with_budget(&control, budget, mac, &mut dial);
        result.map_err(|err| {
            // FIX 3 diagnostics: rounds, both address types and their last OS
            // errors (inside `err`), plus what the control plane managed.
            io::Error::new(
                err.kind(),
                format!(
                    "{err} [rounds: {}/{}, attempts: {}, discovery_stop: {}, device_removal: {}]",
                    report.rounds_attempted,
                    budget.rounds,
                    report.attempts,
                    report.discovery_stop,
                    report.device_removal
                ),
            )
        })
    }

    /// Connect and initialize a Switch 2 controller with explicit handshake options.
    ///
    /// Handshake order mirrors legacy `initialize()`: link, MTU, GATT
    /// discovery, command-response subscription, controller info (real PID),
    /// calibration, LEDs, features, input subscription. Bonding, which legacy
    /// runs from the bridge after activation, happens once the real PID and
    /// handles are known.
    pub fn connect_with(
        mac: &str,
        adapter_mac: &str,
        options: ConnectOptions,
        input_cb: ControllerInputCallback,
    ) -> io::Result<Self> {
        Self::connect_with_timeout(mac, adapter_mac, options, input_cb, L2CAP_CONNECT_TIMEOUT)
    }

    /// Single connect attempt with an explicit per-attempt time budget.
    ///
    /// `connect_timeout` is split between the two address types, so the dial
    /// envelope passes `per_type_timeout * 2` here.
    fn connect_with_timeout(
        mac: &str,
        adapter_mac: &str,
        options: ConnectOptions,
        input_cb: ControllerInputCallback,
        connect_timeout: Duration,
    ) -> io::Result<Self> {
        let dest_le = mac_to_le(mac)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid controller MAC"))?;
        let adapter_le = mac_to_le(adapter_mac)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid adapter MAC"))?;

        info!("Connecting to Switch 2 controller {}...", mac);

        // Legacy order: LE_PUBLIC then LE_RANDOM, each with a **fresh socket**
        // (`bridge.py:436-474`). A socket whose connect(2) failed is poisoned,
        // so retrying on it would only ever yield EINVAL and hide the real
        // failure of the first attempt.
        let (socket, addr_type) = L2capSocket::connect_new(adapter_le, dest_le, connect_timeout)
            .map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("connect to {mac} via adapter {adapter_mac}: {err}"),
                )
            })?;
        info!(
            "L2CAP link established with {} (type: {})",
            mac,
            crate::bluetooth::l2cap::address_type_name(addr_type)
        );

        let att = AttClient::new(socket);

        // 1. MTU: a 23-byte MTU silently truncates every 63-byte report.
        negotiate_mtu(&att, 247)?;

        // 2. Discover the GATT layout.
        let services = match att.discover_all() {
            Ok(services) => services,
            Err(err) => {
                warn!(
                    "GATT discovery failed ({}); falling back to default handles",
                    err
                );
                Vec::new()
            }
        };

        let handles = AttClient::resolve_handles(&services, JOYCON2_LEFT_PID);

        let cmd_resp_state = Arc::new((Mutex::new(None), Condvar::new()));
        let cmd_resp_for_ntf = cmd_resp_state.clone();
        let cmd_resp_handle = handles.command_response;
        let input_handle = handles.input_report;

        let is_ready = Arc::new(AtomicBool::new(false));
        let is_ready_clone = is_ready.clone();

        // Register the master notification callback for input reports and
        // command responses. Only the vibration handle is PID-dependent, so the
        // input/command handles captured here cannot change during the
        // post-info re-resolution below.
        att.set_notification_handler(move |handle, data| {
            if handle == input_handle {
                if let Some(report) = InputReport::parse(data) {
                    if is_ready_clone.load(Ordering::Relaxed) {
                        input_cb(&report);
                    }
                }
            } else if handle == cmd_resp_handle {
                let (lock, cvar) = &*cmd_resp_for_ntf;
                let mut resp = lock.lock().unwrap();
                *resp = Some(data.to_vec());
                cvar.notify_one();
            }
        });

        // 3. Subscribe to command responses before anything expects an ACK.
        att.subscribe(handles.command_response_cccd, true)?;
        info!(
            "Subscribed to command responses (handle 0x{:04X})",
            handles.command_response_cccd
        );

        let mut controller = Self {
            mac: mac.to_string(),
            adapter_mac: adapter_mac.to_string(),
            info: None,
            left_calib: StickCalibration::default(),
            right_calib: StickCalibration::default(),
            trigger_neutral: (GC_TRIGGER_DEFAULT_NEUTRAL, GC_TRIGGER_DEFAULT_NEUTRAL),
            did_bond: false,
            att,
            handles,
            cmd_resp_state,
            is_ready,
        };

        // 4. Identify the pad (Joy-Con L/R vs Pro vs NSO GC).
        let controller_info =
            retry_with("controller info read", RETRY_ATTEMPTS, RETRY_DELAY, || {
                controller.read_controller_info()
            })?;
        info!(
            "Identified controller: {} (serial: {})",
            controller_info.name(),
            controller_info.serial_number
        );
        // 5. Re-resolve with the real PID (vibration UUID is PID-specific).
        controller.handles = AttClient::resolve_handles(&services, controller_info.product_id);
        controller.info = Some(controller_info);

        // 6. Calibration. Deliberate deviation from legacy, which raises and
        // tears the worker down: default calibration still produces usable
        // input, so a transient memory-read failure must not cost the session.
        controller.read_calibration();

        // Bonding (legacy does this from the bridge once activation succeeds).
        if should_bond(options.bond, false) {
            match controller.bond(&adapter_le) {
                Ok(()) => {
                    controller.did_bond = true;
                    info!("Bonding sequence completed for {}", mac);
                }
                Err(err) => warn!("Bonding sequence for {} failed: {}", mac, err),
            }
        } else {
            debug!("Skipping bonding for {} (BondPolicy::Skip)", mac);
        }

        // 7. Player LED pattern (stops the flashing marquee LEDs).
        if let Err(err) = retry_with("player LED set", RETRY_ATTEMPTS, RETRY_DELAY, || {
            controller.set_player_leds(options.player)
        }) {
            warn!("Player LED command: {}", err);
        } else {
            info!("Player LED set to Player {}", options.player);
        }

        // 8. Enable standard input reporting + motion (INIT then ENABLE).
        controller.enable_features(0x03 | FEATURE_MOTION)?;

        // 9. Subscribe to input reports; streaming starts here.
        controller
            .att
            .subscribe(controller.handles.input_report_cccd, true)?;
        info!(
            "Input streaming active (handle 0x{:04X})",
            controller.handles.input_report
        );

        controller.is_ready.store(true, Ordering::Relaxed);

        Ok(controller)
    }

    /// Play a built-in GameCube rumble preset (command `0x0A`, subcommand
    /// `0x02`, data = preset id + three zero bytes).
    ///
    /// **Blocks until the command ACK arrives** (up to 2 s, legacy parity), so
    /// it serialises with memory reads, LED writes, bonding and sleep.
    ///
    /// GameCube pad only: HD-rumble controllers (Pro Controller 2, Joy-Con 2)
    /// use [`Self::write_vibration`] with a
    /// [`crate::protocol::vibration::Sustainer`] instead. There is no stop
    /// subcommand — the controller plays a finite clip, and a held rumble is
    /// emulated by re-issuing the preset (see
    /// [`crate::protocol::vibration::PresetGate`]).
    ///
    /// Deliberately does **not** reproduce legacy's unconditional soft preset
    /// at connect time (analysis §8.4 / deviation 7): connecting a pad must not
    /// rattle it.
    pub fn play_vibration_preset(&self, preset: Preset) -> io::Result<()> {
        self.write_command(
            COMMAND_VIBRATION,
            SUBCOMMAND_VIBRATION_PLAY_PRESET,
            &preset_command_data(preset),
            Duration::from_secs(2),
        )?;
        Ok(())
    }

    /// Write an HD-rumble payload (see [`crate::protocol::vibration`]).
    ///
    /// This is an **ATT Write Command (0x52)**: unacknowledged at the ATT
    /// layer, needs no CCCD subscription, and is deliberately *not* routed
    /// through [`Self::write_command`] (no command ACK is expected). A
    /// `DualBlock` payload is 33 bytes and therefore needs `ATT_MTU >= 36`;
    /// [`Self::connect_with`] already refuses any MTU below 66 for the 63-byte
    /// input reports, so the write always fits here.
    pub fn write_vibration(&self, payload: &[u8]) -> io::Result<()> {
        self.att.write_command(self.handles.vibration, payload)
    }

    /// Send a framed command and await the ACK notification.
    pub fn write_command(
        &self,
        command_id: u8,
        subcommand_id: u8,
        data: &[u8],
        timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        let buf = crate::protocol::constants::build_command(command_id, subcommand_id, data);

        let (lock, cvar) = &*self.cmd_resp_state;
        {
            let mut resp = lock.lock().unwrap();
            *resp = None;
        }

        self.att.write_command(self.handles.command_write, &buf)?;

        let mut resp_guard = lock.lock().unwrap();
        while resp_guard.is_none() {
            let (new_guard, wait_res) = cvar.wait_timeout(resp_guard, timeout).unwrap();
            resp_guard = new_guard;
            if wait_res.timed_out() && resp_guard.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "Command 0x{:02X}/0x{:02X} timed out",
                        command_id, subcommand_id
                    ),
                ));
            }
        }

        let resp = resp_guard.take().unwrap_or_default();
        if resp.len() < 8 || resp[0] != command_id || resp[1] != 0x01 {
            return Err(io::Error::other(format!(
                "Unexpected command response: {:02X?}",
                resp
            )));
        }

        Ok(resp[8..].to_vec())
    }

    /// Read memory from controller address.
    ///
    /// Validates both the echoed length and the echoed address (legacy
    /// `device.py`); a mismatched address means a stale/corrupt reply.
    pub fn read_memory(&self, length: u8, address: u32) -> io::Result<Vec<u8>> {
        if length > MAX_MEMORY_READ_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Memory read length {length} exceeds 0x{MAX_MEMORY_READ_LEN:02X}"),
            ));
        }

        let mut payload = Vec::with_capacity(8);
        payload.push(length);
        payload.extend_from_slice(&[0x7E, 0x00, 0x00]);
        payload.extend_from_slice(&address.to_le_bytes());

        let data = self.write_command(
            COMMAND_MEMORY,
            SUBCOMMAND_MEMORY_READ,
            &payload,
            Duration::from_millis(1500),
        )?;

        if data.len() < 8 || data[0] != length {
            return Err(io::Error::other("Invalid memory read response"));
        }

        // Evidence before validation-by-parsing: what the pad actually sent.
        debug!(
            "memory read addr=0x{address:08X} req_len={length} resp_len={} resp_first8={:02X?}",
            data.len(),
            &data[..data.len().min(8)]
        );

        // `memory_read_payload` re-checks the header (legacy parity) and, like
        // legacy, does NOT require the full requested payload length.
        Ok(memory_read_payload(length, address, &data)?.to_vec())
    }

    pub fn read_controller_info(&self) -> io::Result<ControllerInfo> {
        let mem = self.read_memory(0x40, ADDRESS_CONTROLLER_INFO)?;
        ControllerInfo::from_bytes(&mem)
            .ok_or_else(|| io::Error::other("Failed to parse controller info"))
    }

    /// Read both sticks (always, regardless of PID) and the GC trigger neutral.
    ///
    /// Per-stick failures are logged and fall back to defaults; this never
    /// returns an error so the session survives.
    fn read_calibration(&mut self) {
        self.left_calib = self.read_stick_calibration(
            CALIBRATION_USER_JOYSTICK_1,
            CALIBRATION_JOYSTICK_1,
            "left",
        );
        self.right_calib = self.read_stick_calibration(
            CALIBRATION_USER_JOYSTICK_2,
            CALIBRATION_JOYSTICK_2,
            "right",
        );

        if self.info.as_ref().map(|info| info.product_id) == Some(NSO_GAMECUBE_PID) {
            self.trigger_neutral = self.read_gc_trigger_neutral();
        }
    }

    fn read_stick_calibration(
        &self,
        user_address: u32,
        factory_address: u32,
        stick: &str,
    ) -> StickCalibration {
        let user_result = self.read_memory(CALIBRATION_LEN, user_address);
        crate::protocol::calibration::log_calibration_read(
            stick,
            "user",
            user_address,
            CALIBRATION_LEN,
            &user_result,
        );
        if let Ok(data) = &user_result {
            if let Some(message) =
                calibration_short_read_warning(stick, "user", user_address, data)
            {
                warn!("{message}");
            } else if let Some(message) =
                calibration_short_parseable_note(stick, "user", user_address, data)
            {
                debug!("{message}");
            }
        }
        let user = match &user_result {
            Ok(data) => Some(data.clone()),
            Err(err) => {
                warn!(
                    "Reading {stick} stick user calibration (0x{user_address:08X}) failed: {err}"
                );
                None
            }
        };

        let factory_result = if needs_factory_calibration(user.as_deref()) {
            let result = self.read_memory(CALIBRATION_LEN, factory_address);
            crate::protocol::calibration::log_calibration_read(
                stick,
                "factory",
                factory_address,
                CALIBRATION_LEN,
                &result,
            );
            if let Ok(data) = &result {
                if let Some(message) =
                    calibration_short_read_warning(stick, "factory", factory_address, data)
                {
                    warn!("{message}");
                } else if let Some(message) =
                    calibration_short_parseable_note(stick, "factory", factory_address, data)
                {
                    debug!("{message}");
                }
            }
            Some(result)
        } else {
            None
        };

        let factory = match factory_result.as_ref() {
            Some(Ok(data)) => Some(data.clone()),
            Some(Err(err)) => {
                warn!(
                    "Reading {stick} stick factory calibration \
                     (0x{factory_address:08X}) failed: {err}"
                );
                None
            }
            None => None,
        };

        let (calibration, source) = choose_calibration(user.as_deref(), factory.as_deref());
        match source {
            CalibrationSource::Default => {
                warn!(
                    "{}",
                    default_calibration_warning(
                        stick,
                        user_address,
                        &user_result,
                        factory_address,
                        factory_result.as_ref()
                    )
                );
            }
            _ => debug!("{stick} stick calibration source: {source:?}"),
        }
        calibration
    }

    fn read_gc_trigger_neutral(&self) -> (u8, u8) {
        match self.read_memory(2, CALIBRATION_GC_TRIGGERS) {
            Ok(data) if data.len() >= 2 => {
                (gc_trigger_neutral(data[0]), gc_trigger_neutral(data[1]))
            }
            Ok(data) => {
                warn!(
                    "GC trigger neutral read returned {} bytes; keeping defaults",
                    data.len()
                );
                (GC_TRIGGER_DEFAULT_NEUTRAL, GC_TRIGGER_DEFAULT_NEUTRAL)
            }
            Err(err) => {
                warn!("GC trigger neutral read failed ({err}); keeping defaults");
                (GC_TRIGGER_DEFAULT_NEUTRAL, GC_TRIGGER_DEFAULT_NEUTRAL)
            }
        }
    }

    pub fn set_player_leds(&self, player: u8) -> io::Result<()> {
        let idx = (player as usize).clamp(1, 8);
        let pattern = LED_PATTERNS[idx];
        let payload = [pattern, 0x00, 0x00, 0x00];
        self.write_command(
            COMMAND_LEDS,
            SUBCOMMAND_LEDS_SET_PLAYER,
            &payload,
            Duration::from_millis(1500),
        )?;
        Ok(())
    }

    /// Run feature INIT then ENABLE, each retried like legacy.
    ///
    /// Returns `Err` only when neither command was acknowledged; when one of
    /// the two succeeded the session is usable, so the failure is logged and
    /// swallowed.
    pub fn enable_features(&self, flags: u8) -> io::Result<()> {
        let payload = [flags, 0x00, 0x00, 0x00];

        let init = retry_with("feature init", RETRY_ATTEMPTS, RETRY_DELAY, || {
            self.write_command(
                COMMAND_FEATURE,
                SUBCOMMAND_FEATURE_INIT,
                &payload,
                Duration::from_millis(1000),
            )
            .map(|_| ())
        });
        let enable = retry_with("feature enable", RETRY_ATTEMPTS, RETRY_DELAY, || {
            self.write_command(
                COMMAND_FEATURE,
                SUBCOMMAND_FEATURE_ENABLE,
                &payload,
                Duration::from_millis(1000),
            )
            .map(|_| ())
        });

        match (init, enable) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(init_err), Ok(())) => {
                warn!("Feature init failed: {}", init_err);
                Ok(())
            }
            (Ok(()), Err(enable_err)) => {
                warn!("Feature enable failed: {}", enable_err);
                Ok(())
            }
            (Err(init_err), Err(enable_err)) => {
                warn!("Feature init failed: {}", init_err);
                warn!("Feature enable failed: {}", enable_err);
                Err(io::Error::other(format!(
                    "feature init and enable both failed: {init_err}; {enable_err}"
                )))
            }
        }
    }

    /// Store the host adapter MAC and LTK halves, waiting for every ACK.
    pub fn bond(&self, adapter_le: &[u8; 6]) -> io::Result<()> {
        // SET_MAC carries the host MAC twice (legacy device.py:278).
        let mut mac_payload = vec![0x00, 0x02];
        mac_payload.extend_from_slice(adapter_le);
        mac_payload.extend_from_slice(adapter_le);

        self.write_command(
            COMMAND_PAIR,
            SUBCOMMAND_PAIR_SET_MAC,
            &mac_payload,
            BOND_TIMEOUT,
        )?;
        self.write_command(COMMAND_PAIR, SUBCOMMAND_PAIR_LTK1, &PAIR_LTK1, BOND_TIMEOUT)?;
        self.write_command(COMMAND_PAIR, SUBCOMMAND_PAIR_LTK2, &PAIR_LTK2, BOND_TIMEOUT)?;
        self.write_command(COMMAND_PAIR, SUBCOMMAND_PAIR_FINISH, &[0x00], BOND_TIMEOUT)?;

        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.att.is_connected()
    }

    /// Convert an input report into calibrated sticks and triggers.
    ///
    /// Single source of truth for the conversion described in
    /// `device.py:436-454`; see [`calibrate`] for the per-PID trigger rules.
    pub fn calibrated_input(&self, report: &InputReport) -> CalibratedInput {
        let analog_triggers = self
            .info
            .as_ref()
            .map(|info| has_analog_triggers(info.product_id))
            .unwrap_or(false);
        calibrate(
            &self.left_calib,
            &self.right_calib,
            self.trigger_neutral,
            analog_triggers,
            report,
        )
    }

    /// Ask the pad to sleep, waiting for the correlated ACK before closing.
    pub fn sleep(&self) {
        if let Err(err) = self.write_command(
            COMMAND_HOST,
            SUBCOMMAND_HOST_HCI_STATE,
            &[0x00],
            Duration::from_secs(1),
        ) {
            debug!(
                "Sleep command for {} was not acknowledged: {}",
                self.mac, err
            );
        }
        self.att.close();
    }
}

/// Validate the fixed 8-byte memory-read response header and return the memory
/// bytes.
///
/// Mirrors legacy `device.py:248-250`: only `data[0] == length` and the echoed
/// address are checked. It deliberately does NOT require
/// `data.len() == 8 + length`, so a truncated response passes here and is
/// rejected later by [`StickCalibration::from_bytes`].
fn memory_read_payload<'a>(
    length: u8,
    address: u32,
    data: &'a [u8],
) -> io::Result<&'a [u8]> {
    if data.len() < 8 || data[0] != length {
        return Err(io::Error::other("Invalid memory read response"));
    }

    let echoed_address = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    if echoed_address != address {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Memory read echoed address 0x{echoed_address:08X}, expected 0x{address:08X}"
            ),
        ));
    }

    Ok(&data[8..])
}

/// Uppercase hex preview for short-read diagnostics.
fn hex_bytes(data: &[u8], limit: usize) -> String {
    data.iter()
        .take(limit)
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Warning text for a calibration payload shorter than the 9 bytes consumed by
/// `get_stick_xy`; `None` when the payload is long enough to parse.
fn calibration_short_read_warning(
    stick: &str,
    area: &str,
    address: u32,
    data: &[u8],
) -> Option<String> {
    if data.len() >= CALIBRATION_MIN_PARSE_LEN {
        return None;
    }
    Some(format!(
        "Short {stick} stick {area} calibration read: addr=0x{address:08X} \
         returned {} bytes (need {CALIBRATION_MIN_PARSE_LEN} for three 12-bit pairs), \
         first=[{}]; continuing",
        data.len(),
        hex_bytes(data, 8)
    ))
}

/// Debug text for a payload that is shorter than the requested 0x0B but still
/// has the 9 bytes `get_stick_xy` consumes.
fn calibration_short_parseable_note(
    stick: &str,
    area: &str,
    address: u32,
    data: &[u8],
) -> Option<String> {
    let len = data.len();
    if !(CALIBRATION_MIN_PARSE_LEN..usize::from(CALIBRATION_LEN)).contains(&len) {
        return None;
    }
    Some(format!(
        "Short but parseable {stick} stick {area} calibration read: \
         addr=0x{address:08X} returned {len} bytes; parsing first {CALIBRATION_MIN_PARSE_LEN}"
    ))
}

fn calibration_read_evidence(result: &io::Result<Vec<u8>>) -> String {
    match result {
        Ok(data) => format!("{} bytes", data.len()),
        Err(err) => format!("error: {err}"),
    }
}

/// Final fallback warning: names both areas and why they were rejected, so a
/// default-log user report is actionable without RUST_LOG=debug.
fn default_calibration_warning(
    stick: &str,
    user_address: u32,
    user_result: &io::Result<Vec<u8>>,
    factory_address: u32,
    factory_result: Option<&io::Result<Vec<u8>>>,
) -> String {
    let factory = match factory_result {
        Some(result) => calibration_read_evidence(result),
        None => "not read (user payload did not match FF sentinel)".to_string(),
    };
    format!(
        "Using default {stick} stick calibration: \
         user=0x{user_address:08X} ({}), factory=0x{factory_address:08X} ({factory}), \
         decision=defaults",
        calibration_read_evidence(user_result)
    )
}

/// Whether the user calibration area is absent or still unwritten.
fn needs_factory_calibration(user: Option<&[u8]>) -> bool {
    match user {
        Some(data) if data.len() >= 3 => data[0..3] == [0xFF, 0xFF, 0xFF],
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const VALID_CALIBRATION: [u8; 11] = [
        0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xAA, 0xBB,
    ];
    const FACTORY_CALIBRATION: [u8; 11] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0xCC, 0xDD,
    ];
    const UNWRITTEN: [u8; 11] = [0xFF; 11];

    #[test]
    fn choose_calibration_matrix() {
        let expected_user = StickCalibration::from_bytes(&VALID_CALIBRATION).expect("valid user");
        let expected_factory =
            StickCalibration::from_bytes(&FACTORY_CALIBRATION).expect("valid factory");

        // Valid user data wins.
        assert_eq!(
            choose_calibration(Some(&VALID_CALIBRATION), Some(&FACTORY_CALIBRATION)),
            (expected_user, CalibrationSource::User)
        );

        // Unwritten user area falls back to factory.
        assert_eq!(
            choose_calibration(Some(&UNWRITTEN), Some(&FACTORY_CALIBRATION)),
            (expected_factory, CalibrationSource::Factory)
        );

        // Both unusable: default calibration, no session teardown.
        assert_eq!(
            choose_calibration(Some(&UNWRITTEN), Some(&[0u8; 5])),
            (StickCalibration::default(), CalibrationSource::Default)
        );
        assert_eq!(
            choose_calibration(None, None),
            (StickCalibration::default(), CalibrationSource::Default)
        );

        // A user read shorter than the 9-byte layout is unusable.
        assert_eq!(
            choose_calibration(Some(&VALID_CALIBRATION[..8]), Some(&FACTORY_CALIBRATION)),
            (expected_factory, CalibrationSource::Factory)
        );
    }

    #[test]
    fn gc_trigger_neutral_rejects_sentinels() {
        assert_eq!(gc_trigger_neutral(0x00), GC_TRIGGER_DEFAULT_NEUTRAL);
        assert_eq!(gc_trigger_neutral(0xFF), GC_TRIGGER_DEFAULT_NEUTRAL);
        assert_eq!(gc_trigger_neutral(30), 30);
        assert_eq!(gc_trigger_neutral(137), 137);
    }

    fn sample_report(
        buttons: u32,
        left_stick: (u16, u16),
        right_stick: (u16, u16),
        left_trigger: u8,
        right_trigger: u8,
    ) -> InputReport {
        InputReport {
            timestamp: 0,
            buttons,
            left_stick_raw: left_stick,
            right_stick_raw: right_stick,
            battery_mv: 0,
            accel: (0, 0, 0),
            gyro: (0, 0, 0),
            left_trigger_raw: left_trigger,
            right_trigger_raw: right_trigger,
        }
    }

    #[test]
    fn calibrated_sticks_scale_to_unit_range() {
        let calib = StickCalibration::default();
        let centre = sample_report(0, (2048, 2048), (2048, 2048), 0, 0);
        let out = calibrate(&calib, &calib, (30, 30), false, &centre);
        assert_eq!(out.left, (0.0, 0.0));
        assert_eq!(out.right, (0.0, 0.0));

        // Default calibration: center 2048, max/min delta 1700.
        let extremes = sample_report(0, (3748, 348), (348, 3748), 0, 0);
        let out = calibrate(&calib, &calib, (30, 30), false, &extremes);
        assert!((out.left.0 - 1.0).abs() < 1e-6, "left x: {}", out.left.0);
        assert!((out.left.1 + 1.0).abs() < 1e-6, "left y: {}", out.left.1);
        assert!((out.right.0 + 1.0).abs() < 1e-6, "right x: {}", out.right.0);
        assert!((out.right.1 - 1.0).abs() < 1e-6, "right y: {}", out.right.1);
    }

    #[test]
    fn calibrated_ignores_values_inside_the_deadzone() {
        let calib = StickCalibration::default();
        // +120 and -120 are still inside STICK_DEADZONE (strict comparisons).
        let inside = sample_report(0, (2168, 1928), (2148, 1948), 0, 0);
        let out = calibrate(&calib, &calib, (30, 30), false, &inside);
        assert_eq!(out.left, (0.0, 0.0));
        assert_eq!(out.right, (0.0, 0.0));
    }

    #[test]
    fn calibrated_digital_lr_forces_full_trigger() {
        let calib = StickCalibration::default();
        // Analog branch would normalize raw 0 against neutral 30 to 0; the L/R
        // click must force the axis to full scale (device.py:443-447).
        let left = sample_report(button_mask::L | button_mask::ZL, (2048, 2048), (2048, 2048), 0, 0);
        let out = calibrate(&calib, &calib, (30, 30), true, &left);
        assert_eq!(out.lt, 255);
        assert_eq!(out.rt, 0);

        let right = sample_report(button_mask::R | button_mask::ZR, (2048, 2048), (2048, 2048), 0, 0);
        let out = calibrate(&calib, &calib, (30, 30), true, &right);
        assert_eq!(out.lt, 0);
        assert_eq!(out.rt, 255);
    }

    #[test]
    fn calibrated_gc_pid_uses_analog_trigger_branch() {
        assert!(has_analog_triggers(NSO_GAMECUBE_PID));
        assert!(!has_analog_triggers(
            crate::protocol::constants::PRO_CONTROLLER2_PID
        ));

        let calib = StickCalibration::default();
        // ZL/ZR set is irrelevant in the analog branch: raw values decide.
        let report = sample_report(
            button_mask::ZL | button_mask::ZR,
            (2048, 2048),
            (2048, 2048),
            255,
            30,
        );
        let out = calibrate(
            &calib,
            &calib,
            (30, 30),
            has_analog_triggers(NSO_GAMECUBE_PID),
            &report,
        );
        assert_eq!(out.lt, 255);
        assert_eq!(out.rt, 0);
    }

    #[test]
    fn calibrated_pro_pid_maps_zl_zr_to_0_or_255() {
        let pid = crate::protocol::constants::PRO_CONTROLLER2_PID;
        assert!(!has_analog_triggers(pid));
        let calib = StickCalibration::default();

        // Raw trigger bytes are ignored on non-analog pads.
        let pressed = sample_report(button_mask::ZL | button_mask::ZR, (2048, 2048), (2048, 2048), 7, 9);
        let out = calibrate(&calib, &calib, (30, 30), has_analog_triggers(pid), &pressed);
        assert_eq!((out.lt, out.rt), (255, 255));

        let released = sample_report(0, (2048, 2048), (2048, 2048), 255, 255);
        let out = calibrate(&calib, &calib, (30, 30), has_analog_triggers(pid), &released);
        assert_eq!((out.lt, out.rt), (0, 0));
    }

    #[test]
    fn retry_succeeds_first_try() {
        let calls = AtomicUsize::new(0);
        let value = retry_with("first try op", 3, Duration::ZERO, || {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(42u8)
        })
        .expect("first attempt succeeds");
        assert_eq!(value, 42);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn retry_recovers_on_third_attempt() {
        let calls = AtomicUsize::new(0);
        let value = retry_with("flaky op", 3, Duration::ZERO, || {
            let attempt = calls.fetch_add(1, Ordering::Relaxed) + 1;
            if attempt < 3 {
                Err(io::Error::new(io::ErrorKind::TimedOut, "transient"))
            } else {
                Ok(7u8)
            }
        })
        .expect("third attempt succeeds");
        assert_eq!(value, 7);
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn retry_exhausts_after_three() {
        let calls = AtomicUsize::new(0);
        let err = retry_with("always failing", 3, Duration::ZERO, || -> io::Result<()> {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(io::Error::new(io::ErrorKind::TimedOut, "no reply"))
        })
        .expect_err("all attempts fail");
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let message = err.to_string();
        assert!(message.contains("always failing"), "message: {message}");
        assert!(message.contains('3'), "message: {message}");
    }

    #[test]
    fn bond_policy_decides_correctly() {
        assert!(!should_bond(BondPolicy::Skip, false));
        assert!(!should_bond(BondPolicy::Skip, true));
        assert!(should_bond(BondPolicy::Force, false));
        assert!(should_bond(BondPolicy::Force, true));
        assert!(should_bond(BondPolicy::IfNeeded, false));
        assert!(!should_bond(BondPolicy::IfNeeded, true));
    }

    #[test]
    fn budget_narrows_with_rounds() {
        let budget = DialBudget::default();
        assert_eq!(budget.rounds, 16, "legacy _CONNECT_ATTEMPTS");
        assert_eq!(budget.per_type_timeout, Duration::from_millis(800));
        assert_eq!(budget.between_rounds, Duration::from_millis(20));
        assert!(budget.remove_bluez_device);

        // Rounds shrink monotonically to zero and then stop.
        let left: Vec<u32> = (0..16).filter_map(|r| budget.round(r)).map(|p| p.rounds_left).collect();
        assert_eq!(left, (0..16).rev().collect::<Vec<u32>>());
        assert_eq!(budget.round(15).unwrap().pause_after, Duration::ZERO);
        assert_eq!(budget.round(14).unwrap().pause_after, budget.between_rounds);
        assert!(budget.round(16).is_none());
        assert!(budget.round(99).is_none());

        // Worst case: 16 * 2 * 0.8 s + 15 * 20 ms = 25.9 s.
        let expected = Duration::from_millis(16 * 2 * 800 + 15 * 20);
        assert_eq!(budget.worst_case(), expected);

        // A shrunken budget (tests / diagnostics) keeps the same shape.
        let tiny = DialBudget {
            rounds: 3,
            per_type_timeout: Duration::from_millis(1),
            between_rounds: Duration::ZERO,
            remove_bluez_device: false,
        };
        assert_eq!(tiny.worst_case(), Duration::from_millis(6));
        assert_eq!(tiny.round(2).unwrap().rounds_left, 0);
    }

    #[derive(Default)]
    struct FakeControl {
        prepare_calls: std::cell::Cell<usize>,
        stop_calls: std::cell::Cell<usize>,
        removals: std::cell::Cell<usize>,
        /// Ordered event names, so ordering can be asserted too.
        events: std::cell::RefCell<Vec<String>>,
    }

    impl DialControl for FakeControl {
        fn prepare_global(&self) -> io::Result<()> {
            self.prepare_calls.set(self.prepare_calls.get() + 1);
            self.events.borrow_mut().push("prepare".to_string());
            Ok(())
        }

        fn stop_le_scan(&self, force: bool) -> io::Result<()> {
            self.stop_calls.set(self.stop_calls.get() + 1);
            self.events.borrow_mut().push(format!("stop(force={force})"));
            Ok(())
        }

        fn remove_bluez_device(&self, mac: &str) -> io::Result<()> {
            self.removals.set(self.removals.get() + 1);
            self.events.borrow_mut().push(format!("remove({mac})"));
            Ok(())
        }
    }

    fn tiny_budget(rounds: u32) -> DialBudget {
        DialBudget {
            rounds,
            per_type_timeout: Duration::from_millis(1),
            between_rounds: Duration::ZERO,
            remove_bluez_device: true,
        }
    }

    #[test]
    fn connect_session_gives_up_after_budget() {
        let control = FakeControl::default();
        let mut calls = 0;

        let (result, report) = dial_with_budget(
            &control,
            tiny_budget(3),
            "AA:BB:CC:DD:EE:FF",
            &mut |_per_type: Duration| {
                calls += 1;
                Err::<u32, _>(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "L2CAP connect failed (LE_PUBLIC: timed out; LE_RANDOM: timed out)",
                ))
            },
        );

        assert!(result.is_err(), "exhausted budget must report an error");
        assert_eq!(calls, 3, "one dial per round");
        assert_eq!(report.rounds_attempted, 3);
        assert_eq!(report.attempts, 6, "both address types per round");
        assert!(report
            .last_error
            .as_deref()
            .unwrap()
            .contains("LE_PUBLIC: timed out"));
        assert_eq!(report.discovery_stop, "ok");
        assert_eq!(report.device_removal, "ok");

        // One forced global stop, one removal, one rate-limited stop per round.
        assert_eq!(control.prepare_calls.get(), 1);
        assert_eq!(control.removals.get(), 1);
        assert_eq!(control.stop_calls.get(), 3);
    }

    #[test]
    fn connect_session_stops_at_first_success() {
        let control = FakeControl::default();
        let mut calls = 0;

        let (result, report) = dial_with_budget(
            &control,
            tiny_budget(16),
            "AA:BB:CC:DD:EE:FF",
            &mut |_per_type: Duration| {
                calls += 1;
                if calls < 3 {
                    Err(io::Error::new(io::ErrorKind::TimedOut, "not yet"))
                } else {
                    Ok(calls)
                }
            },
        );

        assert_eq!(result.unwrap(), 3, "returns the session from round 3");
        assert_eq!(calls, 3, "no dialling after success");
        assert_eq!(report.rounds_attempted, 3);
        assert_eq!(report.attempts, 6);
        assert_eq!(report.last_error.as_deref(), Some("not yet"));
        assert_eq!(control.stop_calls.get(), 3);
        assert_eq!(control.removals.get(), 1);
    }

    #[test]
    fn remove_bluez_device_is_called_once_before_rounds_not_per_round() {
        let control = FakeControl::default();
        let mut dial_calls = 0;

        let _ = dial_with_budget(
            &control,
            tiny_budget(4),
            "BC:89:A6:45:83:CC",
            &mut |_per_type: Duration| -> io::Result<()> {
                dial_calls += 1;
                control.events.borrow_mut().push(format!("dial#{dial_calls}"));
                Err(io::Error::new(io::ErrorKind::TimedOut, "nope"))
            },
        );

        assert_eq!(control.removals.get(), 1, "exactly one bluetoothctl remove");
        let events = control.events.borrow();
        assert_eq!(events[0], "prepare", "global stop comes first");
        assert_eq!(events[1], "remove(BC:89:A6:45:83:CC)", "removal before dialling");
        assert_eq!(events[2], "stop(force=false)", "scan stop before round 1");
        assert_eq!(events[3], "dial#1");
        // Removals never repeat between rounds.
        assert_eq!(
            events.iter().filter(|e| e.starts_with("remove(")).count(),
            1
        );

        // A live session disables the removal entirely.
        let control = FakeControl::default();
        let mut budget = tiny_budget(2);
        budget.remove_bluez_device = false;
        let (result, report) = dial_with_budget(&control, budget, "AA:BB:CC:DD:EE:FF", &mut |_| {
            Err::<(), _>(io::Error::new(io::ErrorKind::TimedOut, "nope"))
        });
        assert!(result.is_err());
        assert_eq!(control.removals.get(), 0, "never remove under a live session");
        assert_eq!(report.device_removal, "skipped (live session)");
    }

    #[test]
    fn memory_read_header_validation_rejects_bad_length_and_address() {
        let address = 0x001F_C062u32;
        let mut response = vec![CALIBRATION_LEN, 0x7E, 0x00, 0x00];
        response.extend_from_slice(&address.to_le_bytes());
        response.extend_from_slice(&[0u8; 11]);
        assert_eq!(
            memory_read_payload(CALIBRATION_LEN, address, &response)
                .expect("header parses")
                .len(),
            11
        );

        let mut wrong_len = response.clone();
        wrong_len[0] = CALIBRATION_LEN - 1;
        assert!(memory_read_payload(CALIBRATION_LEN, address, &wrong_len).is_err());

        let mut wrong_address = response.clone();
        wrong_address[4..8].copy_from_slice(&0x001F_C042u32.to_le_bytes());
        assert!(memory_read_payload(CALIBRATION_LEN, address, &wrong_address).is_err());

        assert!(memory_read_payload(CALIBRATION_LEN, address, &response[..7]).is_err());
    }

    #[test]
    fn memory_read_header_validation_accepts_truncated_payload() {
        // Pins the audit finding: a response with a valid 8-byte header but
        // only 3 of the 11 requested memory bytes passes read_memory and
        // becomes a 3-byte Vec, which StickCalibration::from_bytes rejects.
        let address = 0x001F_C062u32;
        let mut response = vec![CALIBRATION_LEN, 0x7E, 0x00, 0x00];
        response.extend_from_slice(&address.to_le_bytes());
        response.extend_from_slice(&[0xAA, 0xBB, 0xCC]);

        let payload = memory_read_payload(CALIBRATION_LEN, address, &response)
            .expect("header checks pass");
        assert_eq!(payload.len(), 3, "requested length is not enforced");
        assert!(StickCalibration::from_bytes(payload).is_none());

        // The 3-byte case must now take the loud short-read path.
        let warning = calibration_short_read_warning("right", "user", address, payload)
            .expect("3-byte payload warns");
        assert!(warning.contains("returned 3 bytes"), "warning: {warning}");
        assert!(warning.contains("first=[AA BB CC]"), "warning: {warning}");
    }

    #[test]
    fn calibration_length_policy_9_bytes_parses_without_short_warning() {
        let nine = [0x00, 0x08, 0x80, 0xA4, 0x46, 0x6A, 0xA4, 0x46, 0x6A];
        let address = 0x001F_C062u32;

        assert!(StickCalibration::from_bytes(&nine).is_some());
        assert_eq!(
            calibration_short_read_warning("right", "user", address, &nine),
            None,
            "9 bytes are enough to parse; no short-read warning"
        );
        let note = calibration_short_parseable_note("right", "user", address, &nine)
            .expect("9..10 byte debug note");
        assert!(note.contains("returned 9 bytes"), "note: {note}");
    }

    #[test]
    fn calibration_length_policy_8_bytes_warns_and_falls_back() {
        let eight = [0x00, 0x08, 0x80, 0xA4, 0x46, 0x6A, 0xA4, 0x46];
        let address = 0x001F_C062u32;

        let warning = calibration_short_read_warning("right", "user", address, &eight)
            .expect("8-byte payload warns");
        assert!(warning.contains("addr=0x001FC062"), "warning: {warning}");
        assert!(warning.contains("returned 8 bytes"), "warning: {warning}");
        assert!(warning.contains("need 9"), "warning: {warning}");
        assert!(
            warning.contains("first=[00 08 80 A4 46 6A A4 46]"),
            "warning: {warning}"
        );
        assert_eq!(
            calibration_short_parseable_note("right", "user", address, &eight),
            None,
            "8 bytes is not parseable"
        );

        assert!(StickCalibration::from_bytes(&eight).is_none());
        assert_eq!(
            choose_calibration(Some(&eight), None),
            (StickCalibration::default(), CalibrationSource::Default)
        );
    }

    #[test]
    fn default_calibration_warning_names_both_areas() {
        let user_address = 0x001F_C062u32;
        let factory_address = 0x0001_30E8u32;
        let user = Ok(vec![0xFF; 11]);
        let factory = Ok(Vec::new());

        let message = default_calibration_warning(
            "right",
            user_address,
            &user,
            factory_address,
            Some(&factory),
        );
        assert_eq!(
            message,
            "Using default right stick calibration: user=0x001FC062 (11 bytes), \
             factory=0x000130E8 (0 bytes), decision=defaults"
        );
        assert!(message.contains("decision=defaults"), "message: {message}");

        // Factory not attempted: the warning still says why.
        let skipped = default_calibration_warning(
            "right",
            user_address,
            &Ok(vec![0x00, 0x08, 0x80]),
            factory_address,
            None,
        );
        assert!(
            skipped.contains("factory=0x000130E8 (not read (user payload did not match FF sentinel))"),
            "message: {skipped}"
        );
    }

    #[test]
    fn short_calibration_warning_includes_evidence() {
        let message = calibration_short_read_warning(
            "right",
            "user",
            0x001F_C062,
            &[0xAA, 0xBB, 0xCC],
        )
        .expect("3-byte payload warns");
        assert!(message.contains("Short right stick user calibration read"));
        assert!(message.contains("addr=0x001FC062"));
        assert!(message.contains("returned 3 bytes"));
        assert!(message.contains("first=[AA BB CC]"));
        assert!(message.contains("need 9"));
        assert!(message.contains("continuing"));
    }

    #[test]
    fn needs_factory_calibration_is_per_stick() {
        // Left and right each call this with their own user result, so one
        // side can never short-circuit the other.
        assert!(needs_factory_calibration(None));
        assert!(needs_factory_calibration(Some(&[])));
        assert!(needs_factory_calibration(Some(&[0x00, 0x08])));
        assert!(needs_factory_calibration(Some(&[
            0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
        ])));
        assert!(!needs_factory_calibration(Some(&[
            0x00, 0x08, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
        ])));
    }
}
