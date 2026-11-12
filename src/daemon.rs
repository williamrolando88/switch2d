//! Central daemon orchestrating multiple controller connections and virtual gamepad lifecycle.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::{DaemonConfig, DaemonMode};
use crate::controller::{BondPolicy, ConnectOptions, DialBudget, SwitchController};
use crate::protocol::calibration::StickCalibration;
use crate::protocol::constants::*;
use crate::uinput::{CombinedJoyConManager, VirtualGamepad};

/// The calibration a live session actually read, captured once when the
/// session is established.
///
/// `SwitchController::left_calib` / `right_calib` are calibration *blocks*, not
/// "the left half of the pad": a right Joy-Con keeps its stick calibration in
/// `right_calib` (falling back to the left block inside the controller), so the
/// callback has to pick the block belonging to the side it is updating. Feeding
/// `StickCalibration::default()` here was the round-6 bug: it capped every
/// stick at the default 0.74 of full scale even though the session had the real
/// calibration.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SessionCalibration {
    left: StickCalibration,
    right: StickCalibration,
}

impl SessionCalibration {
    /// Capture the calibration the controller read during this connect.
    pub(crate) fn capture(controller: &SwitchController) -> Self {
        Self {
            left: controller.left_calib,
            right: controller.right_calib,
        }
    }

