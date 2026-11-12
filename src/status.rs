//! Atomic status plane for the daemon (`state.json`).
//!
//! GUI, Decky plugin and watchdog consume this file instead of scraping logs
//! (analysis §3.7). [`StatusPublisher::publish`] writes pretty JSON to
//! `<path>.tmp` and renames it over the final path, so a reader always sees
//! either the previous document or the new one — never a partial write.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One controller as reported to status consumers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ControllerStatus {
    pub mac: String,
    /// Configured player slot (legacy `state.json` meaning).
    pub player: u8,
    /// Player actually used for LEDs/DSU/name when an `led-players.json`
    /// override is in force (ADR-4). `None` means "same as [`Self::player`]".
    #[serde(default)]
    pub effective_player: Option<u8>,
    pub name: String,
    pub bonded: bool,
    pub connected: bool,
    pub battery_pct: Option<u8>,
    pub battery_mv: Option<u16>,
}

/// Service lifecycle as exposed to status consumers (legacy `service` field).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    /// Normal operation.
    #[default]
    Running,
    /// Shutdown in progress.
    Stopping,
    /// Hub/daemon error state.
    Error,
}

/// Whole-daemon status document (`state.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub updated_at_unix: u64,
    pub pid: u32,
    pub hub_alive: bool,
    pub hub_scanning: bool,
    pub hub_error: Option<String>,
    /// Legacy `service` field: `running` / `stopping` / `error`.
    pub service: ServiceState,
    pub headline: String,
    pub detail: String,
    pub controllers: Vec<ControllerStatus>,
}

impl DaemonStatus {
    /// Fresh status carrying this process id and the current timestamp.
    pub fn new() -> Self {
        Self {
            pid: process::id(),
            updated_at_unix: now_unix(),
            ..Self::default()
        }
    }

    /// Refresh [`DaemonStatus::updated_at_unix`] to now.
    pub fn touch(&mut self) {
        self.updated_at_unix = now_unix();
    }

    /// Set the published lifecycle state (legacy `service` field).
    pub fn set_service(&mut self, service: ServiceState) {
        self.service = service;
    }

    /// Insert or replace (same MAC, ASCII case-insensitively) a controller.
    ///
    /// Replacement keeps the existing position so slot order stays stable.
    pub fn upsert_controller(&mut self, controller: ControllerStatus) {
        match self
            .controllers
            .iter_mut()
            .find(|c| c.mac.eq_ignore_ascii_case(&controller.mac))
        {
            Some(existing) => *existing = controller,
            None => self.controllers.push(controller),
        }
    }

    /// Drop the controller with `mac` (ASCII case-insensitive), if any.
    pub fn remove_controller(&mut self, mac: &str) {
        self.controllers
            .retain(|c| !c.mac.eq_ignore_ascii_case(mac));
    }

    /// Number of controllers currently connected.
    pub fn connected_count(&self) -> usize {
        self.controllers.iter().filter(|c| c.connected).count()
    }

    /// Whether the document is older than `max_age` relative to `now_unix`.
    pub fn is_stale(&self, now_unix: u64, max_age: Duration) -> bool {
        now_unix.saturating_sub(self.updated_at_unix) > max_age.as_secs()
    }
}

/// Path-aware writer for the status document.
#[derive(Debug, Clone)]
pub struct StatusPublisher {
    path: PathBuf,
}

impl Default for StatusPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusPublisher {
    /// Publisher for the standard status path.
    ///
    /// `/` for `XDG_CONFIG_HOME` (falling back to `~/.config`) via
    /// [`state_path_from`].
    pub fn new() -> Self {
        let xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let home = std::env::var("HOME").ok();
        Self::with_path(state_path_from(xdg.as_deref(), home.as_deref()))
    }

