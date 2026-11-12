//! `switch2d doctor`: read-only preflight for the environment problems that
//! otherwise surface as controller reconnect timeouts.
//!
//! Every check has one stable output line:
//!
//! ```text
//! PASS  uinput: /dev/uinput is writable
//! WARN  DSU port: 127.0.0.1:26760 is already in use
//!   fix: stop the other DSU server, or accept that motion is unavailable
//! ```
//!
//! Exit code is 0 when nothing is FAIL, 1 when any check is FAIL. Checks never
//! need root. Probes that cannot run unprivileged report SKIP instead.

use std::fs;
use std::io;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::config::DaemonConfig;

/// One line of doctor output plus its remedy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    pub remedy: String,
}

/// Stable status words, uppercase in output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        }
    }
}

/// `systemctl is-enabled` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitEnabled {
    Enabled,
    Disabled,
    NotFound,
    Unknown(String),
}

impl UnitEnabled {
    fn label(&self) -> String {
        match self {
            UnitEnabled::Enabled => "enabled".to_string(),
            UnitEnabled::Disabled => "disabled".to_string(),
            UnitEnabled::NotFound => "not-found".to_string(),
            UnitEnabled::Unknown(raw) => raw.clone(),
        }
    }
}

/// `systemctl is-active` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitActive {
    Active,
    Inactive,
    Failed,
    Unknown(String),
}

