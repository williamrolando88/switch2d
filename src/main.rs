//! switch2d: High-performance Linux userspace bridge and virtual gamepad daemon
//! for Nintendo Switch 2 controllers (Joy-Con 2, Pro Controller 2, NSO GameCube).

use clap::{Parser, Subcommand};
use evdev::{AbsoluteAxisType, Key};
use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use switch2d::bluetooth::bluez::BluezAdvertSource;
use switch2d::bluetooth::scan::{ScanControl, ShellScanControl};
use switch2d::config::{BondedController, DaemonConfig, DaemonMode};
use switch2d::controller::{BondPolicy, ConnectOptions, DialBudget, SwitchController};
use switch2d::daemon::Daemon;
use switch2d::monitor::{
    advert_dedupe_key, advert_line_for, advert_skip_reason, format_report, pair_hold_header,
    run_monitor, MonitorConfig,
};
use switch2d::protocol::constants::*;
use switch2d::protocol::report::InputReport;
use switch2d::protocol::vibration::{amplitude, payload, stop_payload, Layout, Sample};
use switch2d::uinput::gamepad::{
    find_node, VirtualGamepad, KEYS, STICK_MAX, STICK_MIN, TRIGGER_MAX, TRIGGER_MIN,
};

#[derive(Parser)]
#[command(name = "switch2d")]
#[command(
    version = "0.1.0",
    about = "High-performance userspace Bluetooth bridge & virtual gamepad daemon for Nintendo Switch 2 controllers",
    long_about = "A standalone Rust daemon providing native Linux uinput virtual gamepads for Nintendo Switch 2 controllers with unified Joy-Con combining and sub-millisecond input latency."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Override Bluetooth adapter MAC address
    #[arg(short, long)]
    adapter: Option<String>,

    /// Override operation mode ('combined' or 'split')
    #[arg(short, long)]
    mode: Option<String>,

    /// Enable verbose / debug logging (same as RUST_LOG=debug; verbose wins)
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the daemon in the foreground (default action)
    Run,

    /// Show daemon status, active mode, and bonded controllers
    Status,

    /// Print live decoded input from a bonded controller (BLE smoke test)
    Monitor {
        /// Bluetooth MAC address (defaults to the first bonded controller)
        mac: Option<String>,

        /// How long to listen; 0 means until the link drops
        #[arg(long, default_value_t = 30)]
        seconds: u64,
    },

    /// Set default operation mode: 'combined' (unified 1P controller) or 'split' (separate 2P controllers)
    Mode {
        /// Mode name: 'combined' or 'split'
        mode: String,
    },

    /// Pair, bond, and register a Switch 2 controller (sets player LED and saves to config)
    Pair {
        /// Bluetooth MAC address (e.g. AA:BB:CC:DD:EE:01). If omitted, scans automatically.
        mac: Option<String>,

        /// Optional friendly name (e.g. "Joy-Con 2 (Left)")
        #[arg(short, long)]
        name: Option<String>,

        /// Player assignment (1-8, defaults to next available slot)
        #[arg(short, long)]
        player: Option<u8>,

        /// After bonding, keep the link open for this many seconds and stream
        /// live decoded input (like `monitor`). 0 = the default 6 s LED hold.
        #[arg(long, default_value_t = 0)]
        hold: u64,
    },

    /// Register / add a Switch 2 controller MAC address directly
    Add {
        /// Bluetooth MAC address (e.g. AA:BB:CC:DD:EE:01)
        mac: String,

        /// Optional friendly name (e.g. "Joy-Con 2 (Left)")
        #[arg(short, long)]
        name: Option<String>,

        /// Player assignment (1-8)
        #[arg(short, long, default_value = "1")]
        player: u8,
    },

    /// Remove a bonded controller by MAC address
    Remove {
        /// MAC address to remove
        mac: String,
    },

    /// Print live BLE advertisements (Nintendo ones in full by default)
    Advert {
        /// How long to watch for advertisements; 0 = until Ctrl-C
        #[arg(long, default_value_t = 15)]
        seconds: u64,

        /// Include non-Nintendo devices (address, name, RSSI only)
        #[arg(long)]
        all: bool,
    },

    /// Hardware smoke test: create a virtual pad, print what the kernel sees,
    /// and drive a visible button/stick sweep. No controller or Bluetooth needed.
    Selftest {
        /// How long to drive the visible sweep, in seconds (only with --drive)
        #[arg(long, default_value_t = 6)]
        seconds: u64,

        /// Actually move sticks and press buttons. Off by default: a node that
        /// Steam, a desktop controller daemon or an emulator is watching can
        /// otherwise steal focus or move the pointer while it is being read.
        #[arg(long)]
        drive: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let rust_log = std::env::var("RUST_LOG").ok();
    let log_level = log_level_for(cli.verbose, rust_log.as_deref());
    // Warn after the subscriber is installed, otherwise the line is dropped.
    let rust_log_has_unsupported_filters = rust_log
        .as_deref()
        .is_some_and(rust_log_has_filters);

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_target(false)
        .without_time()
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("Setting default subscriber failed");

    if rust_log_has_unsupported_filters {
        info!(
            "RUST_LOG contains target/field filters or offsets, which this build does not \
             implement; only plain trace/debug/info/warn/error levels are honoured (substring \
             match). Use -v for debug output."
        );
    }

    let mut config = DaemonConfig::load();

    // Command-line overrides
    if let Some(adapter) = cli.adapter {
        config.adapter_mac = adapter;
    }

    if let Some(mode_str) = cli.mode {
        match mode_str.to_lowercase().as_str() {
            "combined" => config.mode = DaemonMode::Combined,
            "split" => config.mode = DaemonMode::Split,
            other => {
                error!("Invalid mode '{}'. Must be 'combined' or 'split'", other);
                std::process::exit(1);
            }
        }
    }

    let command = cli.command.unwrap_or(Commands::Status);

    // The selftest exercises only the uinput half of the daemon: it must not
    // touch Bluetooth at all, so skip adapter autodetection (a
    // `bluetoothctl list` shell-out) for it.
    // Auto-detect adapter if empty
    if config.adapter_mac.is_empty() && !matches!(command, Commands::Selftest { .. }) {
        if let Some(detected) = auto_detect_adapter() {
            info!("Auto-detected Bluetooth adapter: {}", detected);
            config.adapter_mac = detected;
            let _ = config.save();
        }
    }

    match command {
        Commands::Run => {
            println!("===========================================================");
            println!("   Nintendo Switch 2 Linux Gamepad Daemon (Rust)");
            println!("   Mode: {:?} | Adapter: {}", config.mode, config.adapter_mac);
            println!("===========================================================");

            let daemon = Daemon::new(config);
            daemon.run().await?;
        }
        Commands::Monitor { mac, seconds } => {
            let from_config = match &mac {
                Some(wanted) => config
                    .controllers
                    .iter()
                    .find(|c| c.mac.eq_ignore_ascii_case(wanted))
                    .cloned(),
                None => config.controllers.first().cloned(),
            };

            let (target, player) = match (mac, from_config) {
                (Some(_), Some(entry)) => (entry.mac, entry.player),
                (Some(wanted), None) => (wanted.trim().to_uppercase(), 1),
                (None, Some(entry)) => (entry.mac, entry.player),
                (None, None) => {
                    println!("No bonded controllers yet - run `switch2d pair` first,");
                    println!("or pass a MAC: switch2d monitor AA:BB:CC:DD:EE:FF");
                    return Ok(());
                }
            };

            println!("===========================================================");
            println!("   Live monitor  {}  (Player {})", target, player);
            println!("   Press buttons; Ctrl-C to stop early.");
            println!("===========================================================");

            let cfg = MonitorConfig {
                mac: target,
                adapter_mac: config.adapter_mac.clone(),
                player,
                seconds,
            };
            tokio::task::spawn_blocking(move || run_monitor(cfg)).await??;
        }
        Commands::Status => {
            println!("===========================================================");
            println!("   switch2d Overview");
            println!("===========================================================");
            println!("Adapter MAC:  {}", if config.adapter_mac.is_empty() { "None (not configured)" } else { &config.adapter_mac });
            println!("Joy-Con Mode: {:?}", config.mode);
            println!("Bonded Controllers ({}):", config.controllers.len());
            if config.controllers.is_empty() {
                println!("  (No controllers paired yet)");
            } else {
                for (idx, c) in config.controllers.iter().enumerate() {
                    println!(
                        "  [{}] {} ({}) - Player {}",
                        idx + 1,
                        c.name,
                        c.mac,
                        c.player
                    );
                }
            }
            println!("Configuration: {}", DaemonConfig::config_path().display());
            println!("-----------------------------------------------------------");
            println!("Available Commands:");
            println!("  switch2d pair           Scan & bond Joy-Cons (LED locks solid)");
            println!("  switch2d mode <c|s>     Toggle Combined (1P) or Split (2P) mode");
            println!("  switch2d status         Show detailed status");
            println!("  switch2d run            Run daemon in foreground (debugging)");
            println!("===========================================================");
        }
        Commands::Mode { mode } => {
            match mode.to_lowercase().as_str() {
                "combined" => {
                    config.mode = DaemonMode::Combined;
                    config.save()?;
                    println!("Switched to Combined Mode: Left and Right Joy-Cons merge into a single Virtual Gamepad.");
                }
                "split" => {
                    config.mode = DaemonMode::Split;
                    config.save()?;
                    println!("Switched to Split Mode: Joy-Cons operate as individual standalone gamepads for multiplayer.");
                }
                other => {
                    error!("Invalid mode '{}'. Please choose 'combined' or 'split'.", other);
                    std::process::exit(1);
                }
            }
        }
        Commands::Pair {
            mac,
            name,
            player,
            hold,
        } => {
            let target_mac = match mac {
                Some(m) => m.trim().to_uppercase(),
                None => {
                    println!("===========================================================");
                    println!("   Scanning for Nintendo Switch 2 controllers (10s)...");
                    println!("   👉 Hold the circular SYNC button on your Joy-Con");
                    println!("      until the green LEDs cycle back and forth.");
                    println!("===========================================================");
                    match scan_for_joycon().await {
                        Some(found) => {
                            println!("Found Switch 2 controller: {}", found);
                            found
                        }
                        None => {
                            println!("No Switch 2 controllers found in pairing mode.");
                            println!("Tip: If you know the MAC address, run: switch2d pair <MAC>");
                            return Ok(());
                        }
                    }
                }
            };

            // Re-pairing an existing MAC must not invent a new player: reuse
            // its slot unless the user explicitly asked for a different one.
            // Only a MAC that is not in the config gets a fresh slot.
            let existing = config
                .controllers
                .iter()
                .find(|c| c.mac.eq_ignore_ascii_case(&target_mac));
            let existing_player = existing.map(|c| c.player);
            let next_free = (1..=8)
                .find(|p| !config.controllers.iter().any(|c| c.player == *p))
                .unwrap_or(8);
            let next_player = player_for_pair(existing_player, player, next_free);
            match (existing, player) {
                (Some(existing), Some(_)) => info!(
                    "assigning {} to Player {} (explicit; was Player {})",
                    target_mac, next_player, existing.player
                ),
                (Some(_), None) => info!(
                    "re-pairing {}, keeping Player {}",
                    target_mac, next_player
                ),
                (None, _) => info!("assigning {} to Player {}", target_mac, next_player),
            }

            // HANDOVER Bug 3: `scan_for_joycon()` kills the `bluetoothctl scan
            // on` helper, but that does not stop HCI discovery. An LE scan
            // still running makes the connect time out (EBUSY), so stop all
            // discovery first — best effort, never abort the pair attempt.
            let scan_control = ShellScanControl::from_env();
            if let Err(err) = scan_control.prepare_global() {
                warn!(
                    "Could not stop Bluetooth discovery before connecting: {}",
                    err
                );
            }

            println!("Connecting to {} (Player {})...", target_mac, next_player);

            let adapter_mac = config.adapter_mac.clone();
            let mac_for_task = target_mac.clone();

            // Reports are forwarded to this channel from the ATT reader thread
            // (try_send only, never printing) and consumed on the main thread
            // by the optional --hold loop, exactly like `monitor`.
            let (report_tx, report_rx) = mpsc::sync_channel::<InputReport>(64);

            let res = tokio::task::spawn_blocking(move || {
                let callback = Box::new(move |report: &InputReport| {
                    let _ = report_tx.try_send(report.clone());
                });
                // Same dial envelope as the daemon: BlueZ object removal plus
                // short rounds. Pairing a fresh pad rarely needs the retries,
                // but re-pairing a bonded one hits exactly the case they fix.
                SwitchController::connect_session(
                    &mac_for_task,
                    &adapter_mac,
                    ConnectOptions {
                        player: next_player,
                        // An explicit pair always re-bonds.
                        bond: BondPolicy::Force,
                    },
                    callback,
                    DialBudget::default(),
                )
            }).await?;

            match res {
                Ok(ctrl) => {
                    let final_name = name.unwrap_or_else(|| {
                        if let Some(info) = &ctrl.info {
                            info.name().to_string()
                        } else if target_mac.ends_with("CC") {
                            "Joy-Con 2 (Left)".to_string()
                        } else if target_mac.ends_with("74") {
                            "Joy-Con 2 (Right)".to_string()
                        } else {
                            "Switch 2 Controller".to_string()
                        }
                    });

                    let pid = ctrl.info.as_ref().map(|i| i.product_id).unwrap_or_else(|| {
                        if final_name.contains("Left") {
                            JOYCON2_LEFT_PID
                        } else if final_name.contains("Right") {
                            JOYCON2_RIGHT_PID
                        } else {
                            PRO_CONTROLLER2_PID
                        }
                    });

                    let serial = ctrl.info.as_ref().map(|i| i.serial_number.clone()).unwrap_or_default();
                    let bonded = ctrl.did_bond;

                    if !bonded {
                        println!("===========================================================");
                        println!("  ⚠️  Bonding was not confirmed by the controller.");
                        println!("  It may need to be paired again: switch2d pair {}", target_mac);
                        println!("===========================================================");
                    }

                    config.add_or_update_controller(BondedController {
                        mac: target_mac.clone(),
                        name: final_name.clone(),
                        product_id: pid,
                        serial,
                        player: next_player,
                        bonded,
                    });
                    config.save()?;

                    let ctrl_for_hold = ctrl;

                    println!("===========================================================");
                    if bonded {
                        println!("  ✅ Controller Successfully Paired & Bonded!");
                    } else {
                        println!("  ⚠️  Controller Paired — Bonding NOT confirmed!");
                    }
                    println!("  Device:  {}", final_name);
                    println!("  Address: {}", target_mac);
                    println!("  Player:  {}", next_player);
                    println!("===========================================================");
                    println!(
                        "The LED on your controller is now set to Player {}.",
                        next_player
                    );
                    if hold == 0 {
                        println!(
                            "Holding connection for 6 seconds so you can verify the Player {} LED...",
                            next_player
                        );
                        tokio::time::sleep(Duration::from_secs(6)).await;
                    } else {
                        println!("{}", pair_hold_header(&target_mac, next_player, hold));
                        run_pair_hold(ctrl_for_hold, report_rx, hold).await?;
                    }
                    println!("Saved to config. (Joy-Con LEDs will turn off now until switch2d runs or monitors).");
                }
                Err(e) => {
                    println!("❌ Failed to connect to {}: {}", target_mac, e);
                    println!("Make sure the Joy-Con's LEDs are still cycling and try again.");
                }
            }
        }
        Commands::Add { mac, name, player } => {
            let clean_mac = mac.trim().to_uppercase();
            let default_name = if clean_mac.ends_with("CC") {
                "Joy-Con 2 (Left)".to_string()
            } else if clean_mac.ends_with("74") {
                "Joy-Con 2 (Right)".to_string()
            } else {
                "Switch 2 Controller".to_string()
            };

            let final_name = name.unwrap_or(default_name);
            let pid = if final_name.contains("Left") {
                JOYCON2_LEFT_PID
            } else if final_name.contains("Right") {
                JOYCON2_RIGHT_PID
            } else {
                PRO_CONTROLLER2_PID
            };

            config.add_or_update_controller(BondedController {
                mac: clean_mac.clone(),
                name: final_name.clone(),
                product_id: pid,
                serial: String::new(),
                player,
                bonded: true,
            });
            config.save()?;
            println!("Registered controller {} ({}) as Player {}.", final_name, clean_mac, player);
        }
        Commands::Advert { seconds, all } => {
            run_advert(seconds, all).await?;
        }
        Commands::Selftest { seconds, drive } => {
            run_selftest(seconds, drive).await?;
        }
        Commands::Remove { mac } => {
            let clean_mac = mac.trim().to_uppercase();
            let initial_len = config.controllers.len();
            config.controllers.retain(|c| !c.mac.eq_ignore_ascii_case(&clean_mac));
            if config.controllers.len() < initial_len {
                config.save()?;
                println!("Removed controller {}.", clean_mac);
            } else {
                println!("Controller {} not found in bonded list.", clean_mac);
            }
        }
    }

    Ok(())
}