    /// Publisher for an explicit path.
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// The file this publisher writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write `status` atomically: create the parent dir, write `<path>.tmp`,
    /// then rename it onto the final path.
    pub fn publish(&self, status: &DaemonStatus) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        let json = serde_json::to_vec_pretty(status)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let tmp = tmp_path(&self.path);
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &self.path)
    }

    /// Remove the status file (legacy `clear_state` at shutdown).
    ///
    /// A missing file is not an error.
    pub fn clear(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

/// Resolve the status path: `$XDG_CONFIG_HOME/switch2d/state.json`, else
/// `$HOME/.config/switch2d/state.json`, else `./switch2d/state.json`.
///
/// Empty environment values are treated as unset (per the XDG basedir spec).
/// Pure function so both branches are testable without touching process env.
pub fn state_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> PathBuf {
    match xdg_config_home.filter(|dir| !dir.is_empty()) {
        Some(xdg) => PathBuf::from(xdg).join("switch2d/state.json"),
        None => match home.filter(|dir| !dir.is_empty()) {
            Some(home) => PathBuf::from(home).join(".config/switch2d/state.json"),
            None => PathBuf::from("switch2d/state.json"),
        },
    }
}

/// Battery percentage from a voltage reading, using the legacy math
/// (t12 §9.2): `None` for no reading (`0 mV`), otherwise
/// `clamp(((mv - 2950) * 100) / 1300, 0, 100)` with integer truncation.
pub fn battery_pct_from_mv(mv: u16) -> Option<u8> {
    if mv == 0 {
        return None;
    }
    let pct = (i32::from(mv) - 2950) * 100 / 1300;
    Some(pct.clamp(0, 100) as u8)
}

/// Current wall-clock time in whole seconds since the Unix epoch.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `<path>.tmp`, preserving the full original file name.
fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Self-cleaning temp directory (`tempfile` is deliberately not a dep).
    struct TempDir {
        root: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "switch2d-status-{}-{}-{}",
                process::id(),
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

    fn controller(mac: &str, player: u8, connected: bool) -> ControllerStatus {
        ControllerStatus {
            mac: mac.to_string(),
            player,
            effective_player: None,
            name: "Pro Controller 2".to_string(),
            bonded: true,
            connected,
            battery_pct: Some(80),
            battery_mv: Some(4100),
        }
    }

    #[test]
    fn status_serde_round_trip() {
        let mut status = DaemonStatus::new();
        status.hub_alive = true;
        status.hub_scanning = true;
        status.hub_error = Some("scan failed".to_string());
        status.headline = "1 controller".to_string();
        status.detail = "ready".to_string();
        status.controllers.push(ControllerStatus {
            mac: "AA:BB:CC:DD:EE:01".to_string(),
            player: 1,
            effective_player: Some(3),
            name: "Joy-Con 2 (Right)".to_string(),
            bonded: true,
            connected: true,
            battery_pct: Some(78),
            battery_mv: Some(4020),
        });
        // Second pad: no battery reading at all.
        status.controllers.push(ControllerStatus {
            mac: "AA:BB:CC:DD:EE:02".to_string(),
            player: 2,
            effective_player: None,
            name: "NSO GameCube Controller".to_string(),
            bonded: false,
            connected: false,
            battery_pct: None,
            battery_mv: None,
        });

        let json = serde_json::to_string(&status).expect("serialize");
        let back: DaemonStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, status);

        // Option fields use external `null` names, not renamed keys.
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            value["controllers"][1]["battery_pct"],
            serde_json::Value::Null
        );
        assert_eq!(
            value["controllers"][1]["battery_mv"],
            serde_json::Value::Null
        );
        assert_eq!(
            value["controllers"][0]["battery_pct"],
            serde_json::json!(78u8)
        );
        assert_eq!(value["hub_error"], serde_json::json!("scan failed"));
        assert_eq!(value["service"], serde_json::json!("running"));
        assert_eq!(
            value["controllers"][0]["effective_player"],
            serde_json::json!(3)
        );
        assert_eq!(
            value["controllers"][1]["effective_player"],
            serde_json::Value::Null
        );
        // `effective_player` is optional on read (older state files omit it).
        let legacy: DaemonStatus = serde_json::from_str(
            r#"{"updated_at_unix":1,"pid":2,"hub_alive":true,"hub_scanning":false,
                "hub_error":null,"service":"running","headline":"h","detail":"d",
                "controllers":[{"mac":"AA:BB:CC:DD:EE:09","player":1,"name":"Pad",
                                "bonded":true,"connected":false,"battery_pct":null,
                                "battery_mv":null}]}"#,
        )
        .expect("legacy state file parses");
        assert_eq!(legacy.controllers[0].effective_player, None);

        // Default document is the "nothing known yet" shape.
        let default = DaemonStatus::default();
        assert_eq!(default.updated_at_unix, 0);
        assert_eq!(default.pid, 0);
        assert!(!default.hub_alive);
        assert!(default.headline.is_empty());
        assert!(default.controllers.is_empty());
        assert_eq!(
            default,
            DaemonStatus {
                updated_at_unix: 0,
                pid: 0,
                hub_alive: false,
                hub_scanning: false,
                hub_error: None,
                service: ServiceState::Running,
                headline: String::new(),
                detail: String::new(),
                controllers: Vec::new(),
            }
        );
        assert_eq!(ControllerStatus::default().player, 0);
        assert_eq!(ControllerStatus::default().effective_player, None);
        assert_eq!(ControllerStatus::default().battery_pct, None);
    }

    #[test]
    fn service_state_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&ServiceState::Running).expect("serialize"),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&ServiceState::Stopping).expect("serialize"),
            "\"stopping\""
        );
        assert_eq!(
            serde_json::to_string(&ServiceState::Error).expect("serialize"),
            "\"error\""
        );

        // Defaults and set_service round-trip through the document.
        let mut status = DaemonStatus::new();
        assert_eq!(status.service, ServiceState::Running);
        status.set_service(ServiceState::Stopping);
        assert_eq!(status.service, ServiceState::Stopping);
        let json = serde_json::to_string(&status).expect("serialize");
        let back: DaemonStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.service, ServiceState::Stopping);
        assert_eq!(DaemonStatus::default().service, ServiceState::Running);
    }

    #[test]
    fn state_path_from_matrix() {
        assert_eq!(
            state_path_from(Some("/xdg"), Some("/home/u")),
            PathBuf::from("/xdg/switch2d/state.json")
        );
        assert_eq!(
            state_path_from(None, Some("/home/u")),
            PathBuf::from("/home/u/.config/switch2d/state.json")
        );
        assert_eq!(
            state_path_from(None, None),
            PathBuf::from("switch2d/state.json")
        );
        // Empty env values count as unset (XDG basedir semantics).
        assert_eq!(
            state_path_from(Some(""), Some("/home/u")),
            PathBuf::from("/home/u/.config/switch2d/state.json")
        );
        assert_eq!(
            state_path_from(None, Some("")),
            PathBuf::from("switch2d/state.json")
        );
    }

    #[test]
    fn clear_removes_state_file_and_tolerates_missing() {
        let dir = TempDir::new("clear");
        let path = dir.state_path();
        let publisher = StatusPublisher::with_path(path.clone());

        // No file yet: still Ok.
        publisher.clear().expect("clear without file");

        publisher
            .publish(&DaemonStatus::new())
            .expect("publish before clear");
        assert!(path.is_file());
        publisher.clear().expect("clear existing file");
        assert!(!path.exists());
        assert!(!tmp_path(&path).exists());

        // Idempotent.
        publisher.clear().expect("clear twice");
    }

    #[test]
    fn battery_pct_from_mv_legacy_math() {
        assert_eq!(battery_pct_from_mv(0), None);
        assert_eq!(battery_pct_from_mv(2949), Some(0));
        assert_eq!(battery_pct_from_mv(2950), Some(0));
        assert_eq!(battery_pct_from_mv(3600), Some(50));
        assert_eq!(battery_pct_from_mv(4200), Some(96));
        assert_eq!(battery_pct_from_mv(65535), Some(100));
    }

    #[test]
    fn upsert_replaces_matching_mac_case_insensitively() {
        let mut status = DaemonStatus::new();
        status.upsert_controller(controller("aa:bb:cc:dd:ee:01", 1, true));
        status.upsert_controller(controller("AA:BB:CC:DD:EE:02", 2, false));

        // Same physical MAC in different case replaces in place (no push).
        status.upsert_controller(controller("Aa:Bb:Cc:Dd:Ee:01", 4, false));
        assert_eq!(status.controllers.len(), 2);
        assert_eq!(status.controllers[0].player, 4);
        assert_eq!(status.controllers[0].mac, "Aa:Bb:Cc:Dd:Ee:01");
        assert!(!status.controllers[0].connected);
        // Unrelated entry untouched and order preserved.
        assert_eq!(status.controllers[1].mac, "AA:BB:CC:DD:EE:02");

        // Unknown MAC appends.
        status.upsert_controller(controller("AA:BB:CC:DD:EE:03", 3, true));
        assert_eq!(status.controllers.len(), 3);
        assert_eq!(status.controllers[2].mac, "AA:BB:CC:DD:EE:03");
    }

    #[test]
    fn remove_controller_and_connected_count() {
        let mut status = DaemonStatus::new();
        status.upsert_controller(controller("AA:BB:CC:DD:EE:01", 1, true));
        status.upsert_controller(controller("AA:BB:CC:DD:EE:02", 2, true));
        status.upsert_controller(controller("AA:BB:CC:DD:EE:03", 3, false));
        assert_eq!(status.connected_count(), 2);

        status.remove_controller("aa:bb:cc:dd:ee:01");
        assert_eq!(status.controllers.len(), 2);
        assert_eq!(status.connected_count(), 1);

        // Removing an absent MAC is a no-op.
        status.remove_controller("11:22:33:44:55:66");
        assert_eq!(status.controllers.len(), 2);
        assert_eq!(status.connected_count(), 1);

        // Removing the rest empties the document.
        status.remove_controller("AA:BB:CC:DD:EE:02");
        status.remove_controller("AA:BB:CC:DD:EE:03");
        assert!(status.controllers.is_empty());
        assert_eq!(status.connected_count(), 0);
    }

    #[test]
    fn is_stale_boundary() {
        let mut status = DaemonStatus::default();
        status.updated_at_unix = 1_000;
        let max_age = Duration::from_secs(4);

        // Exactly max_age old is still fresh; one second more is stale.
        assert!(!status.is_stale(1_004, max_age));
        assert!(status.is_stale(1_005, max_age));
        assert!(!status.is_stale(1_000, max_age));

        // A clock that went backwards is not stale (saturating subtraction).
        assert!(!status.is_stale(500, max_age));
        assert!(!status.is_stale(0, max_age));

        // Zero max_age: same-second is fresh, anything older is stale.
        let zero = Duration::from_secs(0);
        assert!(!status.is_stale(1_000, zero));
        assert!(status.is_stale(1_001, zero));
    }

    #[test]
    fn publish_is_atomic_and_leaves_no_tmp() {
        let dir = TempDir::new("publish");
        let path = dir.state_path();
        let tmp = tmp_path(&path);
        let publisher = StatusPublisher::with_path(path.clone());
        assert_eq!(publisher.path(), path.as_path());

        let mut first = DaemonStatus::new();
        first.headline = "starting".to_string();
        first.upsert_controller(controller("AA:BB:CC:DD:EE:01", 1, false));
        publisher.publish(&first).expect("first publish");

        let written: DaemonStatus =
            serde_json::from_str(&fs::read_to_string(&path).expect("read status file"))
                .expect("parse status file");
        assert_eq!(written, first);
        assert!(!tmp.exists(), "temporary file must be renamed away");
        assert_eq!(
            fs::read_dir(&dir.root).expect("list dir").count(),
            1,
            "only state.json should remain"
        );

        let mut second = first.clone();
        second.touch();
        second.headline = "1 controller connected".to_string();
        second.upsert_controller(controller("AA:BB:CC:DD:EE:01", 1, true));
        publisher.publish(&second).expect("second publish");

        let written: DaemonStatus =
            serde_json::from_str(&fs::read_to_string(&path).expect("read status file"))
                .expect("parse status file");
        assert_eq!(written, second);
        assert!(!tmp.exists());
        assert_eq!(fs::read_dir(&dir.root).expect("list dir").count(), 1);

        // Parent directories are created on demand.
        let nested = dir.root.join("nested/deeper/state.json");
        let publisher = StatusPublisher::with_path(nested.clone());
        publisher.publish(&second).expect("nested publish");
        assert!(nested.is_file());
        assert!(!tmp_path(&nested).exists());
    }
}
