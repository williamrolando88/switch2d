//! High-level controller abstraction managing handshake, calibration, and input streaming.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::bluetooth::att::{AttClient, ResolvedHandles};
use crate::bluetooth::l2cap::L2capSocket;
use crate::protocol::calibration::{StickCalibration, GC_TRIGGER_DEFAULT_NEUTRAL};
use crate::protocol::command::{mac_to_le, ControllerInfo};
use crate::protocol::constants::*;
use crate::protocol::report::InputReport;

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
        let dest_le = mac_to_le(mac)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid controller MAC"))?;
        let adapter_le = mac_to_le(adapter_mac)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid adapter MAC"))?;

        info!("Connecting to Switch 2 controller {}...", mac);
        let socket = L2capSocket::create()?;
        socket.bind(adapter_le)?;

        // Attempt connect (tries LE_PUBLIC then LE_RANDOM)
        let addr_type = socket.connect_le(dest_le, Duration::from_secs(8))?;
        info!("L2CAP link established with {} (type: {})", mac, addr_type);

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

        let echoed_address = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        if echoed_address != address {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Memory read echoed address 0x{echoed_address:08X}, expected 0x{address:08X}"
                ),
            ));
        }

        Ok(data[8..].to_vec())
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
        let user = match self.read_memory(CALIBRATION_LEN, user_address) {
            Ok(data) => Some(data),
            Err(err) => {
                warn!(
                    "Reading {stick} stick user calibration (0x{user_address:08X}) failed: {err}"
                );
                None
            }
        };

        let factory = if needs_factory_calibration(user.as_deref()) {
            match self.read_memory(CALIBRATION_LEN, factory_address) {
                Ok(data) => Some(data),
                Err(err) => {
                    warn!(
                        "Reading {stick} stick factory calibration \
                         (0x{factory_address:08X}) failed: {err}"
                    );
                    None
                }
            }
        } else {
            None
        };

        let (calibration, source) = choose_calibration(user.as_deref(), factory.as_deref());
        match source {
            CalibrationSource::Default => {
                warn!("Using default {stick} stick calibration (controller reports none)");
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
}
