//! Resolver for `led-players.json` (ADR-4).
//!
//! Legacy reference: `.old/ngc/bridge.py`
//! * file path: `bridge.py:37`
//! * lenient JSON reader: `bridge.py:185-210` (`_read_led_players`,
//!   `_led_override_for`)
//! * `effective_player()`: `bridge.py:509-511`
//! * live re-apply on the status tick: `bridge.py:763-780`
//!   (`_apply_led_overrides`)
//!
//! The file is written by `system/bazzite-set-player-leds.py` and maps
//! `"MAC": player` for emulator player order. Reading it is deliberately
//! best-effort: a missing/unreadable file, invalid JSON, or a non-object root
//! all resolve to an empty map so callers fall back to the configured player.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// File name written by the external LED sync tool (`bridge.py:37`).
pub const LED_PLAYERS_FILE: &str = "led-players.json";

/// Join the LED override file name onto a config directory.
pub fn path_in(dir: &Path) -> PathBuf {
    dir.join(LED_PLAYERS_FILE)
}

/// MAC -> player overrides from `led-players.json`.
///
/// Keys are normalized to uppercase; values are clamped to `1..=8` when the
/// file is parsed. An empty map means "no overrides".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedPlayers {
    map: HashMap<String, u8>,
}

impl LedPlayers {
    /// Load overrides from `path`. Any I/O error yields an empty map
    /// (`bridge.py:187-193`).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => Self::from_json(&raw),
            Err(_) => Self::default(),
        }
    }

    /// Parse the JSON text leniently (`bridge.py:187-203`).
    ///
    /// Invalid JSON, a non-object root, non-integer values, and nulls are all
    /// ignored rather than reported. Integer values are clamped to `1..=8`.
    pub fn from_json(raw: &str) -> Self {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            return Self::default();
        };
        let Some(object) = value.as_object() else {
            return Self::default();
        };

        let mut map = HashMap::with_capacity(object.len());
        for (mac, player) in object {
            // Only JSON integers count; strings/floats/bools/arrays are skipped.
            let Some(player) = player.as_i64() else {
                continue;
            };
            let player = player.clamp(1, 8) as u8;
            map.insert(mac.to_ascii_uppercase(), player);
        }
        Self { map }
    }

    /// Look up an override for `mac` (case-insensitive), if any.
    pub fn override_for(&self, mac: &str) -> Option<u8> {
        self.map.get(&mac.to_ascii_uppercase()).copied()
    }

    /// Effective player for `mac`: the override when present, else
    /// `config_player` (`bridge.py:508-511`).
    ///
    /// This value is authoritative for the player LED pattern, the DSU motion
    /// slot, and the uinput gamepad name (ADR-4). The daemon re-reads
    /// `led-players.json` on its 1.5 s status tick (`bridge.py:763-780`), so
    /// callers should re-resolve rather than caching the result forever.
    pub fn resolve(&self, mac: &str, config_player: u8) -> u8 {
        self.override_for(mac).unwrap_or(config_player)
    }

    /// Whether the parsed file contained no usable overrides.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_uppercases_keys() {
        let players =
            LedPlayers::from_json(r#"{"aa:bb:cc:dd:ee:ff": 3, "11:22:33:44:55:66": 7}"#);
        assert!(!players.is_empty());
        assert_eq!(players.override_for("aa:bb:cc:dd:ee:ff"), Some(3));
        assert_eq!(players.override_for("AA:BB:CC:DD:EE:FF"), Some(3));
        assert_eq!(players.override_for("Aa:Bb:Cc:Dd:Ee:Ff"), Some(3));
        assert_eq!(players.override_for("11:22:33:44:55:66"), Some(7));
        assert_eq!(players.override_for("99:99:99:99:99:99"), None);
    }

    #[test]
    fn skips_non_integer_values() {
        let players = LedPlayers::from_json(
            r#"{
                "AA:00:00:00:00:01": "2",
                "AA:00:00:00:00:02": 2.5,
                "AA:00:00:00:00:03": true,
                "AA:00:00:00:00:04": null,
                "AA:00:00:00:00:05": [2],
                "AA:00:00:00:00:06": {"player": 2},
                "AA:00:00:00:00:07": 4
            }"#,
        );
        for mac in 1..=6 {
            assert_eq!(
                players.override_for(&format!("AA:00:00:00:00:0{mac}")),
                None,
                "non-integer value for {mac} must be skipped"
            );
        }
        assert_eq!(players.override_for("AA:00:00:00:00:07"), Some(4));
    }

    #[test]
    fn clamps_to_one_through_eight() {
        let players = LedPlayers::from_json(
            r#"{
                "AA:00:00:00:00:00": 0,
                "AA:00:00:00:00:01": 1,
                "AA:00:00:00:00:08": 8,
                "AA:00:00:00:00:09": 9,
                "AA:00:00:00:00:42": 42,
                "AA:00:00:00:00:99": -5
            }"#,
        );
        assert_eq!(players.override_for("AA:00:00:00:00:00"), Some(1));
        assert_eq!(players.override_for("AA:00:00:00:00:01"), Some(1));
        assert_eq!(players.override_for("AA:00:00:00:00:08"), Some(8));
        assert_eq!(players.override_for("AA:00:00:00:00:09"), Some(8));
        assert_eq!(players.override_for("AA:00:00:00:00:42"), Some(8));
        assert_eq!(players.override_for("AA:00:00:00:00:99"), Some(1));
    }

    #[test]
    fn invalid_json_yields_empty_map() {
        for raw in [
            "",
            "{not json",
            "[]",
            "[{\"AA:BB:CC:DD:EE:FF\": 2}]",
            "42",
            "null",
            "\"AA:BB:CC:DD:EE:FF\"",
        ] {
            assert!(LedPlayers::from_json(raw).is_empty(), "raw input: {raw:?}");
        }
    }

    #[test]
    fn missing_file_yields_empty_map() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "switch2d-missing-led-players-{}-{}.json",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&path);
        assert!(LedPlayers::load(&path).is_empty());
        assert_eq!(path_in(&dir), dir.join(LED_PLAYERS_FILE));
    }

    #[test]
    fn resolve_falls_back_to_config_player() {
        let empty = LedPlayers::default();
        assert_eq!(empty.resolve("AA:BB:CC:DD:EE:FF", 5), 5);

        let players = LedPlayers::from_json(r#"{"11:22:33:44:55:66": 2}"#);
        assert_eq!(players.resolve("AA:BB:CC:DD:EE:FF", 5), 5);
    }

    #[test]
    fn resolve_prefers_override() {
        let players = LedPlayers::from_json(r#"{"aa:bb:cc:dd:ee:ff": 2}"#);
        assert_eq!(players.resolve("AA:BB:CC:DD:EE:FF", 5), 2);
        assert_eq!(players.resolve("aa:bb:cc:dd:ee:ff", 1), 2);
    }
}