/// Map the CLI/env verbosity hints to a tracing level.
///
/// `tracing-subscriber`'s `env-filter` feature (regex, ~2.7 MB) is deliberately
/// **not** enabled, so `RUST_LOG` is parsed by substring, not as a filter
/// grammar:
///
/// - `-v/--verbose` always wins and selects `DEBUG`;
/// - otherwise a value containing `debug` or `trace` selects `DEBUG`;
/// - everything else (including `warn`, `error`, `off` and unsupported
///   directives) selects `INFO`, because this build has no quieter default.
fn log_level_for(verbose: bool, rust_log: Option<&str>) -> Level {
    if verbose {
        return Level::DEBUG;
    }

    match rust_log {
        Some(value) => {
            let value = value.to_ascii_lowercase();
            if value.contains("debug") || value.contains("trace") {
                Level::DEBUG
            } else {
                Level::INFO
            }
        }
        None => Level::INFO,
    }
}

/// Whether `RUST_LOG` asks for target/field filtering (`switch2d=debug`,
/// `info,hyper=off`) or span offsets, which this build does not implement.
///
/// Used to warn that the value is only being substring-matched, so nobody
/// believes an actual filter is in effect.
fn rust_log_has_filters(rust_log: &str) -> bool {
    rust_log.contains('=') || rust_log.contains(',') || rust_log.contains('[')
}

