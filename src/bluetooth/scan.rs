//! Advertisement bookkeeping and the privileged scan-control seam.
//!
//! [`SeenAdverts`] mirrors legacy `_last_seen` (`bridge.py`): a map keyed by
//! uppercase MAC recording `(mode, seen_at)` only for adverts that pass the
//! host-gating policy, with wake/pairing TTLs applied at selection and sweep
//! time. It is pure bookkeeping — the caller supplies the monotonic clock, so
//! the whole state machine is unit-testable without a radio.
//!
//! [`ShellScanControl`] is the ADR-2 implementation of [`ScanControl`].
//!
//! Discovery start/stop goes through the unprivileged BlueZ D-Bus API
//! (`busctl call ... org.bluez.Adapter1 StartDiscovery/StopDiscovery`), which
//! returns immediately and is exactly what BlueZ uses internally. We no longer
//! shell out to `bluetoothctl` for discovery at all: `busctl` replaces both the
//! old `bluetoothctl scan on/off` calls (ADR-6).
//!
//! The privileged `btmgmt -i N stop-find -l` + `stop-find` pair remains: that
//! part must reach the HCI layer to clear a scan another client left running,
//! and it is short-lived, so the 1.5 s child deadline is correct for it.
//! `btmgmt find -l` is deliberately *not* used for discovery — it is a
//! long-running command and would always be killed at the deadline.
//!
//! Every child gets a 1.5 s deadline and is killed and reaped on expiry: a hung
//! helper must never block the hub.

use std::collections::HashMap;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use tracing::debug;

use crate::protocol::advert::{
    accepts, AcceptPolicy, AdvertInfo, HostBinding, PAIRING_TTL, WAKE_TTL,
};

/// Why a controller was last seen advertising.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvertMode {
    /// Button press while already bonded to this adapter.
    Wake,
    /// Sync held: the pad is advertising for pairing.
    Pairing,
}

/// A recorded advertisement that the hub may dial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingConnect {
    /// Uppercase MAC address.
    pub mac: String,
    pub mode: AdvertMode,
    /// Monotonic timestamp of the most recent acceptable advert.
    pub seen_at: Instant,
    /// TTL for `mode` at the time it was recorded.
    pub ttl: Duration,
}

impl PendingConnect {
    /// Expired only when strictly older than the TTL; a future `seen_at` (clock
    /// injection in tests) counts as fresh rather than panicking.
    fn is_expired(&self, now: Instant) -> bool {
        match now.checked_duration_since(self.seen_at) {
            Some(elapsed) => elapsed > self.ttl,
            None => false,
        }
    }
}

/// One device as reported by an [`AdvertSource`].
///
/// `advert` is `None` whenever the manufacturer block is missing or too short,
/// and callers must treat that as wake-compatible only for a known bonded MAC
/// (see [`crate::protocol::advert::accepts`]).
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceAdvert {
    /// Device MAC address as reported by the backend (usually uppercase).
    pub address: String,
    /// Backend view of the link state.
    pub connected: bool,
    /// Latest RSSI in dBm, when the backend has one.
    pub rssi: Option<i16>,
    /// Decoded Nintendo advertisement, if the block was usable.
    pub advert: Option<AdvertInfo>,
}

impl DeviceAdvert {
    /// Whether this entry reflects a live advertisement rather than a cached
    /// BlueZ property set.
    ///
    /// BlueZ keeps `Device1` properties after a pad stops advertising, but only
    /// sets `RSSI` from live adverts, so a missing or `-127` sentinel RSSI means
    /// stale. The hub, not this module, decides eligibility.
    pub fn is_live(&self) -> bool {
        matches!(self.rssi, Some(rssi) if rssi > -127)
    }
}

/// A source of device advertisements.
pub trait AdvertSource: Send {
    /// Snapshot of every device currently known to the backend.
    fn devices(&mut self) -> io::Result<Vec<DeviceAdvert>>;
}

/// Recently seen controllers keyed by uppercase MAC (legacy `_last_seen`).
#[derive(Debug, Clone, Default)]
pub struct SeenAdverts {
    pending: HashMap<String, PendingConnect>,
}

