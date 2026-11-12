//! switch2d: High-performance Linux userspace bridge and virtual gamepad daemon
//! for Nintendo Switch 2 controllers (Joy-Con 2, Pro Controller 2, NSO GameCube).

use clap::{Parser, Subcommand};
use std::process::Command;
use std::time::Duration;
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use switch2d::bluetooth::scan::{ScanControl, ShellScanControl};
use switch2d::config::{BondedController, DaemonConfig, DaemonMode};
use switch2d::controller::{BondPolicy, ConnectOptions, SwitchController};
use switch2d::daemon::Daemon;
use switch2d::protocol::constants::*;

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

    /// Enable verbose / debug logging
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the daemon in the foreground (default action)
    Run,

    /// Show daemon status, active mode, and bonded controllers
    Status,

    /// Set default operation mode: 'combined' (unified 1P controller) or 'split' (separate 2P controllers)
    Mode {
        /// Mode name: 'combined' or 'split'
        mode: String,
    },

    /// Pair, bond, and register a Switch 2 controller (sets player LED and saves to config)
    Pair {
        /// Bluetooth MAC address (e.g. BC:89:A6:45:83:CC). If omitted, scans automatically.
        mac: Option<String>,

        /// Optional friendly name (e.g. "Joy-Con 2 (Left)")
        #[arg(short, long)]
        name: Option<String>,

        /// Player assignment (1-8, defaults to next available slot)
        #[arg(short, long)]
        player: Option<u8>,
    },

    /// Register / add a Switch 2 controller MAC address directly
    Add {
        /// Bluetooth MAC address (e.g. BC:89:A6:45:83:CC)
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let log_level = if cli.verbose {
        Level::DEBUG
    } else {
        Level::INFO
    };

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_target(false)
        .without_time()
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("Setting default subscriber failed");

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

    // Auto-detect adapter if empty
    if config.adapter_mac.is_empty() {
        if let Some(detected) = auto_detect_adapter() {
            info!("Auto-detected Bluetooth adapter: {}", detected);
            config.adapter_mac = detected;
            let _ = config.save();
        }
    }

    match cli.command.unwrap_or(Commands::Status) {
        Commands::Run => {
            println!("===========================================================");
            println!("   Nintendo Switch 2 Linux Gamepad Daemon (Rust)");
            println!("   Mode: {:?} | Adapter: {}", config.mode, config.adapter_mac);
            println!("===========================================================");

            let daemon = Daemon::new(config);
            daemon.run().await?;
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
        Commands::Pair { mac, name, player } => {
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

            let next_player = player.unwrap_or_else(|| {
                (config.controllers.len() as u8 + 1).min(8)
            });

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

            let res = tokio::task::spawn_blocking(move || {
                SwitchController::connect_with(
                    &mac_for_task,
                    &adapter_mac,
                    ConnectOptions {
                        player: next_player,
                        // An explicit pair always re-bonds.
                        bond: BondPolicy::Force,
                    },
                    Box::new(|_| {}),
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
                    println!("The LED on your controller is now set to Player {}.", next_player);
                    println!("Saved to config. switch2d will now connect to it automatically!");
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

async fn scan_for_joycon() -> Option<String> {
    // Start discovery via bluetoothctl in background for 8 seconds
    let mut child = Command::new("bluetoothctl")
        .args(&["scan", "on"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    tokio::time::sleep(Duration::from_secs(8)).await;
    let _ = child.kill();
    let output = child.wait_with_output().ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if line.contains("Device") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for part in parts {
                if is_valid_mac(part) && (part.starts_with("BC:") || line.to_lowercase().contains("joy-con")) {
                    return Some(part.to_uppercase());
                }
            }
        }
    }

    None
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