    /// Calibration for the side being updated (`is_left` = this pad is a left
    /// Joy-Con). Shared by combined and split mode.
    pub(crate) fn for_side(&self, is_left: bool) -> &StickCalibration {
        if is_left {
            &self.left
        } else {
            &self.right
        }
    }
}

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

    /// Run the daemon event loop until cancelled.
    pub async fn run(&self) -> std::io::Result<()> {
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

        // MACs with a session we currently own. The dial envelope must never
        // remove the BlueZ Device object for these: `bluetoothctl remove` tears
        // the live link down (analysis 8.7 / legacy bridge.py:136-154).
        let live_sessions: Arc<StdMutex<HashSet<String>>> = Arc::new(StdMutex::new(HashSet::new()));

        match self.config.mode {
            DaemonMode::Combined => self.run_combined(live_sessions).await,
            DaemonMode::Split => self.run_split(live_sessions).await,
        }
    }

    async fn run_combined(
        &self,
        live_sessions: Arc<StdMutex<HashSet<String>>>,
    ) -> std::io::Result<()> {
        // Create a single unified Virtual Gamepad for the combined Joy-Cons
        let combined_gamepad = Arc::new(VirtualGamepad::new(
            "Nintendo Switch 2 Combined Joy-Cons",
            PRO_CONTROLLER2_PID,
            "switch2-combined",
        )?);

        let manager = Arc::new(CombinedJoyConManager::new(combined_gamepad));
        let connect_lock = Arc::new(Mutex::new(()));
        let mut handles = Vec::new();

        for ctrl_cfg in &self.config.controllers {
            let ctrl_cfg = ctrl_cfg.clone();
            let adapter_mac = self.config.adapter_mac.clone();
            let running = self.running.clone();
            let mgr = manager.clone();
            let conn_lock = connect_lock.clone();
            let live_sessions = Arc::clone(&live_sessions);

            let handle = tokio::spawn(async move {
                let is_left = ctrl_cfg.name.to_lowercase().contains("left");

                while running.load(Ordering::Relaxed) {
                    let mgr_cb = mgr.clone();
                    let mac_str = ctrl_cfg.mac.clone();
                    let adapter_str = adapter_mac.clone();
                    // Filled as soon as the session is established; the callback
                    // only reads it (no I/O, no locks), so the ATT reader is
                    // never blocked (analysis 5.6).
                    let session_calib: Arc<OnceLock<SessionCalibration>> =
                        Arc::new(OnceLock::new());
                    let calib_for_cb = Arc::clone(&session_calib);

                    // Serialize connection attempts so hardware adapter does not hit EBUSY
                    let conn_permit = conn_lock.lock().await;

                    let live_for_dial = Arc::clone(&live_sessions);
                    let res = tokio::task::spawn_blocking(move || {
                        let is_l = is_left;
                        let mgr_for_cb = mgr_cb.clone();
                        // Never drop the BlueZ Device object for a MAC we own a
                        // session for: the removal kills the live link.
                        let has_live_session = live_for_dial
                            .lock()
                            .map(|live| live.contains(&mac_str))
                            .unwrap_or(false);
                        let budget = DialBudget {
                            remove_bluez_device: !has_live_session,
                            ..DialBudget::default()
                        };
                        // `connect_session` stops discovery before every round,
                        // drops the BlueZ Device object once up front, and uses
                        // the legacy 16 x 0.8 s envelope (analysis 8.7/8.8).
                        SwitchController::connect_session(
                            &mac_str,
                            &adapter_str,
                            ConnectOptions {
                                player: ctrl_cfg.player,
                                // Already-bonded pads only need a wake; unbonded
                                // ones must store host MAC + LTK on this connect.
                                bond: if ctrl_cfg.bonded {
                                    BondPolicy::Skip
                                } else {
                                    BondPolicy::IfNeeded
                                },
                            },
                            Box::new(move |report| {
                                // A report can beat the capture by a few frames
                                // (is_ready is set just before connect returns);
                                // skipping those is better than emitting
                                // default-calibrated values.
                                let Some(calib) = calib_for_cb.get() else {
                                    return;
                                };
                                if is_l {
                                    mgr_for_cb.update_left(report, calib.for_side(true));
                                } else {
                                    mgr_for_cb.update_right(report, calib.for_side(false));
                                }
                            }),
                            budget,
                        )
                    }).await;

                    // Release connect lock regardless of success or failure
                    drop(conn_permit);

                    match res {
                        Ok(Ok(controller)) => {
                            // Capture before the 500 ms health poll so the
                            // callback switches from "skip" to real values now.
                            let _ = session_calib.set(SessionCalibration::capture(&controller));
                            info!("Controller {} connected and streaming inputs!", ctrl_cfg.mac);
                            if let Ok(mut live) = live_sessions.lock() {
                                live.insert(ctrl_cfg.mac.clone());
                            }
                            while controller.is_connected() && running.load(Ordering::Relaxed) {
                                sleep(Duration::from_millis(500)).await;
                            }
                            if let Ok(mut live) = live_sessions.lock() {
                                live.remove(&ctrl_cfg.mac);
                            }
                            warn!("Controller {} disconnected.", ctrl_cfg.mac);
                        }
                        Ok(Err(e)) => {
                            log_connect_failure(&ctrl_cfg.mac, &e);
                            sleep(Duration::from_millis(1500)).await;
                        }
                        Err(e) => {
                            error!("Spawn task error: {}", e);
                            sleep(Duration::from_millis(1500)).await;
                        }
                    }
                }
            });

            handles.push(handle);
        }

        for h in handles {
            let _ = h.await;
        }

        Ok(())
    }

    async fn run_split(
        &self,
        live_sessions: Arc<StdMutex<HashSet<String>>>,
    ) -> std::io::Result<()> {
        let mut handles = Vec::new();
        let connect_lock = Arc::new(Mutex::new(()));

        for ctrl_cfg in &self.config.controllers {
            let ctrl_cfg = ctrl_cfg.clone();
            let adapter_mac = self.config.adapter_mac.clone();
            let running = self.running.clone();
            let conn_lock = connect_lock.clone();
            let live_sessions = Arc::clone(&live_sessions);

            let handle = tokio::spawn(async move {
                let gp_name = format!("Nintendo Switch 2 {}", ctrl_cfg.name);
                let gamepad = match VirtualGamepad::new(&gp_name, ctrl_cfg.product_id, &ctrl_cfg.mac) {
                    Ok(gp) => Arc::new(gp),
                    Err(e) => {
                        error!("Failed to create virtual gamepad: {}", e);
                        return;
                    }
                };

                let manager = Arc::new(CombinedJoyConManager::new(gamepad));

                while running.load(Ordering::Relaxed) {
                    let mgr_cb = manager.clone();
                    let mac_str = ctrl_cfg.mac.clone();
                    let adapter_str = adapter_mac.clone();
                    let is_left = ctrl_cfg.name.to_lowercase().contains("left");
                    // Same session calibration capture as combined mode.
                    let session_calib: Arc<OnceLock<SessionCalibration>> =
                        Arc::new(OnceLock::new());
                    let calib_for_cb = Arc::clone(&session_calib);

                    let conn_permit = conn_lock.lock().await;

                    let live_for_dial = Arc::clone(&live_sessions);
                    let res = tokio::task::spawn_blocking(move || {
                        let is_l = is_left;
                        let mgr_for_cb = mgr_cb.clone();
                        // Same guard as combined mode: no Device-object removal
                        // while we own a live session for this MAC.
                        let has_live_session = live_for_dial
                            .lock()
                            .map(|live| live.contains(&mac_str))
                            .unwrap_or(false);
                        let budget = DialBudget {
                            remove_bluez_device: !has_live_session,
                            ..DialBudget::default()
                        };
                        SwitchController::connect_session(
                            &mac_str,
                            &adapter_str,
                            ConnectOptions {
                                player: ctrl_cfg.player,
                                bond: if ctrl_cfg.bonded {
                                    BondPolicy::Skip
                                } else {
                                    BondPolicy::IfNeeded
                                },
                            },
                            Box::new(move |report| {
                                let Some(calib) = calib_for_cb.get() else {
                                    return;
                                };
                                if is_l {
                                    mgr_for_cb.update_horizontal_left(report, calib.for_side(true));
                                } else {
                                    mgr_for_cb
                                        .update_horizontal_right(report, calib.for_side(false));
                                }
                            }),
                            budget,
                        )
                    }).await;

                    drop(conn_permit);

                    match res {
                        Ok(Ok(controller)) => {
                            let _ = session_calib.set(SessionCalibration::capture(&controller));
                            info!("Controller {} connected in split mode!", ctrl_cfg.mac);
                            if let Ok(mut live) = live_sessions.lock() {
                                live.insert(ctrl_cfg.mac.clone());
                            }
                            while controller.is_connected() && running.load(Ordering::Relaxed) {
                                sleep(Duration::from_millis(500)).await;
                            }
                            if let Ok(mut live) = live_sessions.lock() {
                                live.remove(&ctrl_cfg.mac);
                            }
                            warn!("Controller {} disconnected.", ctrl_cfg.mac);
                        }
                        Ok(Err(e)) => {
                            log_connect_failure(&ctrl_cfg.mac, &e);
                            sleep(Duration::from_millis(1500)).await;
                        }
                        Err(e) => {
                            error!("Spawn task error: {}", e);
                            sleep(Duration::from_millis(1500)).await;
                        }
                    }
                }
            });

            handles.push(handle);
        }

        for h in handles {
            let _ = h.await;
        }

        Ok(())
    }
}

