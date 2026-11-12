//! switch2-daemon: High-performance Linux userspace bridge and virtual gamepad daemon
//! for Nintendo Switch 2 controllers (Joy-Con 2, Pro Controller 2, NSO GameCube).

use clap::{Parser, Subcommand};
use std::process::Command;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

use switch2_daemon::config::{BondedController, DaemonConfig, DaemonMode};
use switch2_daemon::daemon::Daemon;
use switch2_daemon::protocol::constants::*;

#[derive(Parser)]
#[command(name = "switch2-daemon")]
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

    /// Register / bond a Switch 2 controller MAC address
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

    match cli.command.unwrap_or(Commands::Run) {
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
            println!("   switch2-daemon Status");
            println!("===========================================================");
            println!("Adapter MAC:  {}", if config.adapter_mac.is_empty() { "None (not configured)" } else { &config.adapter_mac });
            println!("Joy-Con Mode: {:?}", config.mode);
            println!("Bonded Controllers ({}):", config.controllers.len());
            for (idx, c) in config.controllers.iter().enumerate() {
                println!(
                    "  [{}] {} ({}) - Player {}",
                    idx + 1,
                    c.name,
                    c.mac,
                    c.player
                );
            }
            println!("Configuration file: {}", DaemonConfig::config_path().display());
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
