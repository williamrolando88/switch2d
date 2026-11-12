//! Live monitor: connect to one controller and print exactly what the decoder
//! produces (the BLE half of the smoke test).
//!
//! The input callback runs on the ATT reader thread, so it only performs a
//! non-blocking `try_send` into a 64-slot channel (analysis §5.6: printing or
//! blocking there head-of-line-blocks command responses). Formatting and I/O
//! happen on the monitor thread.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::bluetooth::bluez::AdvertSnapshot;
use crate::controller::{
    BondPolicy, CalibratedInput, ConnectOptions, ControllerInputCallback, DialBudget,
    SwitchController,
};
use crate::protocol::constants::button_mask;
use crate::protocol::report::InputReport;
use crate::status::battery_pct_from_mv;

/// Header for `switch2d pair --hold`.
///
/// Kept pure so the LED-verification note and the player number are pinned by a
/// test rather than by a manual run (the hold loop itself needs hardware).
pub fn pair_hold_header(mac: &str, player: u8, seconds: u64) -> String {
    format!(
        "===========================================================\n\
         Holding the link to {mac} (Player {player}) for up to {seconds} s.\n\
         Note: the Player {player} LED is set for this session and goes out when\n\
         the link drops — this hold is the LED verification window.\n\
         Live decoded input follows, exactly like `switch2d monitor`.\n\
         Ctrl-C ends the hold early; the link then closes cleanly.\n\
         ==========================================================="
    )
}

/// One line for `switch2d advert`, built only from a BlueZ snapshot.
///
/// Nintendo advertisement (full form):
/// `<MAC> <public|random> pid=0x21xx <name> rssi=<n> host=<HH:..|pairing|unknown> vid=0x057E md=<hex>`
///
/// Everything else (used with `--all`): address, optional address type, name,
/// RSSI only.
pub fn advert_line_for(item: &AdvertSnapshot) -> String {
    let rssi = item
        .rssi
        .map(|value| value.to_string())
        .unwrap_or_else(|| "?".to_string());
    let name = item.name.as_deref().unwrap_or("<unknown>");

    let mut line = item.address.clone();
    if let Some(kind) = &item.address_type {
        line.push(' ');
        line.push_str(kind);
    }

    match &item.advert {
        Some(advert) => line.push_str(&format!(
            " pid=0x{:04X} {} rssi={} host={} vid=0x{:04X} md={}",
            advert.pid,
            name,
            rssi,
            host_field(&advert.host),
            advert.vid,
            manufacturer_hex(item.manufacturer_data.as_deref()),
        )),
        None => line.push_str(&format!(" {} rssi={}", name, rssi)),
    }
    line
}

/// Liveness rule for `switch2d advert` (deliberately **not** the hub's).
///
/// BlueZ clears the `RSSI` property when its own advertisement-monitor window
/// closes, but keeps the cached `ManufacturerData` precisely because it saw it.
/// For a diagnostic view, data presence is therefore a better "this pad is
/// here" signal than RSSI. Returns `None` when the entry should be printed, or
/// a human-readable reason when it should be skipped.
///
/// The hub keeps using RSSI-only liveness ([`DeviceAdvert::is_live`]): cached
/// properties must not re-arm a dial.
///
/// [`DeviceAdvert::is_live`]: crate::bluetooth::scan::DeviceAdvert::is_live
pub fn advert_skip_reason(item: &AdvertSnapshot) -> Option<&'static str> {
    let has_data = item.manufacturer_data.is_some();
    let has_rssi = matches!(item.rssi, Some(rssi) if rssi > -127);
    if has_data || has_rssi || item.connected {
        None
    } else {
        Some("no manufacturer data, no live rssi, not connected")
    }
}

/// Dedupe key for `switch2d advert`: MAC plus address type, so a pad that flips
/// `public` -> `random` is printed again instead of being swallowed as
/// "unchanged".
pub fn advert_dedupe_key(item: &AdvertSnapshot) -> String {
    format!(
        "{}|{}",
        item.address,
        item.address_type.as_deref().unwrap_or("")
    )
}

/// Bonded-host field in display order, or a state word.
fn host_field(host: &crate::protocol::advert::HostBinding) -> String {
    use crate::protocol::advert::HostBinding;
    match host {
        HostBinding::Pairing => "pairing".to_string(),
        HostBinding::Unknown => "unknown".to_string(),
        HostBinding::Host(wire) => wire
            .iter()
            .rev()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":"),
    }
}