/// Resolve the player slot for `pair`.
///
/// An explicit `-p/--player` always wins (clamped to 1..=8). Otherwise an
/// already-bonded MAC keeps its existing player, and only a new MAC takes the
/// next free slot.
fn player_for_pair(existing_player: Option<u8>, explicit: Option<u8>, next_free: u8) -> u8 {
    explicit
        .or(existing_player)
        .unwrap_or(next_free)
        .clamp(1, 8)
}

// ---------------------------------------------------------------------------
// `pair --hold` and `switch2d advert`
// ---------------------------------------------------------------------------

/// Keep the freshly bonded link open and print decoded reports until the
/// deadline, Ctrl-C, or the link drops.
///
/// Runs on a blocking thread because `recv_timeout` and `calibrated_input` are
/// synchronous; the caller polls `ctrl_c` and sets the cancel flag.
async fn run_pair_hold(
    controller: SwitchController,
    reports: std::sync::mpsc::Receiver<InputReport>,
    seconds: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let deadline = Instant::now() + Duration::from_secs(seconds);

    let worker = tokio::task::spawn_blocking(move || {
        let mut seq = 0u64;
        while Instant::now() < deadline
            && !worker_cancel.load(Ordering::Relaxed)
            && controller.is_connected()
        {
            match reports.recv_timeout(Duration::from_millis(250)) {
                Ok(report) => {
                    seq += 1;
                    let calibrated = controller.calibrated_input(&report);
                    println!("{}", format_report(seq, &report, &calibrated));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        println!("[hold] link released after {seq} report(s).");
    });

    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            _ = interrupt.as_mut() => {
                cancel.store(true, Ordering::Relaxed);
                println!("[hold] Ctrl-C received, closing the link...");
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if worker.is_finished() {
                    break;
                }
            }
        }
    }
    worker.await?;
    Ok(())
}

/// `switch2d advert`: watch live advertisements for `seconds` (0 = until
/// Ctrl-C) and print one deduplicated line per device.
async fn run_advert(seconds: u64, all: bool) -> Result<(), Box<dyn std::error::Error>> {
    let control = ShellScanControl::from_env();
    let mut source = BluezAdvertSource::new();

    // Printed once so the host field can be compared by eye.
    let adapter = match source.adapter_mac(control.hci_index()) {
        Ok(mac) => mac,
        Err(err) => {
            warn!("Could not read adapter address: {err}");
            "<unknown>".to_string()
        }
    };
    println!("===========================================================");
    println!(
        "   Advertisements  (adapter {} on hci{})",
        adapter,
        control.hci_index()
    );
    println!("   host= is the bonded-host field the pad advertises.");
    println!("===========================================================");

    if let Err(err) = control.start_le_scan() {
        warn!("Could not start LE discovery: {err} (showing cached BlueZ objects only)");
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let worker = tokio::task::spawn_blocking(move || {
        let mut printed: HashMap<String, String> = HashMap::new();
        let mut printed_total = 0usize;
        let deadline = if seconds == 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_secs(seconds))
        };

        loop {
            if worker_cancel.load(Ordering::Relaxed) {
                break;
            }
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    break;
                }
            }

            match source.snapshot() {
                Ok(items) => {
                    for item in &items {
                        // `advert` uses data-presence liveness, not the hub's
                        // RSSI-only rule: BlueZ drops RSSI when its monitor
                        // window closes but keeps the cached block.
                        if let Some(reason) = advert_skip_reason(item) {
                            tracing::debug!("advert skip {}: {reason}", item.address);
                            continue;
                        }
                        if !all && item.advert.is_none() {
                            tracing::debug!(
                                "advert skip {}: not a Nintendo advert (use --all)",
                                item.address
                            );
                            continue;
                        }
                        let line = advert_line_for(item);
                        // Key includes the address type so a public->random flip
                        // is printed again. BlueZ caches properties, so an
                        // unchanged entry stays quiet.
                        let key = advert_dedupe_key(item);
                        if printed.get(&key) != Some(&line) {
                            println!("{line}");
                            printed.insert(key, line);
                            printed_total += 1;
                        }
                    }
                }
                Err(err) => warn!("BlueZ advert snapshot failed: {err}"),
            }

            std::thread::sleep(Duration::from_millis(300));
        }
        printed_total
    });

    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            _ = interrupt.as_mut() => {
                cancel.store(true, Ordering::Relaxed);
                println!("[advert] Ctrl-C received, stopping discovery...");
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if worker.is_finished() {
                    break;
                }
            }
        }
    }
    let printed_total = worker.await?;
    if printed_total == 0 {
        println!("[advert] no live advertisements seen.");
    }

    if let Err(err) = control.stop_le_scan(true) {
        warn!("Could not stop Bluetooth discovery: {err}");
    } else {
        println!("[advert] discovery stopped.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `switch2d selftest` — uinput hardware smoke test (no Bluetooth, no pad)
// ---------------------------------------------------------------------------

const SELFTEST_NAME: &str = "switch2d selftest";
const SELFTEST_PRODUCT_ID: u16 = PRO_CONTROLLER2_PID;
const SELFTEST_TAG: &str = "AA:BB:CC:DD:EE:FF";
const SELFTEST_STEP_INTERVAL: Duration = Duration::from_millis(80);

/// One visible step of the selftest sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Both sticks (and both stick axes) to `(x, y)`.
    Sticks { x: i32, y: i32, label: &'static str },
    /// Both triggers to a raw 0..=255 value.
    Triggers { value: u8, label: &'static str },
    /// One button pressed or released.
    Button { key: Key, pressed: bool },
    /// Every axis to 0 and every button released.
    Neutral,
}

/// The complete sweep: four stick corners, centre, trigger 0 -> 255 -> 0, then
/// every button pressed and released once. One pass is ~3 s at 80 ms per step.
fn sweep_plan() -> Vec<Step> {
    let mut plan = vec![
        Step::Sticks {
            x: STICK_MIN,
            y: STICK_MIN,
            label: "both sticks top-left",
        },
        Step::Sticks {
            x: STICK_MAX,
            y: STICK_MIN,
            label: "both sticks top-right",
        },
        Step::Sticks {
            x: STICK_MAX,
            y: STICK_MAX,
            label: "both sticks bottom-right",
        },
        Step::Sticks {
            x: STICK_MIN,
            y: STICK_MAX,
            label: "both sticks bottom-left",
        },
        Step::Sticks {
            x: 0,
            y: 0,
            label: "both sticks centred",
        },
        Step::Triggers {
            value: TRIGGER_MIN as u8,
            label: "both triggers released (0)",
        },
        Step::Triggers {
            value: TRIGGER_MAX as u8,
            label: "both triggers fully pressed (255)",
        },
        Step::Triggers {
            value: TRIGGER_MIN as u8,
            label: "both triggers released (0)",
        },
    ];

    for key in KEYS {
        plan.push(Step::Button { key, pressed: true });
        plan.push(Step::Button {
            key,
            pressed: false,
        });
    }

    plan.push(Step::Neutral);
    plan
}

fn describe(step: Step) -> String {
    match step {
        Step::Sticks { x, y, label } => format!("{label} (X/Y/RX/RY = {x}, {y})"),
        Step::Triggers { value, label } => format!("{label} (Z/RZ = {value})"),
        Step::Button { key, pressed } => {
            format!("{key:?} {}", if pressed { "pressed" } else { "released" })
        }
        Step::Neutral => "all axes neutral, every button released".to_string(),
    }
}

fn apply_step(gamepad: &VirtualGamepad, step: Step) {
    match step {
        Step::Sticks { x, y, .. } => {
            gamepad.set_abs(AbsoluteAxisType::ABS_X, x);
            gamepad.set_abs(AbsoluteAxisType::ABS_Y, y);
            gamepad.set_abs(AbsoluteAxisType::ABS_RX, x);
            gamepad.set_abs(AbsoluteAxisType::ABS_RY, y);
        }
        Step::Triggers { value, .. } => {
            gamepad.set_abs(AbsoluteAxisType::ABS_Z, i32::from(value));
            gamepad.set_abs(AbsoluteAxisType::ABS_RZ, i32::from(value));
        }
        Step::Button { key, pressed } => {
            gamepad.set_key(key, pressed);
        }
        Step::Neutral => {
            for axis in [
                AbsoluteAxisType::ABS_X,
                AbsoluteAxisType::ABS_Y,
                AbsoluteAxisType::ABS_RX,
                AbsoluteAxisType::ABS_RY,
                AbsoluteAxisType::ABS_Z,
                AbsoluteAxisType::ABS_RZ,
                AbsoluteAxisType::ABS_HAT0X,
                AbsoluteAxisType::ABS_HAT0Y,
            ] {
                gamepad.set_abs(axis, 0);
            }
            for key in KEYS {
                gamepad.set_key(key, false);
            }
        }
    }
    gamepad.sync();
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn run_selftest(seconds: u64, drive: bool) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== switch2d selftest (uinput only, no Bluetooth) ===");

    let gamepad = match VirtualGamepad::new(SELFTEST_NAME, SELFTEST_PRODUCT_ID, SELFTEST_TAG) {
        Ok(pad) => pad,
        Err(err) => {
            println!(
                "selftest: cannot create a virtual pad: {err}. /dev/uinput must exist and be \
                 writable (input group / udev rule). This is an environment problem, not a bug; \
                 skipping."
            );
            return Ok(());
        }
    };

    let Some((path, device)) = find_node(SELFTEST_NAME) else {
        println!(
            "selftest: the pad registered with the kernel but no /dev/input/event* node appeared \
             within 1 s; skipping (environment problem)."
        );
        return Ok(());
    };

    // Everything below is what the KERNEL reports, not what we asked for.
    println!("node:     {}", path.display());
    println!("name:     {}", device.name().unwrap_or("<none>"));
    println!("phys:     {}", device.physical_path().unwrap_or("<none>"));
    println!("uniq:     {}", device.unique_name().unwrap_or("<none>"));
    let id = device.input_id();
    println!(
        "input id: bus {:#06x} vendor {:#06x} product {:#06x} version {:#06x}",
        id.bus_type().0,
        id.vendor(),
        id.product(),
        id.version()
    );

    let abs_state = device.get_abs_state().ok();
    match (device.supported_absolute_axes(), abs_state) {
        (Some(axes), Some(state)) => {
            println!("abs axes ({}):", axes.iter().count());
            for axis in axes.iter() {
                let info = &state[axis.0 as usize];
                println!(
                    "  {axis:?} (0x{:02x}): min {} max {} fuzz {} flat {} value {}",
                    axis.0, info.minimum, info.maximum, info.fuzz, info.flat, info.value
                );
            }
        }
        _ => println!("abs axes: none"),
    }

    let keys = device.supported_keys();
    println!("keys:     {}", keys.map_or(0, |keys| keys.iter().count()));
    for key in [Key::BTN_SOUTH, Key::BTN_NORTH, Key::BTN_C, Key::BTN_TL2] {
        let present = keys.is_some_and(|keys| keys.contains(key));
        println!("  {key:?}: {}", if present { "present" } else { "MISSING" });
    }

    println!();
    println!("Watch it in another terminal:");
    println!("  evtest {}", path.display());
    println!();

    println!("HD rumble payloads the encoder emits (Pro Controller 2 = 33 B dual block):");
    let sample = Sample::with_amplitudes(amplitude(0.5, 0.0), 0);
    println!(
        "  target 0.5: {}",
        hex(&payload(Layout::DualBlock, 0x50, &sample))
    );
    println!(
        "  stop:       {}",
        hex(&stop_payload(Layout::DualBlock, 0x50))
    );
    println!("HD rumble payloads for a single-block pad (Joy-Con / GameCube = 17 B):");
    println!(
        "  target 0.5: {}",
        hex(&payload(Layout::SingleBlock, 0x50, &sample))
    );
    println!(
        "  stop:       {}",
        hex(&stop_payload(Layout::SingleBlock, 0x50))
    );
    println!();

    if !drive {
        // Passive mode: the node exists and is described, but nothing moves.
        // A uinput pad that suddenly sweeps buttons is a very good way to steal
        // focus from whatever has controller support enabled.
        println!(
            "Passive mode: the pad is up but NOT driven. Press Ctrl-C when done, or re-run with \
             --drive to sweep sticks and buttons."
        );
        println!("If anything on this desktop maps gamepads (Steam Input, antimicrox), close it first.");
        println!();
        let _ = tokio::signal::ctrl_c().await;
        drop(gamepad);
        println!("selftest: pad released.");
        return Ok(());
    }

    println!("This will sweep sticks and press every button.");
    println!("If anything on this desktop maps gamepads (Steam Input, antimicrox), close it first.");
    println!();

    let plan = sweep_plan();
    println!(
        "Driving {} steps per pass every {} ms for {} s...",
        plan.len(),
        SELFTEST_STEP_INTERVAL.as_millis(),
        seconds
    );

    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let worker = tokio::task::spawn_blocking(move || {
        let sweep_for = Duration::from_secs(seconds);
        let start = Instant::now();
        let mut passes = 0usize;

        // Always do at least one full pass, even with `--seconds 0`.
        while passes == 0 || start.elapsed() < sweep_for {
            for step in &plan {
                if passes > 0 && start.elapsed() >= sweep_for {
                    break;
                }
                if worker_cancel.load(Ordering::Relaxed) {
                    apply_step(&gamepad, Step::Neutral);
                    println!("selftest: interrupted; virtual pad released.");
                    return;
                }

                apply_step(&gamepad, *step);
                println!(
                    "[{:5.2}s] {}",
                    start.elapsed().as_secs_f32(),
                    describe(*step)
                );
                std::thread::sleep(SELFTEST_STEP_INTERVAL);
            }
            passes += 1;
        }

        apply_step(&gamepad, Step::Neutral);
        println!("Sweep finished after {passes} pass(es). Virtual pad released.");
    });

    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            _ = interrupt.as_mut() => {
                // The blocking worker notices within one 80 ms step, then drops
                // the pad; `RawUinput::drop` destroys the node.
                cancel.store(true, Ordering::Relaxed);
                println!("selftest: Ctrl-C received, releasing the virtual pad...");
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if worker.is_finished() {
                    break;
                }
            }
        }
    }
    worker.await?;

    // The pad is dropped here (RawUinput::drop destroys it): confirm the node
    // really disappears instead of asserting it.
    let mut gone = false;
    for _ in 0..20 {
        if !path.exists() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    println!(
        "Done. {} {}",
        path.display(),
        if gone {
            "is gone."
        } else {
            "is still present (it will vanish when the process exits)."
        }
    );
    Ok(())
}

async fn scan_for_joycon() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let output = Command::new("bluetoothctl")
            .args(&["--timeout", "8", "scan", "on"])
            .output()
            .ok()?;

        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if line.contains("Device") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                for part in parts {
                    let clean = part.trim();
                    if is_valid_mac(clean) && (clean.starts_with("BC:") || line.to_lowercase().contains("joy-con")) {
                        return Some(clean.to_uppercase());
                    }
                }
            }
        }

        None
    })
    .await
    .ok()?
}

