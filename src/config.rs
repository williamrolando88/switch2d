//! Configuration persistence for switch2-daemon.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonMode {
    Combined,
    Split,
}

impl Default for DaemonMode {
    fn default() -> Self {
        Self::Combined
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BondedController {
    pub mac: String,
    pub name: String,
    pub product_id: u16,
    #[serde(default)]
    pub serial: String,
    pub player: u8,
    #[serde(default = "default_true")]
    pub bonded: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub adapter_mac: String,
    #[serde(default)]
    pub mode: DaemonMode,
    #[serde(default)]
    pub controllers: Vec<BondedController>,
    #[serde(default = "default_idle_sleep")]
    pub idle_sleep_seconds: u64,
}

fn default_idle_sleep() -> u64 {
    300
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            adapter_mac: String::new(),
            mode: DaemonMode::Combined,
            controllers: Vec::new(),
            idle_sleep_seconds: 300,
        }
    }
}

impl DaemonConfig {
    pub fn config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config/switch2-daemon/config.json")
    }

    /// Load config from file, migrating from legacy python config if necessary.
    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(cfg) = serde_json::from_str::<DaemonConfig>(&content) {
                    info!("Loaded config from {}", path.display());
                    return cfg;
                }
            }
        }

        // Try migrating from legacy ~/.config/nso-gc/config.json
        if let Some(legacy_cfg) = Self::try_import_legacy() {
            info!("Imported bonded controllers from legacy ~/.config/nso-gc/config.json");
            let _ = legacy_cfg.save();
            return legacy_cfg;
        }

        Self::default()
    }

    fn try_import_legacy() -> Option<Self> {
        let home = std::env::var("HOME").ok()?;
        let legacy_path = PathBuf::from(home).join(".config/nso-gc/config.json");
        if !legacy_path.exists() {
            return None;
        }

        let content = fs::read_to_string(legacy_path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&content).ok()?;

        let adapter_mac = v.get("adapter_mac")?.as_str()?.to_string();
        let mut controllers = Vec::new();

        if let Some(ctrls) = v.get("controllers").and_then(|c| c.as_array()) {
            for c in ctrls {
                let mac = c.get("mac").and_then(|m| m.as_str()).unwrap_or("").to_string();
                let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("Switch 2 Controller").to_string();
                let player = c.get("player").and_then(|p| p.as_u64()).unwrap_or(1) as u8;

                // Guess PID from name
                let pid = if name.contains("Left") {
                    crate::protocol::constants::JOYCON2_LEFT_PID
                } else if name.contains("Right") {
                    crate::protocol::constants::JOYCON2_RIGHT_PID
                } else {
                    crate::protocol::constants::PRO_CONTROLLER2_PID
                };

                if !mac.is_empty() {
                    controllers.push(BondedController {
                        mac,
                        name,
                        product_id: pid,
                        serial: String::new(),
                        player,
                        bonded: true,
                    });
                }
            }
        }

        Some(Self {
            adapter_mac,
            mode: DaemonMode::Combined,
            controllers,
            idle_sleep_seconds: 300,
        })
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json)?;
        info!("Saved config to {}", path.display());
        Ok(())
    }

    pub fn add_or_update_controller(&mut self, controller: BondedController) {
        if let Some(existing) = self.controllers.iter_mut().find(|c| c.mac.eq_ignore_ascii_case(&controller.mac)) {
            *existing = controller;
        } else {
            self.controllers.push(controller);
        }
    }
}
