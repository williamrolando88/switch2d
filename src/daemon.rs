//! Central daemon orchestrating multiple controller connections and virtual gamepad lifecycle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::{DaemonConfig, DaemonMode};
use crate::controller::SwitchController;
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
                "No Bluetooth adapter MAC configured. Run 'switch2-daemon pair' first.",
            ));
        }

        info!("Starting switch2-daemon with adapter {}", adapter_mac);
        info!("Active mode: {:?}", self.config.mode);

        if self.config.controllers.is_empty() {
            warn!("No controllers paired yet. Run 'switch2-daemon pair' to bond Joy-Cons.");
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

        let mut handles = Vec::new();

        for ctrl_cfg in &self.config.controllers {
            let ctrl_cfg = ctrl_cfg.clone();
            let adapter_mac = self.config.adapter_mac.clone();
            let running = self.running.clone();
            let mgr = manager.clone();

            let handle = tokio::spawn(async move {
                let is_left = ctrl_cfg.name.to_lowercase().contains("left");

                while running.load(Ordering::Relaxed) {
                    let mgr_cb = mgr.clone();
                    let mac_str = ctrl_cfg.mac.clone();
                    let adapter_str = adapter_mac.clone();

                    let res = tokio::task::spawn_blocking(move || {
                        let is_l = is_left;
                        let mgr_for_cb = mgr_cb.clone();
                        SwitchController::connect(
                            &mac_str,
                            &adapter_str,
                            ctrl_cfg.player,
                            Box::new(move |report| {
                                if is_l {
                                    mgr_for_cb.update_left(report, &crate::protocol::calibration::StickCalibration::default());
                                } else {
                                    mgr_for_cb.update_right(report, &crate::protocol::calibration::StickCalibration::default());
                                }
                            }),
                        )
                    }).await;

                    match res {
                        Ok(Ok(controller)) => {
                            info!("Controller {} connected and streaming inputs!", ctrl_cfg.mac);
                            while controller.is_connected() && running.load(Ordering::Relaxed) {
                                sleep(Duration::from_millis(500)).await;
                            }
                            warn!("Controller {} disconnected.", ctrl_cfg.mac);
                        }
                        Ok(Err(_e)) => {
                            // Connection failed / controller asleep; retry after short interval
                            trace_or_debug(&ctrl_cfg.mac);
                            sleep(Duration::from_millis(2000)).await;
                        }
                        Err(e) => {
                            error!("Spawn task error: {}", e);
                            sleep(Duration::from_millis(2000)).await;
                        }
                    }
                }
            });

            handles.push(handle);
        }

        // Wait for all worker tasks
        for h in handles {
            let _ = h.await;
        }

        Ok(())
    }

    async fn run_split(&self) -> std::io::Result<()> {
        let mut handles = Vec::new();

        for ctrl_cfg in &self.config.controllers {
            let ctrl_cfg = ctrl_cfg.clone();
            let adapter_mac = self.config.adapter_mac.clone();
            let running = self.running.clone();

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

                    let res = tokio::task::spawn_blocking(move || {
                        let is_l = is_left;
                        let mgr_for_cb = mgr_cb.clone();
                        SwitchController::connect(
                            &mac_str,
                            &adapter_str,
                            ctrl_cfg.player,
                            Box::new(move |report| {
                                if is_l {
                                    mgr_for_cb.update_left(report, &crate::protocol::calibration::StickCalibration::default());
                                } else {
                                    mgr_for_cb.update_right(report, &crate::protocol::calibration::StickCalibration::default());
                                }
                            }),
                        )
                    }).await;

                    match res {
                        Ok(Ok(controller)) => {
                            info!("Controller {} connected in split mode!", ctrl_cfg.mac);
                            while controller.is_connected() && running.load(Ordering::Relaxed) {
                                sleep(Duration::from_millis(500)).await;
                            }
                            warn!("Controller {} disconnected.", ctrl_cfg.mac);
                        }
                        Ok(Err(_)) => {
                            sleep(Duration::from_millis(2000)).await;
                        }
                        Err(e) => {
                            error!("Spawn task error: {}", e);
                            sleep(Duration::from_millis(2000)).await;
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

fn trace_or_debug(mac: &str) {
    tracing::debug!("Waiting for {} to advertise...", mac);
}