fn is_valid_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6 && parts.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn auto_detect_adapter() -> Option<String> {
    let output = Command::new("bluetoothctl")
        .arg("list")
        .output()
        .ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if line.starts_with("Controller") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                return Some(parts[1].to_uppercase());
            }
        }
    }
    None
}

#[cfg(test)]
mod pair_player_tests {
    use super::*;

    #[test]
    fn reuses_existing_player_without_explicit() {
        assert_eq!(player_for_pair(Some(1), None, 3), 1);
        assert_eq!(player_for_pair(Some(7), None, 2), 7);
    }

    #[test]
    fn explicit_player_wins_over_existing() {
        assert_eq!(player_for_pair(Some(1), Some(4), 3), 4);
    }

    #[test]
    fn new_mac_gets_next_free() {
        assert_eq!(player_for_pair(None, None, 3), 3);
    }

    #[test]
    fn explicit_player_is_clamped() {
        assert_eq!(player_for_pair(None, Some(0), 3), 1);
        assert_eq!(player_for_pair(None, Some(9), 3), 8);
        assert_eq!(player_for_pair(Some(1), Some(0), 3), 1);
    }
}

#[cfg(test)]
mod selftest_tests {
    use super::*;

    #[test]
    fn selftest_sweep_sequence_covers_all_buttons_and_axis_extremes() {
        let plan = sweep_plan();

        // Every exported button is pressed exactly once and released exactly
        // once, and the plan contains no other button steps.
        for key in KEYS {
            let presses = plan
                .iter()
                .filter(|step| matches!(step, Step::Button { key: k, pressed: true } if *k == key))
                .count();
            let releases = plan
                .iter()
                .filter(|step| matches!(step, Step::Button { key: k, pressed: false } if *k == key))
                .count();
            assert_eq!(presses, 1, "{key:?} must be pressed exactly once");
            assert_eq!(releases, 1, "{key:?} must be released exactly once");
        }
        let button_steps = plan
            .iter()
            .filter(|step| matches!(step, Step::Button { .. }))
            .count();
        assert_eq!(button_steps, KEYS.len() * 2);

        // Both sticks hit every corner and the centre.
        for (x, y) in [
            (STICK_MIN, STICK_MIN),
            (STICK_MAX, STICK_MIN),
            (STICK_MAX, STICK_MAX),
            (STICK_MIN, STICK_MAX),
            (0, 0),
        ] {
            assert!(
                plan.iter().any(
                    |step| matches!(step, Step::Sticks { x: sx, y: sy, .. } if *sx == x && *sy == y)
                ),
                "missing stick position ({x}, {y})"
            );
        }

        let xs: Vec<i32> = plan
            .iter()
            .filter_map(|step| match step {
                Step::Sticks { x, .. } => Some(*x),
                _ => None,
            })
            .collect();
        let ys: Vec<i32> = plan
            .iter()
            .filter_map(|step| match step {
                Step::Sticks { y, .. } => Some(*y),
                _ => None,
            })
            .collect();
        assert!(xs.contains(&STICK_MIN) && xs.contains(&STICK_MAX));
        assert!(ys.contains(&STICK_MIN) && ys.contains(&STICK_MAX));

        // Triggers sweep 0 -> 255 -> 0.
        let triggers: Vec<u8> = plan
            .iter()
            .filter_map(|step| match step {
                Step::Triggers { value, .. } => Some(*value),
                _ => None,
            })
            .collect();
        assert!(triggers.contains(&(TRIGGER_MIN as u8)));
        assert!(triggers.contains(&(TRIGGER_MAX as u8)));
        assert_eq!(
            triggers,
            vec![0, 255, 0],
            "trigger sweep must be 0 -> 255 -> 0"
        );

        // The sweep ends neutral so nothing is left held.
        assert_eq!(plan.last(), Some(&Step::Neutral));
    }
}