/// Lower-case, separator-free hex of the manufacturer block (`-` when absent).
fn manufacturer_hex(data: Option<&[u8]>) -> String {
    match data {
        Some(bytes) if !bytes.is_empty() => bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        _ => "-".to_string(),
    }
}

/// Stable, ordered button table for the one-line report. Masks come from
/// `protocol::constants::button_mask`; the order is defined once here and is
/// intentionally a slice, not a map.
const BUTTON_ORDER: &[(u32, &str)] = &[
    (button_mask::A, "A"),
    (button_mask::B, "B"),
    (button_mask::X, "X"),
    (button_mask::Y, "Y"),
    (button_mask::UP, "UP"),
    (button_mask::DOWN, "DOWN"),
    (button_mask::LEFT, "LEFT"),
    (button_mask::RIGHT, "RIGHT"),
    (button_mask::PLUS, "PLUS"),
    (button_mask::MINUS, "MINUS"),
    (button_mask::HOME, "HOME"),
    (button_mask::CAPTURE, "CAPTURE"),
    (button_mask::L, "L"),
    (button_mask::R, "R"),
    (button_mask::ZL, "ZL"),
    (button_mask::ZR, "ZR"),
    (button_mask::L_STICK_BTN, "L_STICK"),
    (button_mask::R_STICK_BTN, "R_STICK"),
    (button_mask::SL_L, "SL_L"),
    (button_mask::SR_L, "SR_L"),
    (button_mask::SL_R, "SL_R"),
    (button_mask::SR_R, "SR_R"),
    (button_mask::C_BTN, "C"),
    (button_mask::GL, "GL"),
    (button_mask::GR, "GR"),
];

/// Configuration for [`run_monitor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorConfig {
    pub mac: String,
    pub adapter_mac: String,
    pub player: u8,
    /// Hard time limit. `0` means "until the link drops".
    pub seconds: u64,
}

/// One-second statistics window.
///
/// `observe()` records an input timestamp and updates `count`/`max_gap`; the
/// producer side of the channel adds any dropped frames to `dropped` (or via
/// [`WindowStats::record_dropped`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowStats {
    pub count: u64,
    pub dropped: u64,
    pub max_gap: Duration,
    last: Option<Instant>,
}

impl WindowStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one received report. The gap is measured from the previously
    /// observed report, so the first observation contributes no gap.
    pub fn observe(&mut self, now: Instant) {
        if let Some(previous) = self.last {
            let gap = now.saturating_duration_since(previous);
            if gap > self.max_gap {
                self.max_gap = gap;
            }
        }
        self.last = Some(now);
        self.count += 1;
    }

    /// Add dropped-frame count reported by the input callback.
    pub fn record_dropped(&mut self, dropped: u64) {
        self.dropped = self.dropped.saturating_add(dropped);
    }
}

/// Format one input report (`[#seq] btn=… raw=… cal=… lt=… rt=… bat=…`).
pub fn format_report(seq: u64, report: &InputReport, cal: &CalibratedInput) -> String {
    let buttons = pressed_button_names(report.buttons);
    let battery = match battery_pct_from_mv(report.battery_mv) {
        Some(pct) => format!("{pct}%"),
        None => "n/a%".to_string(),
    };

    // Raw IMU LSB samples -> g / deg/s with the same constants and signs the
    // DSU path uses (`.old/ngc/dsu.py:45-51`).
    let (accel, gyro) = crate::dsu::scale_motion(
        [
            f32::from(report.accel.0),
            f32::from(report.accel.1),
            f32::from(report.accel.2),
        ],
        [
            f32::from(report.gyro.0),
            f32::from(report.gyro.1),
            f32::from(report.gyro.2),
        ],
    );

    format!(
        "[#{seq}] btn={buttons} raw=<{lx},{ly}><{rx},{ry}> \
         cal=<{clx:.2},{cly:.2}><{crx:.2},{cry:.2}> \
         lt={lt} rt={rt} bat={bat_mv}mV {battery} \
         acc={ax:.2},{ay:.2},{az:.2}g gyro={gx:.2},{gy:.2},{gz:.2}dps",
        lx = report.left_stick_raw.0,
        ly = report.left_stick_raw.1,
        rx = report.right_stick_raw.0,
        ry = report.right_stick_raw.1,
        clx = cal.left.0,
        cly = cal.left.1,
        crx = cal.right.0,
        cry = cal.right.1,
        lt = cal.lt,
        rt = cal.rt,
        bat_mv = report.battery_mv,
        ax = accel[0],
        ay = accel[1],
        az = accel[2],
        gx = gyro[0],
        gy = gyro[1],
        gz = gyro[2],
    )
}

