//! Configuration persistence for switch2-daemon.

use serde::{Deserialize, Serialize};
use std::fs;

use crate::uinput::FaceLayout;
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

/// Stick deadzone applied by the mapper when the config does not set one.
pub const DEFAULT_STICK_DEADZONE: u16 = 120;

/// Accepted range for `stick_deadzone`; out-of-range values are clamped.
const STICK_DEADZONE_MIN: u16 = 1;
const STICK_DEADZONE_MAX: u16 = 1000;

fn default_stick_deadzone() -> u16 {
    DEFAULT_STICK_DEADZONE
}

/// Parse `stick_deadzone` tolerantly: accept integer or numeric-string input
/// and clamp to the supported range instead of rejecting the whole config.
fn deserialize_stick_deadzone<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    let value = serde_json::Value::deserialize(deserializer)?;
    let raw = match &value {
        serde_json::Value::Number(number) => number.as_i64(),
        serde_json::Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    };
    let raw = raw.ok_or_else(|| D::Error::custom("stick_deadzone must be an integer"))?;
    Ok(raw.clamp(i64::from(STICK_DEADZONE_MIN), i64::from(STICK_DEADZONE_MAX)) as u16)
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
    /// Stick deadzone policy value. Optional in the file; absent means 120.
    /// Values outside `1..=1000` are clamped rather than rejected.
    #[serde(
        default = "default_stick_deadzone",
        deserialize_with = "deserialize_stick_deadzone"
    )]
    pub stick_deadzone: u16,
    /// Face-button convention for the uinput nodes (`positions` default).
    ///
    /// Tolerant: an unknown string warns and falls back to the default instead
    /// of failing the whole config load.
    #[serde(default, deserialize_with = "deserialize_face_layout")]
    pub face_button_layout: FaceLayout,
}

fn deserialize_face_layout<'de, D>(deserializer: D) -> Result<FaceLayout, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Same tolerant shape as `stick_deadzone`: the file is JSON, and a
    // wrong-typed value must not fail the whole config load.
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value.as_str().and_then(FaceLayout::from_name) {
        Some(layout) => layout,
        None => {
            if !value.is_null() {
                tracing::warn!(
                    "Unknown face_button_layout {}; using positions (valid: positions, labels)",
                    value
                );
            }
            FaceLayout::Positions
        }
    })
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
            stick_deadzone: DEFAULT_STICK_DEADZONE,
            face_button_layout: FaceLayout::Positions,
        }
    }
}

impl DaemonConfig {
    pub fn config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config/switch2d/config.json")
    }

    /// Load config from file, migrating from previous config formats if necessary.
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

        // Try migrating from ~/.config/switch2-daemon/config.json
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let prev_path = PathBuf::from(&home).join(".config/switch2-daemon/config.json");
        if prev_path.exists() {
            if let Ok(content) = fs::read_to_string(&prev_path) {
                if let Ok(cfg) = serde_json::from_str::<DaemonConfig>(&content) {
                    info!("Migrated config from {}", prev_path.display());
                    let _ = cfg.save();
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
                let mac = c
                    .get("mac")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = c
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("Switch 2 Controller")
                    .to_string();
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
            stick_deadzone: DEFAULT_STICK_DEADZONE,
            face_button_layout: FaceLayout::Positions,
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
        if let Some(existing) = self
            .controllers
            .iter_mut()
            .find(|c| c.mac.eq_ignore_ascii_case(&controller.mac))
        {
            *existing = controller;
        } else {
            self.controllers.push(controller);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> DaemonConfig {
        serde_json::from_str(json).expect("config parses")
    }

    #[test]
    fn stick_deadzone_defaults_when_absent() {
        let config = parse(r#"{}"#);
        assert_eq!(config.stick_deadzone, DEFAULT_STICK_DEADZONE);
        assert_eq!(config.stick_deadzone, 120);
        assert_eq!(
            DaemonConfig::default().stick_deadzone,
            DEFAULT_STICK_DEADZONE
        );
    }

    #[test]
    fn stick_deadzone_accepts_custom_value() {
        assert_eq!(parse(r#"{"stick_deadzone": 250}"#).stick_deadzone, 250);
        // Tolerant of a numeric string, as in a hand-edited config.
        assert_eq!(parse(r#"{"stick_deadzone": "300"}"#).stick_deadzone, 300);
    }

    #[test]
    fn stick_deadzone_clamps_out_of_range_values() {
        assert_eq!(parse(r#"{"stick_deadzone": 0}"#).stick_deadzone, 1);
        assert_eq!(parse(r#"{"stick_deadzone": -50}"#).stick_deadzone, 1);
        assert_eq!(parse(r#"{"stick_deadzone": 5000}"#).stick_deadzone, 1000);
        assert_eq!(
            parse(r#"{"stick_deadzone": 999999999999}"#).stick_deadzone,
            1000
        );
    }
}