#[cfg(test)]
mod log_level_tests {
    use super::*;

    #[test]
    fn verbose_wins_over_rust_log() {
        assert_eq!(log_level_for(true, None), Level::DEBUG);
        assert_eq!(log_level_for(true, Some("warn")), Level::DEBUG);
        assert_eq!(log_level_for(true, Some("off")), Level::DEBUG);
    }

    #[test]
    fn rust_log_debug_and_trace_select_debug() {
        assert_eq!(log_level_for(false, Some("debug")), Level::DEBUG);
        assert_eq!(log_level_for(false, Some("DEBUG")), Level::DEBUG);
        assert_eq!(log_level_for(false, Some("trace")), Level::DEBUG);
        // Documented limitation: substring match, so a target filter also works
        // (it is warned about separately).
        assert_eq!(log_level_for(false, Some("switch2d=debug")), Level::DEBUG);
    }

    #[test]
    fn other_and_missing_values_stay_info() {
        assert_eq!(log_level_for(false, None), Level::INFO);
        assert_eq!(log_level_for(false, Some("")), Level::INFO);
        assert_eq!(log_level_for(false, Some("info")), Level::INFO);
        // This build has no quieter default, so warn/error do not silence it.
        assert_eq!(log_level_for(false, Some("warn")), Level::INFO);
        assert_eq!(log_level_for(false, Some("error")), Level::INFO);
        assert_eq!(log_level_for(false, Some("hyper=off")), Level::INFO);
    }

    #[test]
    fn unsupported_directives_are_detected_for_the_warning() {
        assert!(rust_log_has_filters("switch2d=debug"));
        assert!(rust_log_has_filters("info,hyper=off"));
        assert!(rust_log_has_filters("debug[span{field=value}]"));
        assert!(!rust_log_has_filters("debug"));
        assert!(!rust_log_has_filters("info"));
        assert!(!rust_log_has_filters("switch2d::att"));
    }
}