impl SeenAdverts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an advert for `address` if it is acceptable.
    ///
    /// Returns `true` when recorded. `advert == None` means the advertisement
    /// carried no usable Nintendo block: that is wake-compatible, but only
    /// because callers already know `address` is a bonded MAC.
    ///
    /// A rejected advert neither creates nor refreshes an entry; an accepted
    /// one replaces any earlier entry for the same MAC.
    pub fn record(
        &mut self,
        advert: Option<&AdvertInfo>,
        address: &str,
        adapter_display_mac: &str,
        policy: AcceptPolicy,
        now: Instant,
    ) -> bool {
        if !accepts(advert, adapter_display_mac, policy) {
            return false;
        }

        let mode = match advert.map(|info| &info.host) {
            Some(HostBinding::Pairing) => AdvertMode::Pairing,
            _ => AdvertMode::Wake,
        };
        let ttl = match mode {
            AdvertMode::Wake => WAKE_TTL,
            AdvertMode::Pairing => PAIRING_TTL,
        };

        let mac = address.to_ascii_uppercase();
        self.pending.insert(
            mac.clone(),
            PendingConnect {
                mac,
                mode,
                seen_at: now,
                ttl,
            },
        );
        true
    }

    /// Disconnected, unexpired entries, newest advert first; ties broken by MAC
    /// for determinism.
    pub fn select_pending(
        &self,
        now: Instant,
        is_connected: &mut dyn FnMut(&str) -> bool,
    ) -> Vec<PendingConnect> {
        let mut pending: Vec<PendingConnect> = self
            .pending
            .values()
            .filter(|entry| !entry.is_expired(now) && !is_connected(&entry.mac))
            .cloned()
            .collect();

        pending.sort_by(|a, b| b.seen_at.cmp(&a.seen_at).then_with(|| a.mac.cmp(&b.mac)));
        pending
    }

    /// Drop entries whose TTL has expired at `now`.
    pub fn sweep(&mut self, now: Instant) {
        self.pending.retain(|_, entry| !entry.is_expired(now));
    }

    /// Forget a MAC (case-insensitive).
    pub fn forget(&mut self, mac: &str) {
        self.pending.remove(&mac.to_ascii_uppercase());
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Privileged discovery control, behind a seam so hub logic never spawns
/// processes itself (ADR-2).
pub trait ScanControl: Send + Sync {
    /// Stop competing discovery clients and clear any stuck LE scan.
    fn prepare_global(&self) -> io::Result<()>;
    /// Start an LE scan that reports advertisements.
    fn start_le_scan(&self) -> io::Result<()>;
    /// Stop the LE scan. Rate-limited unless `force`.
    fn stop_le_scan(&self, force: bool) -> io::Result<()>;
    /// Remove a BlueZ device object.
    ///
    /// The caller must never invoke this while a live session for `mac`
    /// exists: removing the device object tears down the connection.
    fn remove_bluez_device(&self, mac: &str) -> io::Result<()>;
}

/// Minimum spacing between real `stop_le_scan` invocations (legacy 350 ms).
const MIN_STOP_INTERVAL: Duration = Duration::from_millis(350);
/// Deadline for any single child process.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(1500);
/// Poll interval while waiting for a child to exit.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// [`ScanControl`] implemented with the proven `busctl` / `sudo btmgmt` /
/// `bluetoothctl` shell-outs.
#[derive(Debug)]
pub struct ShellScanControl {
    hci_index: u8,
    last_stop: Mutex<Option<Instant>>,
}

impl ShellScanControl {
    pub fn new(hci_index: u8) -> Self {
        Self {
            hci_index,
            last_stop: Mutex::new(None),
        }
    }

    /// Build from `NGC_HCI` (default adapter index 0).
    pub fn from_env() -> Self {
        let hci_index = std::env::var("NGC_HCI")
            .ok()
            .and_then(|value| value.trim().parse::<u8>().ok())
            .unwrap_or(0);
        Self::new(hci_index)
    }

    pub fn hci_index(&self) -> u8 {
        self.hci_index
    }

    /// `busctl call ... org.bluez.Adapter1 <method>` on this adapter.
    fn adapter_call(&self, method: &str) -> io::Result<()> {
        let path = format!("/org/bluez/hci{}", self.hci_index);
        run_command(Command::new("busctl").args([
            "call",
            "org.bluez",
            path.as_str(),
            "org.bluez.Adapter1",
            method,
        ]))
        .map(|_| ())
    }
}

impl ScanControl for ShellScanControl {
    fn prepare_global(&self) -> io::Result<()> {
        // Best-effort: a missing binary or a failing helper must never skip the
        // stop below, because an HCI scan left running breaks create-connection.
        if let Err(err) =
            run_command(Command::new("pkill").args(["-f", "decky-bluetooth-wake-control"]))
        {
            // pkill exits 1 when no matching helper is running: the normal case.
            debug!("pkill decky-bluetooth-wake-control: {}", err);
        }
        if let Err(err) = self.adapter_call("StopDiscovery") {
            // Fails when nothing is discovering: also the normal case.
            debug!(
                "Adapter1.StopDiscovery failed: {} (continuing to stop-find)",
                err
            );
        }
        self.stop_le_scan(true)
    }

    fn start_le_scan(&self) -> io::Result<()> {
        // D-Bus StartDiscovery returns immediately; `btmgmt find -l` would run
        // until killed and could never report success through our deadline.
        self.adapter_call("StartDiscovery")
    }

    fn stop_le_scan(&self, force: bool) -> io::Result<()> {
        let now = Instant::now();
        {
            let mut last_stop = self.last_stop.lock().unwrap();
            if !force {
                if let Some(last) = *last_stop {
                    if now
                        .checked_duration_since(last)
                        .is_some_and(|elapsed| elapsed < MIN_STOP_INTERVAL)
                    {
                        return Ok(());
                    }
                }
            }
            // Recorded before spawning: a failing command must not tight-loop.
            *last_stop = Some(now);
        }

        let index = self.hci_index.to_string();
        // `sudo -n` is mandatory: btmgmt stop-find needs CAP_NET_ADMIN, plain
        // btmgmt fails with EPERM and the HCI scan would keep running, breaking
        // create-connection (HANDOVER Bug 3). `-n` also guarantees a missing
        // sudoers rule fails fast instead of waiting on a password prompt for
        // the whole 1.5 s child deadline. The least-privilege rule lives in
        // /etc/sudoers.d/nso-gc-btmgmt.
        let attempts: [&[&str]; 2] = [
            &["-n", "btmgmt", "-i", index.as_str(), "stop-find", "-l"],
            &["-n", "btmgmt", "-i", index.as_str(), "stop-find"],
        ];

        let mut first_error = None;
        for args in attempts {
            if let Err(err) = run_command(Command::new("sudo").args(args)) {
                debug!("btmgmt {} failed: {}", args.join(" "), err);
                first_error.get_or_insert(err);
            }
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn remove_bluez_device(&self, mac: &str) -> io::Result<()> {
        run_command(Command::new("bluetoothctl").args(["remove", mac])).map(|_| ())
    }
}

/// Spawn `cmd` with a 1.5 s deadline, killing and reaping it on expiry, and
/// return its stdout.
///
/// Shared by [`ShellScanControl`] and `BluezAdvertSource`. Stdout is drained on
/// a helper thread so a chatty child can never block on a full pipe. Spawn
/// failures (e.g. missing binary) become `io::Error`s keeping the original
/// [`io::ErrorKind`]; a non-zero exit becomes [`io::Error::other`].
pub(crate) fn run_command(cmd: &mut Command) -> io::Result<Vec<u8>> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("failed to spawn {program}: {err}")))?;

    let reader = child.stdout.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut output = Vec::new();
            let _ = pipe.read_to_end(&mut output);
            output
        })
    });

    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = join_reader(reader);
                return if status.success() {
                    Ok(output)
                } else {
                    Err(io::Error::other(format!("{program} exited with {status}")))
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    // Reap the killed child so it cannot linger as a zombie.
                    let _ = child.wait();
                    let _ = join_reader(reader);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("{program} timed out after {COMMAND_TIMEOUT:?}"),
                    ));
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_reader(reader);
                return Err(io::Error::new(
                    err.kind(),
                    format!("failed to poll {program}: {err}"),
                ));
            }
        }
    }
}