/// Log a failed connect attempt: the first failure for a pad is a warning with
/// the full diagnostic (MAC, both address types, both OS errors, discovery
/// outcome); later retries drop to debug so a parked/unreachable pad does not
/// spam the log.
fn log_connect_failure(mac: &str, err: &std::io::Error) {
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
    // Cheap per-MAC slot: last hex digit of the MAC.
    let slot = mac.chars().last().and_then(|c| c.to_digit(16)).unwrap_or(0);
    let slot = (slot as usize) % WARNED.len();

    if !WARNED[slot].swap(true, Ordering::Relaxed) {
        warn!("Connect to {mac} failed: {err}");
    } else {
        tracing::debug!("Connect to {mac} failed (retrying): {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Realistic single-stick calibration block: centre 2048, max/min delta
    /// 1254. The same shape the controller tests use; raw 3302 is a hard right
    /// deflection that this block maps to full scale.
    const REAL_STICK_CALIBRATION: [u8; 11] = [
        0x00, 0x08, 0x80, 0xE6, 0x64, 0x4E, 0xE6, 0x64, 0x4E, 0x00, 0x00,
    ];

    /// Deadzone the virtual-pad mapper applies (CombinedJoyConManager calls
    /// `apply_scaled_i16(raw, 120)` for every stick update).
    const MAPPER_DEADZONE: i32 = 120;

    #[test]
    fn daemon_feeds_session_calibration_not_defaults() {
        let real = StickCalibration::from_bytes(&REAL_STICK_CALIBRATION).expect("block parses");
        let default = StickCalibration::default();
        // Hard right deflection: (3302 - 2048) is exactly this block's max delta.
        let raw = (3302, 2048);

        // A right Joy-Con keeps its calibration in the right block (left stays
        // default), so the daemon must feed the right block for that side.
        let right_pad = SessionCalibration {
            left: default,
            right: real,
        };
        assert_eq!(*right_pad.for_side(false), real);
        assert_eq!(
            right_pad
                .for_side(false)
                .apply_scaled_i16(raw, MAPPER_DEADZONE),
            (32767, 0)
        );

        // A left Joy-Con is the mirror image.
        let left_pad = SessionCalibration {
            left: real,
            right: default,
        };
        assert_eq!(*left_pad.for_side(true), real);
        assert_eq!(
            left_pad
                .for_side(true)
                .apply_scaled_i16(raw, MAPPER_DEADZONE),
            (32767, 0)
        );

        // The round-6 bug: defaults cap the same sample at
        // (3302 - 2048) / 1700 = 0.7376 of the i16 range.
        assert_eq!(
            default.apply_scaled_i16(raw, MAPPER_DEADZONE),
            (24170, 0),
            "default calibration caps at ~0.74 (the observed evtest peak)"
        );

        // And the daemon must never pick the other side's block.
        assert_ne!(*right_pad.for_side(false), default);
        assert_eq!(*right_pad.for_side(true), default);
    }
}
