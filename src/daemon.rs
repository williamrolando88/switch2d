//! Central daemon orchestrating multiple controller connections and virtual gamepad lifecycle.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::bluetooth::scan::{ScanControl, ShellScanControl};
use crate::config::{DaemonConfig, DaemonMode};
use crate::controller::{BondPolicy, ConnectOptions, SwitchController};
use crate::protocol::constants::*;
use crate::uinput::{CombinedJoyConManager, VirtualGamepad};

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

        match self.config.mode {
            DaemonMode::Combined => self.run_combined().await,
            DaemonMode::Split => self.run_split().await,
        }
    }

    async fn run_combined(&self) -> std::io::Result<()> {
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

            let handle = tokio::spawn(async move {
                let is_left = ctrl_cfg.name.to_lowercase().contains("left");

                while running.load(Ordering::Relaxed) {
                    let mgr_cb = mgr.clone();
                    let mac_str = ctrl_cfg.mac.clone();
                    let adapter_str = adapter_mac.clone();

                    // Serialize connection attempts so hardware adapter does not hit EBUSY
                    let conn_permit = conn_lock.lock().await;

                    let res = tokio::task::spawn_blocking(move || {
                        let is_l = is_left;
                        let mgr_for_cb = mgr_cb.clone();
                        // HANDOVER Bug 3: stop discovery *per attempt*, not once
                        // at startup — another app (decky, a stray bluetoothctl,
                        // BlueZ itself) can restart scanning at any time, and a
                        // live LE scan makes create-connection fail (ENOSYS).
                        // Task 09B moves this into the hub.
                        let discovery_note = stop_discovery_for_connect(&mac_str);
                        SwitchController::connect_with(
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
                                if is_l {
                                    mgr_for_cb.update_left(report, &crate::protocol::calibration::StickCalibration::default());
                                } else {
                                    mgr_for_cb.update_right(report, &crate::protocol::calibration::StickCalibration::default());
                                }
                            }),
                        )
                        .map_err(|err| {
                            io::Error::new(
                                err.kind(),
                                format!("{err} [discovery_stop: {discovery_note}]"),
                            )
                        })
                    }).await;

                    // Release connect lock regardless of success or failure
                    drop(conn_permit);

                    match res {
                        Ok(Ok(controller)) => {
                            info!("Controller {} connected and streaming inputs!", ctrl_cfg.mac);
                            while controller.is_connected() && running.load(Ordering::Relaxed) {
                                sleep(Duration::from_millis(500)).await;
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

    async fn run_split(&self) -> std::io::Result<()> {
        let mut handles = Vec::new();
        let connect_lock = Arc::new(Mutex::new(()));

        for ctrl_cfg in &self.config.controllers {
            let ctrl_cfg = ctrl_cfg.clone();
            let adapter_mac = self.config.adapter_mac.clone();
            let running = self.running.clone();
            let conn_lock = connect_lock.clone();

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

                    let conn_permit = conn_lock.lock().await;

                    let res = tokio::task::spawn_blocking(move || {
                        let is_l = is_left;
                        let mgr_for_cb = mgr_cb.clone();
                        // HANDOVER Bug 3: discovery stops per connect attempt
                        // (see the combined-mode branch; 09B centralises this in
                        // the hub, which is the only dialer).
                        let discovery_note = stop_discovery_for_connect(&mac_str);
                        SwitchController::connect_with(
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
                                if is_l {
                                    mgr_for_cb.update_horizontal_left(report, &crate::protocol::calibration::StickCalibration::default());
                                } else {
                                    mgr_for_cb.update_horizontal_right(report, &crate::protocol::calibration::StickCalibration::default());
                                }
                            }),
                        )
                        .map_err(|err| {
                            io::Error::new(
                                err.kind(),
                                format!("{err} [discovery_stop: {discovery_note}]"),
                            )
                        })
                    }).await;

                    drop(conn_permit);

                    match res {
                        Ok(Ok(controller)) => {
                            info!("Controller {} connected in split mode!", ctrl_cfg.mac);
                            while controller.is_connected() && running.load(Ordering::Relaxed) {
                                sleep(Duration::from_millis(500)).await;
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

/// Stop all Bluetooth discovery before an LE create-connection (HANDOVER
/// Bug 3, TEST_RESULTS.md finding 3/4). Best effort: failures are reported in
/// the connect error text, never fatal.
fn stop_discovery_for_connect(mac: &str) -> String {
    match ShellScanControl::from_env().prepare_global() {
        Ok(()) => "ok".to_string(),
        Err(err) => {
            warn!("Could not stop Bluetooth discovery before connecting {mac}: {err}");
            format!("failed: {err}")
        }
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