/// Format one per-second statistics line.
pub fn format_stats(stats: &WindowStats) -> String {
    format!(
        "[stats] reports={}/s dropped={} max_gap={}",
        stats.count,
        stats.dropped,
        stats.max_gap.as_millis()
    )
}

fn pressed_button_names(buttons: u32) -> String {
    let mut pressed = Vec::new();
    for &(mask, name) in BUTTON_ORDER {
        if buttons & mask != 0 {
            pressed.push(name);
        }
    }
    if pressed.is_empty() {
        "-".to_string()
    } else {
        pressed.join(",")
    }
}

fn flush_dropped(dropped: &AtomicU64, stats: &mut WindowStats) {
    stats.record_dropped(dropped.swap(0, Ordering::Relaxed));
}

fn print_handshake(cfg: &MonitorConfig, controller: &SwitchController) {
    let name = controller
        .info
        .as_ref()
        .map(|info| info.name())
        .unwrap_or("n/a");
    let product_id = controller
        .info
        .as_ref()
        .map(|info| format!("0x{:04X}", info.product_id))
        .unwrap_or_else(|| "n/a".to_string());
    // Neither the negotiated ATT MTU nor the calibration source is exposed by
    // SwitchController; print n/a instead of adding accessors this round.
    let bond = if controller.did_bond {
        "bonded"
    } else {
        "not-bonded"
    };
    println!(
        "[monitor] connected mac={} adapter={} player={} name={} pid={} mtu=n/a \
         cal=n/a bond={} seconds={}",
        cfg.mac, cfg.adapter_mac, cfg.player, name, product_id, bond, cfg.seconds
    );
}