impl UnitActive {
    fn label(&self) -> String {
        match self {
            UnitActive::Active => "active".to_string(),
            UnitActive::Inactive => "inactive".to_string(),
            UnitActive::Failed => "failed".to_string(),
            UnitActive::Unknown(raw) => raw.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdUnit {
    pub enabled: UnitEnabled,
    pub active: UnitActive,
}

impl SystemdUnit {
    fn detail(&self) -> String {
        format!(
            "is-enabled={}, is-active={}",
            self.enabled.label(),
            self.active.label()
        )
    }
}

/// One BlueZ `org.bluez.Adapter1` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    pub path: String,
    pub address: String,
    pub name: Option<String>,
    /// `None` when BlueZ did not expose `SupportedTechnologies`.
    pub le: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterProbe {
    Found(Vec<AdapterInfo>),
    None,
    Error(String),
}

/// Counts from the current BlueZ object cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiscoveryCounts {
    pub devices: usize,
    pub with_manufacturer: usize,
}

impl DiscoveryCounts {
    fn detail(self) -> String {
        format!(
            "Device1 objects: {}, with ManufacturerData: {}",
            self.devices, self.with_manufacturer
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryProbe {
    Accepted(DiscoveryCounts),
    /// StartDiscovery returned InProgress: something else already owns discovery.
    InProgress(DiscoveryCounts),
    Failed {
        counts: DiscoveryCounts,
        error: String,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SudoBtmgmtProbe {
    Allowed,
    Denied(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigProbe {
    /// No config file at the canonical path.
    Missing,
    Loaded(usize),
    Error(String),
}

/// Raw environment probes. Tests inject a fake; production uses [`RealProbes`].
trait SystemProbes {
    fn uinput(&self) -> Result<(), String>;
    fn privacy_unit(&self) -> SystemdUnit;
    fn adapter(&self) -> AdapterProbe;
    fn discovery(&self) -> DiscoveryProbe;
    fn sudo_btmgmt(&self) -> SudoBtmgmtProbe;
    fn legacy_bridge_units(&self) -> Result<Vec<(&'static str, SystemdUnit)>, String>;
    fn other_switch2d_pids(&self) -> Result<Vec<u32>, String>;
    fn user_unit_status(&self) -> Result<SystemdUnit, String>;
    fn user_unit_text(&self) -> Result<String, String>;
    fn dsu_port_bound(&self) -> Result<bool, String>;
    fn config(&self) -> ConfigProbe;
    fn udev_rule(&self) -> Option<PathBuf>;
}

struct RealProbes;

impl SystemProbes for RealProbes {
    fn uinput(&self) -> Result<(), String> {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/uinput")
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    fn privacy_unit(&self) -> SystemdUnit {
        query_systemd_unit("switch2d-disable-le-privacy.service", false)
    }

    fn adapter(&self) -> AdapterProbe {
        match query_bluez_objects() {
            Ok(json) => match parse_adapters(&json) {
                Ok(adapters) if adapters.is_empty() => AdapterProbe::None,
                Ok(adapters) => AdapterProbe::Found(adapters),
                Err(err) => AdapterProbe::Error(err),
            },
            Err(err) => AdapterProbe::Error(err),
        }
    }

    fn discovery(&self) -> DiscoveryProbe {
        let json = match query_bluez_objects() {
            Ok(json) => json,
            Err(err) => return DiscoveryProbe::Error(err),
        };
        let counts = parse_device_cache_counts(&json).unwrap_or_default();
        let adapter = match parse_adapters(&json) {
            Ok(adapters) => match adapters.into_iter().next() {
                Some(adapter) => adapter,
                None => {
                    return DiscoveryProbe::Failed {
                        counts,
                        error: "BlueZ reported no org.bluez.Adapter1".to_string(),
                    }
                }
            },
            Err(err) => return DiscoveryProbe::Error(err),
        };

        let result = busctl_call(&[
            "--system",
            "call",
            "org.bluez",
            &adapter.path,
            "org.bluez.Adapter1",
            "StartDiscovery",
        ]);

        match result {
            Ok(_) => {
                // We started it, so leave the adapter as we found it.
                let _ = busctl_call(&[
                    "--system",
                    "call",
                    "org.bluez",
                    &adapter.path,
                    "org.bluez.Adapter1",
                    "StopDiscovery",
                ]);
                DiscoveryProbe::Accepted(counts)
            }
            Err(err)
                if err.to_ascii_lowercase().contains("inprogress")
                    || err.to_ascii_lowercase().contains("in progress") =>
            {
                // Someone else owns discovery; do not stop theirs.
                DiscoveryProbe::InProgress(counts)
            }
            Err(error) => DiscoveryProbe::Failed { counts, error },
        }
    }

    fn sudo_btmgmt(&self) -> SudoBtmgmtProbe {
        let output = match Command::new("sudo")
            .args(["-n", "-l", "/usr/bin/btmgmt"])
            .output()
        {
            Ok(output) => output,
            Err(err) => return SudoBtmgmtProbe::Error(format!("sudo: {err}")),
        };
        if output.status.success() {
            return SudoBtmgmtProbe::Allowed;
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let reason = if !stderr.is_empty() { stderr } else { stdout };
        SudoBtmgmtProbe::Denied(if reason.is_empty() {
            format!("sudo -n -l /usr/bin/btmgmt exited with {}", output.status)
        } else {
            reason
        })
    }

    fn legacy_bridge_units(&self) -> Result<Vec<(&'static str, SystemdUnit)>, String> {
        const UNITS: [&str; 4] = [
            "nso-gc.service",
            "nso-gc-watchdog.service",
            "nso-gc-watchdog.timer",
            "nso-gc-after-gamescope.service",
        ];
        Ok(UNITS
            .iter()
            .map(|unit| (*unit, query_systemd_unit(unit, true)))
            .collect())
    }

    fn other_switch2d_pids(&self) -> Result<Vec<u32>, String> {
        let output = Command::new("pgrep")
            .args(["-af", "switch2d"])
            .output()
            .map_err(|err| format!("pgrep: {err}"))?;

        if output.status.code() == Some(1) {
            return Ok(Vec::new());
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(format!("pgrep exited with {}: {}", output.status, stderr));
        }

        let own_pid = std::process::id();
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(switch2d_process_pid)
            .filter(|pid| *pid != own_pid)
            .collect())
    }

    fn user_unit_status(&self) -> Result<SystemdUnit, String> {
        let enabled = systemctl_field("is-enabled", "switch2d.service", true)?;
        let active = systemctl_field("is-active", "switch2d.service", true)?;
        Ok(SystemdUnit {
            enabled: parse_enabled(&enabled),
            active: parse_active(&active),
        })
    }

    fn user_unit_text(&self) -> Result<String, String> {
        let output = Command::new("systemctl")
            .args(["--user", "cat", "switch2d.service"])
            .output()
            .map_err(|err| format!("systemctl --user cat: {err}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(format!(
                "systemctl --user cat switch2d.service exited with {}: {}",
                output.status, stderr
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn dsu_port_bound(&self) -> Result<bool, String> {
        match UdpSocket::bind(("127.0.0.1", 26760)) {
            Ok(_) => Ok(false),
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => Ok(true),
            Err(err) => Err(err.to_string()),
        }
    }

    fn config(&self) -> ConfigProbe {
        // The startup path already called DaemonConfig::load once. Parse the
        // canonical file directly here so doctor neither logs a second load
        // nor hides malformed JSON behind the loader's defaults.
        let path = DaemonConfig::config_path();
        if !path.exists() {
            return ConfigProbe::Missing;
        }
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<DaemonConfig>(&text) {
                Ok(config) => ConfigProbe::Loaded(config.controllers.len()),
                Err(err) => ConfigProbe::Error(format!("{}: {err}", path.display())),
            },
            Err(err) => ConfigProbe::Error(format!("{}: {err}", path.display())),
        }
    }

    fn udev_rule(&self) -> Option<PathBuf> {
        first_existing_path(UACCESS_RULE_PATHS)
    }
}

const UACCESS_RULE_PATHS: &[&str] = &[
    "/etc/udev/rules.d/70-switch2d.rules",
    "/usr/lib/udev/rules.d/70-switch2d.rules",
    "/lib/udev/rules.d/70-switch2d.rules",
    "/usr/local/lib/udev/rules.d/70-switch2d.rules",
    // Name used by the upstream Python project; accepted so a machine that
    // already has it installed is not told to install a duplicate rule.
    "/etc/udev/rules.d/70-ngc-imu-uaccess.rules",
    "/usr/lib/udev/rules.d/70-ngc-imu-uaccess.rules",
    "/lib/udev/rules.d/70-ngc-imu-uaccess.rules",
];

fn query_bluez_objects() -> Result<String, String> {
    let output = Command::new("busctl")
        .args([
            "--system",
            "call",
            "org.bluez",
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            "--json=short",
        ])
        .output()
        .map_err(|err| format!("busctl: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("busctl exited with {}: {}", output.status, stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn busctl_call(args: &[&str]) -> Result<String, String> {
    let output = Command::new("busctl")
        .args(args)
        .output()
        .map_err(|err| format!("busctl: {err}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let reason = if !stderr.is_empty() { stderr } else { stdout };
    Err(if reason.is_empty() {
        format!("busctl exited with {}", output.status)
    } else {
        reason
    })
}

fn parse_device_cache_counts(json: &str) -> Result<DiscoveryCounts, String> {
    let root: Value =
        serde_json::from_str(json).map_err(|err| format!("invalid BlueZ JSON: {err}"))?;
    let objects = root
        .get("data")
        .and_then(Value::as_array)
        .and_then(|data| data.first())
        .and_then(Value::as_object)
        .ok_or_else(|| "BlueZ payload has no object map at data[0]".to_string())?;

    let mut counts = DiscoveryCounts::default();
    for object in objects.values() {
        if let Some(device) = object.get("org.bluez.Device1") {
            counts.devices += 1;
            if device.get("ManufacturerData").is_some() {
                counts.with_manufacturer += 1;
            }
        }
    }
    Ok(counts)
}

/// `pgrep -af switch2d` also matches shell commands/cwds containing the name;
/// keep only processes whose executable basename is exactly `switch2d`.
fn switch2d_process_pid(line: &str) -> Option<u32> {
    let (pid, command) = line.split_once(char::is_whitespace)?;
    let executable = command.trim_start().split_whitespace().next()?;
    let basename = Path::new(executable).file_name()?.to_str()?;
    if basename != "switch2d" {
        return None;
    }
    pid.parse().ok()
}

fn first_existing_path(paths: &[&str]) -> Option<PathBuf> {
    paths
        .iter()
        .map(PathBuf::from)
        .find(|path| Path::new(path).exists())
}

fn systemctl_field(verb: &str, unit: &str, user: bool) -> Result<String, String> {
    let mut command = Command::new("systemctl");
    if user {
        command.arg("--user");
    }
    let output = command
        .args([verb, unit])
        .output()
        .map_err(|err| format!("systemctl: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return Err(stderr);
    }
    Err(format!("systemctl exited with {}", output.status))
}

fn query_systemd_unit(unit: &str, user: bool) -> SystemdUnit {
    let enabled = systemctl_field("is-enabled", unit, user)
        .map(|raw| parse_enabled(&raw))
        .unwrap_or_else(UnitEnabled::Unknown);
    let active = systemctl_field("is-active", unit, user)
        .map(|raw| parse_active(&raw))
        .unwrap_or_else(UnitActive::Unknown);
    SystemdUnit { enabled, active }
}

fn parse_enabled(raw: &str) -> UnitEnabled {
    match raw.trim() {
        "enabled" | "enabled-runtime" => UnitEnabled::Enabled,
        // Units without [Install] report one of these; none can be enabled.
        "disabled" | "static" | "indirect" | "alias" | "generated" => UnitEnabled::Disabled,
        "not-found" => UnitEnabled::NotFound,
        other => UnitEnabled::Unknown(other.to_string()),
    }
}

fn parse_active(raw: &str) -> UnitActive {
    match raw.trim() {
        "active" => UnitActive::Active,
        "inactive" => UnitActive::Inactive,
        "failed" => UnitActive::Failed,
        other => UnitActive::Unknown(other.to_string()),
    }
}

/// Parse a `GetManagedObjects` reply for every `org.bluez.Adapter1`.
fn parse_adapters(json: &str) -> Result<Vec<AdapterInfo>, String> {
    let root: Value =
        serde_json::from_str(json).map_err(|err| format!("invalid BlueZ JSON: {err}"))?;
    let objects = root
        .get("data")
        .and_then(Value::as_array)
        .and_then(|data| data.first())
        .and_then(Value::as_object)
        .ok_or_else(|| "BlueZ payload has no object map at data[0]".to_string())?;

    let mut adapters = Vec::new();
    for (path, object) in objects {
        let Some(adapter) = object.get("org.bluez.Adapter1") else {
            continue;
        };
        let address = variant_string(adapter, "Address").unwrap_or_else(|| "?".to_string());
        let name = variant_string(adapter, "Alias").or_else(|| variant_string(adapter, "Name"));
        let le = variant_string_array(adapter, "SupportedTechnologies").map(|technologies| {
            technologies
                .iter()
                .any(|tech| tech.eq_ignore_ascii_case("le"))
        });
        adapters.push(AdapterInfo {
            path: path.clone(),
            address,
            name,
            le,
        });
    }
    Ok(adapters)
}

fn variant_value<'a>(object: &'a Value, name: &str) -> Option<&'a Value> {
    object.get(name)?.get("data")
}

fn variant_string(object: &Value, name: &str) -> Option<String> {
    variant_value(object, name)?.as_str().map(str::to_string)
}

fn variant_string_array(object: &Value, name: &str) -> Option<Vec<String>> {
    variant_value(object, name)?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect()
}

/// Evaluate all checks against the injected probes. Pure: no shelling out.
fn evaluate(probes: &dyn SystemProbes) -> Vec<CheckResult> {
    vec![
        check_uinput(probes),
        check_privacy_unit(probes),
        check_adapter(probes),
        check_discovery(probes),
        check_sudo_btmgmt(probes),
        check_legacy_bridge_units(probes),
        check_other_instances(probes),
        check_user_unit(probes),
        check_dsu_port(probes),
        check_config(probes),
        check_udev_rule(probes),
    ]
}

fn check_uinput(probes: &dyn SystemProbes) -> CheckResult {
    match probes.uinput() {
        Ok(()) => CheckResult {
            name: "uinput",
            status: Status::Pass,
            detail: "/dev/uinput is writable".to_string(),
            remedy: String::new(),
        },
        Err(err) => CheckResult {
            name: "uinput",
            status: Status::Fail,
            detail: format!("/dev/uinput is not writable: {err}"),
            remedy: "install the /dev/uinput uaccess rule or add this user to the input group, \
                     then re-login; verify with `test -w /dev/uinput`"
                .to_string(),
        },
    }
}

fn check_privacy_unit(probes: &dyn SystemProbes) -> CheckResult {
    let unit = probes.privacy_unit();
    match (&unit.enabled, &unit.active) {
        (UnitEnabled::Enabled, UnitActive::Active) => CheckResult {
            name: "le-privacy unit",
            status: Status::Pass,
            detail: format!("switch2d-disable-le-privacy.service: {}", unit.detail()),
            remedy: String::new(),
        },
        (UnitEnabled::NotFound, _) => CheckResult {
            name: "le-privacy unit",
            status: Status::Fail,
            detail: format!("switch2d-disable-le-privacy.service: {}", unit.detail()),
            remedy: "sudo install -Dm755 system/disable-le-privacy.sh /usr/local/bin/disable-le-privacy.sh; \
                     sudo install -Dm644 system/switch2d-disable-le-privacy.service /etc/systemd/system/switch2d-disable-le-privacy.service; \
                     sudo systemctl daemon-reload; \
                     sudo systemctl enable --now switch2d-disable-le-privacy.service"
                .to_string(),
        },
        (UnitEnabled::Unknown(_), _) | (_, UnitActive::Unknown(_)) => CheckResult {
            name: "le-privacy unit",
            status: Status::Skip,
            detail: format!(
                "could not query switch2d-disable-le-privacy.service: {}",
                unit.detail()
            ),
            remedy: "run `systemctl status switch2d-disable-le-privacy.service` as the logged-in user"
                .to_string(),
        },
        _ => CheckResult {
            name: "le-privacy unit",
            status: Status::Fail,
            detail: format!("switch2d-disable-le-privacy.service: {}", unit.detail()),
            remedy: "sudo systemctl enable --now switch2d-disable-le-privacy.service (reconnects fail while LE privacy is on)"
                .to_string(),
        },
    }
}

fn check_adapter(probes: &dyn SystemProbes) -> CheckResult {
    match probes.adapter() {
        AdapterProbe::Found(adapters) => {
            let Some(adapter) = adapters.first() else {
                return CheckResult {
                    name: "bluetooth adapter",
                    status: Status::Fail,
                    detail: "BlueZ reported no org.bluez.Adapter1".to_string(),
                    remedy: "check `systemctl status bluetooth` and `bluetoothctl list`"
                        .to_string(),
                };
            };
            let label = adapter.name.clone().unwrap_or_else(|| adapter.path.clone());
            let detail = format!(
                "{} ({}) [MAC partially redacted]",
                label,
                redact_mac(&adapter.address)
            );
            if adapter.le == Some(false) {
                CheckResult {
                    name: "bluetooth adapter",
                    status: Status::Warn,
                    detail: format!("{detail} does not list LE in SupportedTechnologies"),
                    remedy: "use an LE-capable adapter/firmware; Switch 2 controllers need BLE"
                        .to_string(),
                }
            } else {
                CheckResult {
                    name: "bluetooth adapter",
                    status: Status::Pass,
                    detail,
                    remedy: String::new(),
                }
            }
        }
        AdapterProbe::None => CheckResult {
            name: "bluetooth adapter",
            status: Status::Fail,
            detail: "BlueZ reported no org.bluez.Adapter1".to_string(),
            remedy: "check `systemctl status bluetooth` and `bluetoothctl list`".to_string(),
        },
        AdapterProbe::Error(err) => CheckResult {
            name: "bluetooth adapter",
            status: Status::Skip,
            detail: format!("could not query BlueZ: {err}"),
            remedy: "install/start BlueZ (busctl) and re-run `switch2d doctor`".to_string(),
        },
    }
}

fn check_discovery(probes: &dyn SystemProbes) -> CheckResult {
    match probes.discovery() {
        DiscoveryProbe::Accepted(counts) => CheckResult {
            name: "discovery",
            status: Status::Pass,
            detail: format!(
                "StartDiscovery accepted (adapter is discoverable); {}",
                counts.detail()
            ),
            remedy: String::new(),
        },
        DiscoveryProbe::InProgress(counts) => CheckResult {
            name: "discovery",
            status: Status::Pass,
            detail: format!(
                "StartDiscovery already in progress (InProgress); {}",
                counts.detail()
            ),
            remedy: String::new(),
        },
        DiscoveryProbe::Failed { counts, error } => CheckResult {
            name: "discovery",
            status: Status::Warn,
            detail: format!(
                "StartDiscovery failed: {error} ({}); the daemon can still work via its timed fallback, but advert-based wake will not",
                counts.detail()
            ),
            remedy: "restart BlueZ (`sudo systemctl restart bluetooth`) and re-run `switch2d doctor`; if it still fails, check `busctl --system call org.bluez / org.freedesktop.DBus.ObjectManager GetManagedObjects --json=short`"
                .to_string(),
        },
        DiscoveryProbe::Error(err) => CheckResult {
            name: "discovery",
            status: Status::Warn,
            detail: format!("could not query BlueZ discovery state: {err}"),
            remedy: "install/start BlueZ and re-run `switch2d doctor`".to_string(),
        },
    }
}

fn check_sudo_btmgmt(probes: &dyn SystemProbes) -> CheckResult {
    match probes.sudo_btmgmt() {
        SudoBtmgmtProbe::Allowed => CheckResult {
            name: "sudo btmgmt",
            status: Status::Pass,
            detail: "sudo -n -l /usr/bin/btmgmt is allowed (no password prompt)".to_string(),
            remedy: String::new(),
        },
        SudoBtmgmtProbe::Denied(reason) => CheckResult {
            name: "sudo btmgmt",
            status: Status::Fail,
            detail: format!("passwordless sudo for /usr/bin/btmgmt is missing: {reason}"),
            remedy: "echo \"$USER ALL=(root) NOPASSWD: /usr/bin/btmgmt -i 0 stop-find -l, /usr/bin/btmgmt -i 0 stop-find\" | sudo tee /etc/sudoers.d/switch2d-btmgmt; sudo chmod 0440 /etc/sudoers.d/switch2d-btmgmt"
                .to_string(),
        },
        SudoBtmgmtProbe::Error(err) => CheckResult {
            name: "sudo btmgmt",
            status: Status::Fail,
            detail: format!("could not probe sudo rule: {err}"),
            remedy: "install sudo and add the /etc/sudoers.d/switch2d-btmgmt rule (`$USER ALL=(root) NOPASSWD: /usr/bin/btmgmt -i 0 stop-find -l, /usr/bin/btmgmt -i 0 stop-find`)"
                .to_string(),
        },
    }
}

fn check_legacy_bridge_units(probes: &dyn SystemProbes) -> CheckResult {
    let units = match probes.legacy_bridge_units() {
        Ok(units) => units,
        Err(err) => {
            return CheckResult {
                name: "legacy bridge units",
                status: Status::Skip,
                detail: format!("could not query --user systemd: {err}"),
                remedy: "run `systemctl --user list-unit-files 'nso-gc*'` as the logged-in user"
                    .to_string(),
            };
        }
    };

    let mut offending = Vec::new();
    let mut unknown = Vec::new();
    for (name, unit) in &units {
        if matches!(unit.enabled, UnitEnabled::Unknown(_))
            || matches!(unit.active, UnitActive::Unknown(_))
        {
            unknown.push(format!("{name}: {}", unit.detail()));
        }
        if unit.enabled == UnitEnabled::Enabled || unit.active == UnitActive::Active {
            offending.push(*name);
        }
    }

    if !offending.is_empty() {
        return CheckResult {
            name: "legacy bridge units",
            status: Status::Warn,
            detail: format!("enabled or active: {}", offending.join(", ")),
            remedy: format!("systemctl --user disable --now {}", offending.join(" ")),
        };
    }

    if !unknown.is_empty() {
        return CheckResult {
            name: "legacy bridge units",
            status: Status::Skip,
            detail: format!("could not determine: {}", unknown.join("; ")),
            remedy: "run `systemctl --user list-unit-files 'nso-gc*'` as the logged-in user"
                .to_string(),
        };
    }

    CheckResult {
        name: "legacy bridge units",
        status: Status::Pass,
        detail: "no nso-gc unit is enabled or active".to_string(),
        remedy: String::new(),
    }
}

fn check_other_instances(probes: &dyn SystemProbes) -> CheckResult {
    match probes.other_switch2d_pids() {
        Ok(pids) if pids.is_empty() => CheckResult {
            name: "running instances",
            status: Status::Pass,
            detail: "no other switch2d process".to_string(),
            remedy: String::new(),
        },
        Ok(pids) => CheckResult {
            name: "running instances",
            status: Status::Warn,
            detail: format!("other switch2d process(es): {pids:?}"),
            remedy:
                "stop the other instance(s) (`kill <pids>` or `systemctl --user stop switch2d`)"
                    .to_string(),
        },
        Err(err) => CheckResult {
            name: "running instances",
            status: Status::Skip,
            detail: format!("could not run pgrep: {err}"),
            remedy: "install procps/pgrep and re-run `switch2d doctor`".to_string(),
        },
    }
}

/// Show the OUI and last two octets only, e.g. `8C:17:..:D6:1E`.
fn redact_mac(mac: &str) -> String {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() == 6 {
        format!("{}:{}:..:{}:{}", parts[0], parts[1], parts[4], parts[5])
    } else {
        "<redacted>".to_string()
    }
}

/// First command word of the first `ExecStart=` line, with `%h`/`%u` expanded.
fn exec_start_path(unit_text: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let user = std::env::var("USER").unwrap_or_default();
    for line in unit_text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("ExecStart=") else {
            continue;
        };
        let command = rest.trim().split_whitespace().next()?;
        let command = command.strip_prefix('-').unwrap_or(command);
        let expanded = command.replace("%h", &home).replace("%u", &user);
        return Some(PathBuf::from(expanded));
    }
    None
}

fn check_user_unit(probes: &dyn SystemProbes) -> CheckResult {
    let status = match probes.user_unit_status() {
        Ok(status) => status,
        Err(err) => {
            return CheckResult {
                name: "user unit",
                status: Status::Skip,
                detail: format!("could not query switch2d.service: {err}"),
                remedy: "run `systemctl --user status switch2d.service` as the logged-in user"
                    .to_string(),
            };
        }
    };
    if status.enabled == UnitEnabled::NotFound {
        return CheckResult {
            name: "user unit",
            status: Status::Pass,
            detail: "not installed (optional; run ./install.sh --service to enable at login)"
                .to_string(),
            remedy: String::new(),
        };
    }

    let text = match probes.user_unit_text() {
        Ok(text) => text,
        Err(err) => {
            return CheckResult {
                name: "user unit",
                status: Status::Skip,
                detail: format!("could not read switch2d.service: {err}"),
                remedy: "run `systemctl --user cat switch2d.service` as the logged-in user"
                    .to_string(),
            };
        }
    };

    let mut reasons = Vec::new();
    if status.active == UnitActive::Active {
        reasons.push("unit is active".to_string());
    }

    match exec_start_path(&text) {
        Some(path) if !path.exists() => reasons.push(format!(
            "ExecStart path {} does not exist (stale install)",
            path.display()
        )),
        Some(path) => {
            if reasons.is_empty() {
                return CheckResult {
                    name: "user unit",
                    status: Status::Pass,
                    detail: format!(
                        "switch2d.service inactive; ExecStart path {} exists",
                        path.display()
                    ),
                    remedy: String::new(),
                };
            }
            reasons.push(format!("ExecStart path {} exists", path.display()));
        }
        None => reasons.push("no ExecStart= line".to_string()),
    }

    CheckResult {
        name: "user unit",
        status: Status::Warn,
        detail: format!("switch2d.service: {}", reasons.join("; ")),
        remedy: "systemctl --user daemon-reload and update/stop switch2d.service so it points at the installed binary"
            .to_string(),
    }
}

fn check_dsu_port(probes: &dyn SystemProbes) -> CheckResult {
    match probes.dsu_port_bound() {
        Ok(false) => CheckResult {
            name: "DSU port",
            status: Status::Pass,
            detail: "127.0.0.1:26760 is free".to_string(),
            remedy: String::new(),
        },
        Ok(true) => CheckResult {
            name: "DSU port",
            status: Status::Warn,
            detail: "127.0.0.1:26760 is already in use".to_string(),
            remedy: "stop the other DSU server or accept that motion is unavailable (controller input still works)"
                .to_string(),
        },
        Err(err) => CheckResult {
            name: "DSU port",
            status: Status::Skip,
            detail: format!("could not probe 127.0.0.1:26760: {err}"),
            remedy: "check local networking and re-run `switch2d doctor`".to_string(),
        },
    }
}

fn check_config(probes: &dyn SystemProbes) -> CheckResult {
    let path = DaemonConfig::config_path();
    match probes.config() {
        ConfigProbe::Loaded(count) => CheckResult {
            name: "config",
            status: Status::Pass,
            detail: format!("{} parsed; {count} bonded controller(s)", path.display()),
            remedy: String::new(),
        },
        ConfigProbe::Missing => CheckResult {
            name: "config",
            status: Status::Pass,
            detail: format!(
                "no config file at {} (0 bonded controllers)",
                path.display()
            ),
            remedy: String::new(),
        },
        ConfigProbe::Error(err) => CheckResult {
            name: "config",
            status: Status::Fail,
            detail: format!("config does not parse: {err}"),
            remedy: format!(
                "fix or remove {} (the daemon falls back to defaults while it is malformed)",
                path.display()
            ),
        },
    }
}

fn check_udev_rule(probes: &dyn SystemProbes) -> CheckResult {
    match probes.udev_rule() {
        Some(path) => CheckResult {
            name: "udev uaccess rule",
            status: Status::Pass,
            detail: format!("found {}", path.display()),
            remedy: String::new(),
        },
        None => CheckResult {
            name: "udev uaccess rule",
            status: Status::Warn,
            detail: format!(
                "no switch2d udev rule found in {}",
                UACCESS_RULE_PATHS.join(", ")
            ),
            remedy: "install system/70-switch2d.rules and run `sudo udevadm control --reload-rules && sudo udevadm trigger` (uinput/evdev access and the IMU node need it)"
                .to_string(),
        },
    }
}

/// Format one check exactly as doctor prints it.
pub fn format_check(check: &CheckResult) -> String {
    let mut out = format!(
        "{}  {}: {}",
        check.status.as_str(),
        check.name,
        check.detail
    );
    if check.status != Status::Pass && !check.remedy.is_empty() {
        out.push_str(&format!("\n  fix: {}", check.remedy));
    }
    out
}

/// 0 when no check failed, 1 when any check is FAIL. WARN/SKIP do not fail.
pub fn exit_code(checks: &[CheckResult]) -> i32 {
    if checks.iter().any(|check| check.status == Status::Fail) {
        1
    } else {
        0
    }
}

/// Run the real checks, print them, and return the process exit code.
pub fn run() -> i32 {
    println!("switch2d doctor");
    let checks = evaluate(&RealProbes);
    for check in &checks {
        println!("{}", format_check(check));
    }
    exit_code(&checks)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProbes {
        privacy: SystemdUnit,
        user_unit: SystemdUnit,
        user_unit_text: Result<String, String>,
        discovery: DiscoveryProbe,
        sudo_btmgmt: SudoBtmgmtProbe,
        legacy_bridge_units: Result<Vec<(&'static str, SystemdUnit)>, String>,
    }

    impl Default for FakeProbes {
        fn default() -> Self {
            Self {
                privacy: SystemdUnit {
                    enabled: UnitEnabled::Enabled,
                    active: UnitActive::Active,
                },
                user_unit: SystemdUnit {
                    enabled: UnitEnabled::Disabled,
                    active: UnitActive::Inactive,
                },
                user_unit_text: Ok("[Service]\nExecStart=/bin/sh run\n".to_string()),
                discovery: DiscoveryProbe::Accepted(DiscoveryCounts {
                    devices: 2,
                    with_manufacturer: 1,
                }),
                sudo_btmgmt: SudoBtmgmtProbe::Allowed,
                legacy_bridge_units: Ok(Vec::new()),
            }
        }
    }

    impl SystemProbes for FakeProbes {
        fn uinput(&self) -> Result<(), String> {
            Ok(())
        }

        fn privacy_unit(&self) -> SystemdUnit {
            self.privacy.clone()
        }

        fn adapter(&self) -> AdapterProbe {
            AdapterProbe::Found(vec![AdapterInfo {
                path: "/org/bluez/hci0".to_string(),
                address: "AA:BB:CC:DD:EE:01".to_string(),
                name: Some("test adapter".to_string()),
                le: Some(true),
            }])
        }

        fn discovery(&self) -> DiscoveryProbe {
            self.discovery.clone()
        }

        fn sudo_btmgmt(&self) -> SudoBtmgmtProbe {
            self.sudo_btmgmt.clone()
        }

        fn legacy_bridge_units(&self) -> Result<Vec<(&'static str, SystemdUnit)>, String> {
            self.legacy_bridge_units.clone()
        }

        fn other_switch2d_pids(&self) -> Result<Vec<u32>, String> {
            Ok(Vec::new())
        }

        fn user_unit_status(&self) -> Result<SystemdUnit, String> {
            Ok(self.user_unit.clone())
        }

        fn user_unit_text(&self) -> Result<String, String> {
            self.user_unit_text.clone()
        }

        fn dsu_port_bound(&self) -> Result<bool, String> {
            Ok(false)
        }

        fn config(&self) -> ConfigProbe {
            ConfigProbe::Missing
        }

        fn udev_rule(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/etc/udev/rules.d/70-switch2d.rules"))
        }
    }

    fn check_named<'a>(checks: &'a [CheckResult], name: &str) -> &'a CheckResult {
        checks
            .iter()
            .find(|check| check.name == name)
            .unwrap_or_else(|| panic!("missing check {name}"))
    }

    #[test]
    fn format_pass_has_exact_prefix_and_no_fix_line() {
        let check = CheckResult {
            name: "uinput",
            status: Status::Pass,
            detail: "/dev/uinput is writable".to_string(),
            remedy: "ignored for PASS".to_string(),
        };
        assert_eq!(
            format_check(&check),
            "PASS  uinput: /dev/uinput is writable"
        );
    }

    #[test]
    fn format_warn_and_fail_add_one_indented_fix_line() {
        let warn = CheckResult {
            name: "DSU port",
            status: Status::Warn,
            detail: "127.0.0.1:26760 is already in use".to_string(),
            remedy: "stop the other DSU server".to_string(),
        };
        assert_eq!(
            format_check(&warn),
            "WARN  DSU port: 127.0.0.1:26760 is already in use\n  fix: stop the other DSU server"
        );

        let fail = CheckResult {
            name: "uinput",
            status: Status::Fail,
            detail: "not writable".to_string(),
            remedy: "fix permissions".to_string(),
        };
        assert_eq!(
            format_check(&fail),
            "FAIL  uinput: not writable\n  fix: fix permissions"
        );
    }

    #[test]
    fn exit_code_is_one_only_for_fail() {
        let pass = CheckResult {
            name: "a",
            status: Status::Pass,
            detail: String::new(),
            remedy: String::new(),
        };
        let warn = CheckResult {
            status: Status::Warn,
            ..pass.clone()
        };
        let skip = CheckResult {
            status: Status::Skip,
            ..pass.clone()
        };
        let fail = CheckResult {
            status: Status::Fail,
            ..pass.clone()
        };

        assert_eq!(exit_code(&[pass.clone()]), 0);
        assert_eq!(exit_code(&[pass.clone(), warn, skip]), 0);
        assert_eq!(exit_code(&[pass, fail]), 1);
    }

    #[test]
    fn missing_privacy_unit_fails_with_install_commands() {
        let probes = FakeProbes {
            privacy: SystemdUnit {
                enabled: UnitEnabled::NotFound,
                active: UnitActive::Unknown("unknown".to_string()),
            },
            ..FakeProbes::default()
        };
        let checks = evaluate(&probes);
        let privacy = check_named(&checks, "le-privacy unit");

        assert_eq!(privacy.status, Status::Fail);
        assert!(
            privacy.detail.contains("not-found"),
            "detail must name the missing unit state: {}",
            privacy.detail
        );
        assert!(privacy
            .remedy
            .contains("install -Dm755 system/disable-le-privacy.sh"));
        assert!(privacy
            .remedy
            .contains("install -Dm644 system/switch2d-disable-le-privacy.service"));
        assert!(privacy
            .remedy
            .contains("enable --now switch2d-disable-le-privacy.service"));
        assert_eq!(exit_code(&checks), 1);
    }

    #[test]
    fn mac_display_is_partially_redacted() {
        assert_eq!(redact_mac("8C:17:59:41:D6:1E"), "8C:17:..:D6:1E");
        assert_eq!(redact_mac("not-a-mac"), "<redacted>");
    }

    #[test]
    fn active_user_unit_warns() {
        let probes = FakeProbes {
            user_unit: SystemdUnit {
                enabled: UnitEnabled::Enabled,
                active: UnitActive::Active,
            },
            ..FakeProbes::default()
        };
        let checks = evaluate(&probes);
        let unit = check_named(&checks, "user unit");
        assert_eq!(unit.status, Status::Warn);
        assert!(unit.detail.contains("unit is active"), "{}", unit.detail);
    }

    #[test]
    fn stale_exec_start_warns() {
        let probes = FakeProbes {
            user_unit_text: Ok(
                "[Service]\nExecStart=/definitely/missing/switch2d run\n".to_string()
            ),
            ..FakeProbes::default()
        };
        let checks = evaluate(&probes);
        let unit = check_named(&checks, "user unit");
        assert_eq!(unit.status, Status::Warn);
        assert!(
            unit.detail.contains("does not exist (stale install)"),
            "{}",
            unit.detail
        );
    }

    #[test]
    fn correct_user_unit_passes() {
        let probes = FakeProbes::default();
        let checks = evaluate(&probes);
        let unit = check_named(&checks, "user unit");
        assert_eq!(unit.status, Status::Pass);
        assert!(unit.detail.contains("ExecStart path /bin/sh exists"));
    }

    #[test]
    fn discovery_in_progress_maps_to_pass() {
        let probes = FakeProbes {
            discovery: DiscoveryProbe::InProgress(DiscoveryCounts {
                devices: 3,
                with_manufacturer: 2,
            }),
            ..FakeProbes::default()
        };
        let checks = evaluate(&probes);
        let discovery = check_named(&checks, "discovery");
        assert_eq!(discovery.status, Status::Pass);
        assert!(
            discovery.detail.contains("InProgress"),
            "{}",
            discovery.detail
        );
        assert!(
            discovery
                .detail
                .contains("Device1 objects: 3, with ManufacturerData: 2"),
            "{}",
            discovery.detail
        );
    }

    #[test]
    fn static_units_do_not_warn() {
        for raw in ["static", "indirect", "alias", "generated"] {
            assert_eq!(
                parse_enabled(raw),
                UnitEnabled::Disabled,
                "{raw} must not count as enabled"
            );
        }

        let probes = FakeProbes {
            legacy_bridge_units: Ok(vec![(
                "nso-gc.service",
                SystemdUnit {
                    enabled: parse_enabled("static"),
                    active: UnitActive::Inactive,
                },
            )]),
            ..FakeProbes::default()
        };
        let checks = evaluate(&probes);
        let legacy = check_named(&checks, "legacy bridge units");
        assert_eq!(legacy.status, Status::Pass, "{}", legacy.detail);
    }

    #[test]
    fn enabled_unit_warns_with_disable_command() {
        let probes = FakeProbes {
            legacy_bridge_units: Ok(vec![(
                "nso-gc.service",
                SystemdUnit {
                    enabled: UnitEnabled::Enabled,
                    active: UnitActive::Inactive,
                },
            )]),
            ..FakeProbes::default()
        };
        let checks = evaluate(&probes);
        let legacy = check_named(&checks, "legacy bridge units");
        assert_eq!(legacy.status, Status::Warn);
        assert!(
            legacy
                .remedy
                .contains("systemctl --user disable --now nso-gc.service"),
            "{}",
            legacy.remedy
        );
    }

    #[test]
    fn active_unit_warns_even_if_disabled() {
        let probes = FakeProbes {
            legacy_bridge_units: Ok(vec![(
                "nso-gc-watchdog.timer",
                SystemdUnit {
                    enabled: UnitEnabled::Disabled,
                    active: UnitActive::Active,
                },
            )]),
            ..FakeProbes::default()
        };
        let checks = evaluate(&probes);
        let legacy = check_named(&checks, "legacy bridge units");
        assert_eq!(legacy.status, Status::Warn);
        assert!(
            legacy
                .remedy
                .contains("systemctl --user disable --now nso-gc-watchdog.timer"),
            "{}",
            legacy.remedy
        );
    }

    #[test]
    fn missing_user_unit_passes_as_optional() {
        let probes = FakeProbes {
            user_unit: SystemdUnit {
                enabled: UnitEnabled::NotFound,
                active: UnitActive::Inactive,
            },
            user_unit_text: Err("No files found for switch2d.service.".to_string()),
            ..FakeProbes::default()
        };
        let checks = evaluate(&probes);
        let unit = check_named(&checks, "user unit");
        assert_eq!(unit.status, Status::Pass);
        assert!(
            unit.detail
                .contains("not installed (optional; run ./install.sh --service"),
            "{}",
            unit.detail
        );
    }

    #[test]
    fn unreadable_user_unit_skips() {
        let probes = FakeProbes {
            user_unit_text: Err("permission denied".to_string()),
            ..FakeProbes::default()
        };
        let checks = evaluate(&probes);
        let unit = check_named(&checks, "user unit");
        assert_eq!(unit.status, Status::Skip);
        assert!(unit.detail.contains("permission denied"), "{}", unit.detail);
    }

    #[test]
    fn active_legacy_watchdog_warns_with_disable_command() {
        let probes = FakeProbes {
            legacy_bridge_units: Ok(vec![(
                "nso-gc-watchdog.timer",
                SystemdUnit {
                    enabled: UnitEnabled::Enabled,
                    active: UnitActive::Active,
                },
            )]),
            ..FakeProbes::default()
        };
        let checks = evaluate(&probes);
        let legacy = check_named(&checks, "legacy bridge units");
        assert_eq!(legacy.status, Status::Warn);
        assert!(
            legacy.detail.contains("nso-gc-watchdog.timer"),
            "{}",
            legacy.detail
        );
        assert!(
            legacy
                .remedy
                .contains("systemctl --user disable --now nso-gc-watchdog.timer"),
            "{}",
            legacy.remedy
        );
    }

    #[test]
    fn adapter_parser_reads_bluez_json() {
        let json = r#"{
            "type":"a{oa{sa{sv}}}",
            "data":[{
                "/org/bluez/hci0":{
                    "org.bluez.Adapter1":{
                        "Address":{"type":"s","data":"AA:BB:CC:DD:EE:01"},
                        "Alias":{"type":"s","data":"test adapter"},
                        "SupportedTechnologies":{"type":"as","data":["bredr","le"]}
                    }
                }
            }]
        }"#;
        let adapters = parse_adapters(json).expect("parse adapters");
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].address, "AA:BB:CC:DD:EE:01");
        assert_eq!(adapters[0].le, Some(true));
    }
}