/// Collect a stdout-drain thread's output, tolerating a panicked reader.
fn join_reader(reader: Option<thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::advert::host_field_from_adapter;
    use crate::protocol::constants::{NINTENDO_VENDOR_ID, PRO_CONTROLLER2_PID};

    const ADAPTER: &str = "AA:BB:CC:DD:EE:FF";
    const OTHER_ADAPTER: &str = "11:22:33:44:55:66";

    fn info(address: &str, host: HostBinding) -> AdvertInfo {
        AdvertInfo {
            address: address.to_ascii_uppercase(),
            vid: NINTENDO_VENDOR_ID,
            pid: PRO_CONTROLLER2_PID,
            host,
        }
    }

    fn bonded_to(adapter: &str) -> HostBinding {
        HostBinding::Host(host_field_from_adapter(adapter).expect("valid adapter MAC"))
    }

    #[test]
    fn records_accepted_advert_and_sets_wake_mode() {
        let base = Instant::now();
        let mut seen = SeenAdverts::new();
        let advert = info("aa:bb:cc:dd:ee:01", bonded_to(ADAPTER));

        assert!(seen.record(
            Some(&advert),
            "aa:bb:cc:dd:ee:01",
            ADAPTER,
            AcceptPolicy::Wake,
            base
        ));
        assert_eq!(seen.len(), 1);
        assert!(!seen.is_empty());

        let pending = seen.select_pending(base, &mut |_| false);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].mac, "AA:BB:CC:DD:EE:01");
        assert_eq!(pending[0].mode, AdvertMode::Wake);
        assert_eq!(pending[0].ttl, WAKE_TTL);
        assert_eq!(pending[0].seen_at, base);
    }

    #[test]
    fn records_pairing_mode_with_45s_ttl() {
        let base = Instant::now();
        let mut seen = SeenAdverts::new();
        let advert = info("AA:BB:CC:DD:EE:02", HostBinding::Pairing);

        assert!(seen.record(
            Some(&advert),
            "AA:BB:CC:DD:EE:02",
            ADAPTER,
            AcceptPolicy::Pairing,
            base
        ));

        let pending = seen.select_pending(base, &mut |_| false);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].mode, AdvertMode::Pairing);
        assert_eq!(pending[0].ttl, PAIRING_TTL);
        assert_eq!(PAIRING_TTL, Duration::from_secs(45));
    }

    #[test]
    fn foreign_host_advert_is_not_recorded() {
        let base = Instant::now();
        let mut seen = SeenAdverts::new();
        let advert = info("AA:BB:CC:DD:EE:03", bonded_to(OTHER_ADAPTER));

        assert!(!seen.record(
            Some(&advert),
            "AA:BB:CC:DD:EE:03",
            ADAPTER,
            AcceptPolicy::Wake,
            base
        ));
        // Even an explicit pairing policy does not accept another host's pad.
        assert!(!seen.record(
            Some(&advert),
            "AA:BB:CC:DD:EE:03",
            ADAPTER,
            AcceptPolicy::Pairing,
            base
        ));
        assert!(seen.is_empty());
        assert!(seen.select_pending(base, &mut |_| false).is_empty());
    }

    #[test]
    fn absent_advert_is_recorded_as_wake() {
        let base = Instant::now();
        let mut seen = SeenAdverts::new();

        assert!(seen.record(None, "aa:bb:cc:dd:ee:04", ADAPTER, AcceptPolicy::Wake, base));

        let pending = seen.select_pending(base, &mut |_| false);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].mac, "AA:BB:CC:DD:EE:04");
        assert_eq!(pending[0].mode, AdvertMode::Wake);
        assert_eq!(pending[0].ttl, WAKE_TTL);
    }

    #[test]
    fn later_advert_refreshes_entry() {
        let base = Instant::now();
        let mut seen = SeenAdverts::new();
        let mac = "AA:BB:CC:DD:EE:05";
        let wake = info(mac, bonded_to(ADAPTER));
        let pairing = info(mac, HostBinding::Pairing);

        assert!(seen.record(Some(&wake), mac, ADAPTER, AcceptPolicy::Wake, base));
        let later = base + Duration::from_secs(2);
        assert!(seen.record(Some(&pairing), mac, ADAPTER, AcceptPolicy::Pairing, later));

        // Replaced, not duplicated; mode and timestamp both updated.
        assert_eq!(seen.len(), 1);
        let pending = seen.select_pending(later, &mut |_| false);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].seen_at, later);
        assert_eq!(pending[0].mode, AdvertMode::Pairing);
        assert_eq!(pending[0].ttl, PAIRING_TTL);

        // A rejected advert must not refresh the entry.
        let rejected_at = later + Duration::from_secs(1);
        let foreign = info(mac, bonded_to(OTHER_ADAPTER));
        assert!(!seen.record(
            Some(&foreign),
            mac,
            ADAPTER,
            AcceptPolicy::Wake,
            rejected_at
        ));
        let pending = seen.select_pending(rejected_at, &mut |_| false);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].seen_at, later);
        assert_eq!(pending[0].mode, AdvertMode::Pairing);
    }

    #[test]
    fn select_pending_excludes_connected() {
        let base = Instant::now();
        let mut seen = SeenAdverts::new();
        let first = info("AA:BB:CC:DD:EE:06", bonded_to(ADAPTER));
        let second = info("AA:BB:CC:DD:EE:07", bonded_to(ADAPTER));

        assert!(seen.record(
            Some(&first),
            &first.address,
            ADAPTER,
            AcceptPolicy::Wake,
            base
        ));
        assert!(seen.record(
            Some(&second),
            &second.address,
            ADAPTER,
            AcceptPolicy::Wake,
            base
        ));

        let pending = seen.select_pending(base, &mut |mac| {
            mac.eq_ignore_ascii_case("AA:BB:CC:DD:EE:06")
        });
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].mac, "AA:BB:CC:DD:EE:07");
    }

    #[test]
    fn select_pending_sorts_newest_first_with_deterministic_ties() {
        let base = Instant::now();
        let mut seen = SeenAdverts::new();
        let a = info("AA:BB:CC:DD:EE:0A", bonded_to(ADAPTER));
        let b = info("AA:BB:CC:DD:EE:0B", bonded_to(ADAPTER));
        let c = info("AA:BB:CC:DD:EE:0C", bonded_to(ADAPTER));

        assert!(seen.record(Some(&a), &a.address, ADAPTER, AcceptPolicy::Wake, base));
        assert!(seen.record(
            Some(&b),
            &b.address,
            ADAPTER,
            AcceptPolicy::Wake,
            base + Duration::from_secs(1)
        ));
        // Same timestamp as A: the MAC tie-break decides the order.
        assert!(seen.record(Some(&c), &c.address, ADAPTER, AcceptPolicy::Wake, base));

        let pending = seen.select_pending(base + Duration::from_secs(1), &mut |_| false);
        let order: Vec<&str> = pending.iter().map(|p| p.mac.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "AA:BB:CC:DD:EE:0B",
                "AA:BB:CC:DD:EE:0A",
                "AA:BB:CC:DD:EE:0C"
            ]
        );
    }

    #[test]
    fn expiry_boundary_exactly_ttl_is_pending() {
        let base = Instant::now();
        let mut seen = SeenAdverts::new();
        assert!(seen.record(None, "AA:BB:CC:DD:EE:08", ADAPTER, AcceptPolicy::Wake, base));

        let exactly_ttl = base + WAKE_TTL;
        assert_eq!(seen.select_pending(exactly_ttl, &mut |_| false).len(), 1);
        seen.sweep(exactly_ttl);
        assert_eq!(seen.len(), 1);

        let just_past = exactly_ttl + Duration::from_millis(1);
        assert!(seen.select_pending(just_past, &mut |_| false).is_empty());
        seen.sweep(just_past);
        assert!(seen.is_empty());
    }

    #[test]
    fn future_seen_at_does_not_panic() {
        let base = Instant::now();
        let mut seen = SeenAdverts::new();
        let future = base + Duration::from_secs(10);

        assert!(seen.record(
            None,
            "AA:BB:CC:DD:EE:09",
            ADAPTER,
            AcceptPolicy::Wake,
            future
        ));
        assert_eq!(seen.select_pending(base, &mut |_| false).len(), 1);
        seen.sweep(base);
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen.select_pending(future + WAKE_TTL, &mut |_| false).len(),
            1
        );
    }

    #[test]
    fn sweep_drops_only_expired() {
        let base = Instant::now();
        let mut seen = SeenAdverts::new();
        let wake_mac = "AA:BB:CC:DD:EE:10";
        let pairing_mac = "AA:BB:CC:DD:EE:11";
        let pairing = info(pairing_mac, HostBinding::Pairing);

        assert!(seen.record(None, wake_mac, ADAPTER, AcceptPolicy::Wake, base));
        let pairing_seen = base + Duration::from_secs(3);
        assert!(seen.record(
            Some(&pairing),
            pairing_mac,
            ADAPTER,
            AcceptPolicy::Pairing,
            pairing_seen
        ));
        assert_eq!(seen.len(), 2);

        // 5 s after base: wake (4 s) expired, pairing (45 s) still fresh.
        let now = base + Duration::from_secs(5);
        seen.sweep(now);
        assert_eq!(seen.len(), 1);
        let pending = seen.select_pending(now, &mut |_| false);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].mac, pairing_mac);
        assert_eq!(pending[0].mode, AdvertMode::Pairing);
    }

    #[test]
    fn forget_removes_entry() {
        let base = Instant::now();
        let mut seen = SeenAdverts::new();
        assert!(seen.record(None, "AA:BB:CC:DD:EE:12", ADAPTER, AcceptPolicy::Wake, base));
        assert!(seen.record(None, "AA:BB:CC:DD:EE:13", ADAPTER, AcceptPolicy::Wake, base));

        seen.forget("aa:bb:cc:dd:ee:12");
        assert_eq!(seen.len(), 1);
        let pending = seen.select_pending(base, &mut |_| false);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].mac, "AA:BB:CC:DD:EE:13");

        // Unknown MAC is a no-op.
        seen.forget("11:22:33:44:55:66");
        assert_eq!(seen.len(), 1);

        // Default/new start empty.
        let default = SeenAdverts::default();
        assert!(default.is_empty());
        assert_eq!(default.len(), 0);
    }
}