/// Blocking live monitor: connect, print the handshake, then print one line per
/// input report and a per-second stats line.
///
/// Called from `spawn_blocking` by the CLI. A link drop prints
/// `[monitor] link dropped …` and returns `Ok(())`; only the initial connect
/// can return `Err`.
pub fn run_monitor(cfg: MonitorConfig) -> io::Result<()> {
    let (tx, rx) = mpsc::sync_channel::<InputReport>(64);
    let dropped = Arc::new(AtomicU64::new(0));
    let dropped_cb = Arc::clone(&dropped);

    // Reader-thread callback: never blocks, never prints.
    let callback: ControllerInputCallback = Box::new(move |report: &InputReport| {
        if tx.try_send(report.clone()).is_err() {
            dropped_cb.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Live monitor owns no session registry, so the dial envelope may drop the
    // BlueZ Device object unconditionally before dialling: that object is what
    // BlueZ/Steam otherwise race with our raw ATT connect (HANDOVER Bug 3/4,
    // analysis 8.7). `connect_session` also stops LE discovery before every
    // round and reports both the removal and the discovery outcome on failure.
    let controller = SwitchController::connect_session(
        &cfg.mac,
        &cfg.adapter_mac,
        ConnectOptions {
            player: cfg.player,
            bond: BondPolicy::Skip,
        },
        callback,
        DialBudget::default(),
    )?;

    print_handshake(&cfg, &controller);

    let start = Instant::now();
    let deadline = if cfg.seconds == 0 {
        None
    } else {
        Some(start + Duration::from_secs(cfg.seconds))
    };
    let mut stats = WindowStats::new();
    let mut window_start = start;
    let mut seq = 0u64;
    let mut total = 0u64;

    loop {
        let now = Instant::now();
        if let Some(deadline) = deadline {
            if now >= deadline {
                flush_dropped(&dropped, &mut stats);
                if stats.count > 0 || stats.dropped > 0 {
                    println!("{}", format_stats(&stats));
                }
                break;
            }
        }

        if !controller.is_connected() {
            println!("[monitor] link dropped after {total} reports");
            return Ok(());
        }

        let mut timeout = Duration::from_secs(1);
        if let Some(deadline) = deadline {
            timeout = timeout.min(deadline.saturating_duration_since(now));
        }
        if timeout.is_zero() {
            continue;
        }

        match rx.recv_timeout(timeout) {
            Ok(report) => {
                seq += 1;
                total += 1;
                let cal = controller.calibrated_input(&report);
                println!("{}", format_report(seq, &report, &cal));
                stats.observe(Instant::now());

                if window_start.elapsed() >= Duration::from_secs(1) {
                    flush_dropped(&dropped, &mut stats);
                    println!("{}", format_stats(&stats));
                    stats = WindowStats::new();
                    window_start = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                flush_dropped(&dropped, &mut stats);
                println!("{}", format_stats(&stats));
                stats = WindowStats::new();
                window_start = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => {
                println!("[monitor] link dropped after {total} reports");
                return Ok(());
            }
        }
    }

    println!("[monitor] done reports={total}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report(buttons: u32, battery_mv: u16) -> InputReport {
        InputReport {
            timestamp: 0,
            buttons,
            left_stick_raw: (2048, 2048),
            right_stick_raw: (2048, 2048),
            battery_mv,
            accel: (0, 0, 0),
            gyro: (0, 0, 0),
            left_trigger_raw: 0,
            right_trigger_raw: 0,
        }
    }

    fn zero_cal() -> CalibratedInput {
        CalibratedInput::default()
    }

    #[test]
    fn format_report_lists_pressed_buttons_in_order() {
        let buttons = button_mask::A
            | button_mask::X
            | button_mask::LEFT
            | button_mask::PLUS
            | button_mask::HOME
            | button_mask::ZR;
        let line = format_report(7, &sample_report(buttons, 3600), &zero_cal());
        assert!(
            line.starts_with("[#7] btn=A,X,LEFT,PLUS,HOME,ZR "),
            "line: {line}"
        );
    }

    #[test]
    fn format_report_uses_dash_when_nothing_is_pressed() {
        let line = format_report(1, &sample_report(0, 3600), &zero_cal());
        assert!(line.contains("btn=- "), "line: {line}");
        assert!(line.starts_with("[#1] btn=- raw=<2048,2048><2048,2048> "));
    }

    #[test]
    fn format_report_battery_agrees_with_status_helper() {
        for mv in [0u16, 2949, 2950, 3600, 4200, 65535] {
            let line = format_report(1, &sample_report(0, mv), &zero_cal());
            let expected = match battery_pct_from_mv(mv) {
                Some(pct) => format!("bat={mv}mV {pct}%"),
                None => format!("bat={mv}mV n/a%"),
            };
            assert!(
                line.contains(&expected),
                "mv={mv} expected {expected:?} in {line:?}"
            );
        }
    }

    #[test]
    fn format_stats_reports_rate_and_max_gap() {
        let stats = WindowStats {
            count: 42,
            dropped: 3,
            max_gap: Duration::from_millis(12),
            ..WindowStats::default()
        };
        assert_eq!(
            format_stats(&stats),
            "[stats] reports=42/s dropped=3 max_gap=12"
        );
    }

    #[test]
    fn window_stats_tracks_max_gap_and_drops() {
        let mut stats = WindowStats::new();
        let start = Instant::now();
        stats.observe(start);
        stats.observe(start + Duration::from_millis(10));
        stats.observe(start + Duration::from_millis(60));
        stats.record_dropped(2);
        stats.record_dropped(1);

        assert_eq!(stats.count, 3);
        assert_eq!(stats.dropped, 3);
        assert_eq!(stats.max_gap, Duration::from_millis(50));
    }

    fn snapshot(host: crate::protocol::advert::HostBinding, name: Option<&str>) -> AdvertSnapshot {
        // Full Nintendo block: prefix, vid 0x057E, pid 0x2067, flags, host field.
        let mut md = vec![0x00, 0x00, 0x00, 0x7E, 0x05, 0x67, 0x20, 0x00, 0x00, 0x00];
        md.extend_from_slice(&[0x1E, 0xD6, 0x41, 0x59, 0x17, 0x8C]);
        AdvertSnapshot {
            address: "BC:89:A6:45:83:CC".to_string(),
            address_type: Some("public".to_string()),
            name: name.map(|n| n.to_string()),
            connected: false,
            rssi: Some(-53),
            manufacturer_data: Some(md),
            advert: Some(crate::protocol::advert::AdvertInfo {
                address: "BC:89:A6:45:83:CC".to_string(),
                vid: 0x057E,
                pid: 0x2067,
                host,
            }),
        }
    }

    #[test]
    fn advert_line_for_nintendo_shows_host_in_display_order() {
        // Wire bytes `1E:D6:41:59:17:8C` reverse to `8C:17:59:41:D6:1E`.
        let line = advert_line_for(&snapshot(
            crate::protocol::advert::HostBinding::Host([0x1E, 0xD6, 0x41, 0x59, 0x17, 0x8C]),
            Some("Joy-Con 2 (Left)"),
        ));

        assert_eq!(
            line,
            "BC:89:A6:45:83:CC public pid=0x2067 Joy-Con 2 (Left) rssi=-53 \
             host=8C:17:59:41:D6:1E vid=0x057E \
             md=0000007e0567200000001ed64159178c"
        );
    }

    #[test]
    fn advert_line_for_reports_pairing_and_unknown_bindings() {
        let pairing = advert_line_for(&snapshot(
            crate::protocol::advert::HostBinding::Pairing,
            None,
        ));
        assert!(pairing.contains("host=pairing"), "{pairing}");
        assert!(pairing.contains("<unknown>"), "missing name is explicit");

        let unknown = advert_line_for(&snapshot(
            crate::protocol::advert::HostBinding::Unknown,
            Some("Pro Controller 2"),
        ));
        assert!(unknown.contains("host=unknown"), "{unknown}");
        assert!(unknown.contains("pid=0x2067"));
        assert!(unknown.contains("vid=0x057E"));
    }

    #[test]
    fn advert_line_for_non_nintendo_is_address_name_rssi_only() {
        let item = AdvertSnapshot {
            address: "11:22:33:44:55:66".to_string(),
            address_type: Some("random".to_string()),
            name: Some("some-headset".to_string()),
            connected: false,
            rssi: Some(-71),
            manufacturer_data: None,
            advert: None,
        };

        assert_eq!(
            advert_line_for(&item),
            "11:22:33:44:55:66 random some-headset rssi=-71"
        );
        // No Nintendo-only fields leak into a non-Nintendo line.
        assert!(!advert_line_for(&item).contains("md="));
        assert!(!advert_line_for(&item).contains("pid="));
    }

    #[test]
    fn pair_hold_header_keeps_the_led_note_and_ctrl_c_hint() {
        let header = pair_hold_header("BC:89:A6:45:83:CC", 2, 30);

        assert!(header.contains("BC:89:A6:45:83:CC"));
        assert!(header.contains("Player 2"));
        assert!(header.contains("30 s"));
        assert!(header.contains("LED"), "6 s verification note survives");
        assert!(header.contains("Ctrl-C"));
        assert!(
            header.contains("switch2d monitor"),
            "points at the same view"
        );
    }

    #[test]
    fn advert_prints_snapshot_with_md_but_no_rssi() {
        let mut item = snapshot(
            crate::protocol::advert::HostBinding::Host([0x1E, 0xD6, 0x41, 0x59, 0x17, 0x8C]),
            Some("Joy-Con 2 (Left)"),
        );
        // BlueZ dropped RSSI when its adv-monitor window closed.
        item.rssi = None;

        assert_eq!(
            advert_skip_reason(&item),
            None,
            "cached manufacturer data is enough to print"
        );
        let line = advert_line_for(&item);
        assert!(line.contains("rssi=?"), "{line}");
        assert!(
            line.contains("md=0000007e0567200000001ed64159178c"),
            "{line}"
        );
        assert!(line.contains("host=8C:17:59:41:D6:1E"), "{line}");
    }

    #[test]
    fn advert_skip_reason_explains_empty_entries() {
        let mut item = snapshot(crate::protocol::advert::HostBinding::Pairing, None);

        // Nothing at all: skipped, with a reason naming all three signals.
        item.manufacturer_data = None;
        item.rssi = None;
        item.connected = false;
        let reason = advert_skip_reason(&item).expect("no data => skipped");
        assert!(reason.contains("manufacturer data"), "{reason}");
        assert!(reason.contains("rssi"), "{reason}");
        assert!(reason.contains("connected"), "{reason}");

        // The -127 sentinel is not liveness.
        item.rssi = Some(-127);
        assert!(advert_skip_reason(&item).is_some());

        // A real RSSI is.
        item.rssi = Some(-100);
        assert_eq!(advert_skip_reason(&item), None);

        // So is being connected, even without RSSI or a block.
        item.rssi = None;
        item.connected = true;
        assert_eq!(advert_skip_reason(&item), None);
    }

    #[test]
    fn advert_dedupe_key_includes_address_type() {
        let mut public = snapshot(crate::protocol::advert::HostBinding::Pairing, None);
        public.address = "BC:89:A6:45:83:CC".to_string();
        public.address_type = Some("public".to_string());

        let mut random = public.clone();
        random.address_type = Some("random".to_string());

        assert_ne!(advert_dedupe_key(&public), advert_dedupe_key(&random));
        assert_eq!(advert_dedupe_key(&public), "BC:89:A6:45:83:CC|public");

        // The printed line changes too, so even line-based dedupe would reprint.
        assert_ne!(advert_line_for(&public), advert_line_for(&random));
    }
}
