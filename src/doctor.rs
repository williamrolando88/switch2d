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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigProbe {
    /// No config file at the canonical path; the count came from legacy import.
    Missing(usize),
    Loaded(usize),
    Error(String),
}

/// Raw environment probes. Tests inject a fake; production uses [`RealProbes`].
trait SystemProbes {
    fn uinput(&self) -> Result<(), String>;
    fn privacy_unit(&self) -> SystemdUnit;
    fn adapter(&self) -> AdapterProbe;
    fn other_switch2d_pids(&self) -> Result<Vec<u32>, String>;
    fn legacy_user_unit(&self) -> Result<SystemdUnit, String>;
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
        let output = match Command::new("busctl")
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
        {
            Ok(output) => output,
            Err(err) => return AdapterProbe::Error(format!("busctl: {err}")),
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return AdapterProbe::Error(format!(
                "busctl exited with {}: {}",
                output.status, stderr
            ));
        }

        match parse_adapters(&String::from_utf8_lossy(&output.stdout)) {
            Ok(adapters) if adapters.is_empty() => AdapterProbe::None,
            Ok(adapters) => AdapterProbe::Found(adapters),
            Err(err) => AdapterProbe::Error(err),
        }
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

    fn legacy_user_unit(&self) -> Result<SystemdUnit, String> {
        let enabled = systemctl_field("is-enabled", "switch2d.service", true)?;
        let active = systemctl_field("is-active", "switch2d.service", true)?;
        Ok(SystemdUnit {
            enabled: parse_enabled(&enabled),
            active: parse_active(&active),
        })
    }

    fn dsu_port_bound(&self) -> Result<bool, String> {
        match UdpSocket::bind(("127.0.0.1", 26760)) {
            Ok(_) => Ok(false),
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => Ok(true),
            Err(err) => Err(err.to_string()),
        }
    }

    fn config(&self) -> ConfigProbe {
        // The task wants the real loader's view; `load` also handles legacy
        // migration. Parsing the canonical file separately makes a malformed
        // JSON file an explicit FAIL instead of a silent default.
        let loaded = DaemonConfig::load();
        let count = loaded.controllers.len();
        let path = DaemonConfig::config_path();
        if !path.exists() {
            return ConfigProbe::Missing(count);
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
        "enabled" => UnitEnabled::Enabled,
        "disabled" => UnitEnabled::Disabled,
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
        check_other_instances(probes),
        check_legacy_unit(probes),
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
            let detail = format!("{} ({})", label, adapter.address);
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

fn check_legacy_unit(probes: &dyn SystemProbes) -> CheckResult {
    match probes.legacy_user_unit() {
        Ok(unit)
            if unit.enabled == UnitEnabled::Enabled && unit.active == UnitActive::Active =>
        {
            CheckResult {
                name: "legacy user unit",
                status: Status::Warn,
                detail: format!("switch2d.service: {}", unit.detail()),
                remedy: "systemctl --user disable --now switch2d.service (the old unit competes for the adapter)"
                    .to_string(),
            }
        }
        Ok(unit) => CheckResult {
            name: "legacy user unit",
            status: Status::Pass,
            detail: format!("switch2d.service: {}", unit.detail()),
            remedy: String::new(),
        },
        Err(err) => CheckResult {
            name: "legacy user unit",
            status: Status::Skip,
            detail: format!("could not query --user systemd: {err}"),
            remedy: "run `systemctl --user status switch2d.service` as the logged-in user"
                .to_string(),
        },
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
        ConfigProbe::Missing(count) => CheckResult {
            name: "config",
            status: Status::Pass,
            detail: format!(
                "no config at {}; {count} bonded controller(s) loaded from a legacy location",
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
    }

    impl Default for FakeProbes {
        fn default() -> Self {
            Self {
                privacy: SystemdUnit {
                    enabled: UnitEnabled::Enabled,
                    active: UnitActive::Active,
                },
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

        fn other_switch2d_pids(&self) -> Result<Vec<u32>, String> {
            Ok(Vec::new())
        }

        fn legacy_user_unit(&self) -> Result<SystemdUnit, String> {
            Ok(SystemdUnit {
                enabled: UnitEnabled::Disabled,
                active: UnitActive::Inactive,
            })
        }

        fn dsu_port_bound(&self) -> Result<bool, String> {
            Ok(false)
        }

        fn config(&self) -> ConfigProbe {
            ConfigProbe::Missing(0)
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
