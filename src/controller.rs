//! High-level controller abstraction managing handshake, calibration, and input streaming.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::bluetooth::att::{AttClient, ResolvedHandles};
use crate::bluetooth::l2cap::L2capSocket;
use crate::protocol::calibration::{StickCalibration, GC_TRIGGER_DEFAULT_NEUTRAL};
use crate::protocol::command::{
    build_bonding_commands, build_sleep_command, mac_to_le, ControllerInfo,
};
use crate::protocol::constants::*;
use crate::protocol::report::InputReport;

pub type ControllerInputCallback = Box<dyn Fn(&InputReport) + Send + Sync + 'static>;

pub struct SwitchController {
    pub mac: String,
    pub adapter_mac: String,
    pub info: Option<ControllerInfo>,
    pub left_calib: StickCalibration,
    pub right_calib: StickCalibration,
    pub trigger_neutral: (u8, u8),
    att: AttClient,
    handles: ResolvedHandles,
    cmd_resp_state: Arc<(Mutex<Option<Vec<u8>>>, Condvar)>,
    pub is_ready: Arc<AtomicBool>,
}

impl SwitchController {
    /// Connect and initialize a Switch 2 controller via raw BLE L2CAP.
    pub fn connect(
        mac: &str,
        adapter_mac: &str,
        player: u8,
        input_cb: ControllerInputCallback,
    ) -> std::io::Result<Self> {
        let dest_le = mac_to_le(mac).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid controller MAC")
        })?;
        let adapter_le = mac_to_le(adapter_mac).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid adapter MAC")
        })?;

        info!("Connecting to Switch 2 controller {}...", mac);
        let socket = L2capSocket::create()?;
        socket.bind(adapter_le)?;

        // Attempt connect (tries LE_PUBLIC then LE_RANDOM)
        let addr_type = socket.connect_le(dest_le, Duration::from_secs(8))?;
        info!("L2CAP link established with {} (type: {})", mac, addr_type);

        let att = AttClient::new(socket);

        // MTU exchange
        let mtu = att.exchange_mtu(247).unwrap_or(23);
        debug!("Negotiated ATT MTU: {}", mtu);

        // Discover GATT layout
        let services = match att.discover_all() {
            Ok(s) => s,
            Err(e) => {
                warn!("GATT discovery failed ({}); falling back to default handles", e);
                Vec::new()
            }
        };

        let handles = AttClient::resolve_handles(&services, JOYCON2_LEFT_PID);

        let cmd_resp_state = Arc::new((Mutex::new(None), Condvar::new()));
        let cmd_resp_for_ntf = cmd_resp_state.clone();
        let cmd_resp_handle = handles.command_response;
        let input_handle = handles.input_report;

        let is_ready = Arc::new(AtomicBool::new(false));
        let is_ready_clone = is_ready.clone();

        // Register master notification callback for both input reports and command responses
        att.set_notification_handler(move |handle, data| {
            if handle == input_handle {
                if let Some(report) = InputReport::parse(data) {
                    if is_ready_clone.load(Ordering::Relaxed) {
                        input_cb(&report);
                    }
                }
            } else if handle == cmd_resp_handle {
                let (lock, cvar) = &*cmd_resp_for_ntf;
                let mut resp = lock.lock().unwrap();
                *resp = Some(data.to_vec());
                cvar.notify_one();
            }
        });

        // Subscribe to command responses first so we can receive acknowledgements
        att.subscribe(handles.command_response_cccd, true)?;
        info!("Subscribed to command responses (handle 0x{:04X})", handles.command_response_cccd);

        let mut controller = Self {
            mac: mac.to_string(),
            adapter_mac: adapter_mac.to_string(),
            info: None,
            left_calib: StickCalibration::default(),
            right_calib: StickCalibration::default(),
            trigger_neutral: (GC_TRIGGER_DEFAULT_NEUTRAL, GC_TRIGGER_DEFAULT_NEUTRAL),
            att,
            handles,
            cmd_resp_state,
            is_ready,
        };

        // Try reading controller info to identify Joy-Con L/R or Pro pad
        if let Ok(info) = controller.read_controller_info() {
            info!("Identified controller: {} (serial: {})", info.name(), info.serial_number);
            let pid = info.product_id;
            controller.handles = AttClient::resolve_handles(&services, pid);
            controller.info = Some(info);
        }

        // Always complete bonding so the controller remembers host adapter and stops pairing mode
        if let Err(e) = controller.bond(&adapter_le) {
            warn!("Bonding sequence notice: {}", e);
        }

        // Set player LED pattern (stops flashing marquee LEDs!)
        if let Err(e) = controller.set_player_leds(player) {
            warn!("Player LED command: {}", e);
        } else {
            info!("Player LED set to Player {}", player);
        }

        // Enable standard input reporting + motion
        let _ = controller.enable_features(0x03 | FEATURE_MOTION);

        // Subscribe to input reports
        controller.att.subscribe(controller.handles.input_report_cccd, true)?;
        info!("Input streaming active (handle 0x{:04X})", controller.handles.input_report);

        controller.is_ready.store(true, Ordering::Relaxed);

        Ok(controller)
    }

    /// Send a framed command and await the ACK notification.
    pub fn write_command(
        &self,
        command_id: u8,
        subcommand_id: u8,
        data: &[u8],
        timeout: Duration,
    ) -> std::io::Result<Vec<u8>> {
        let buf = crate::protocol::constants::build_command(command_id, subcommand_id, data);

        let (lock, cvar) = &*self.cmd_resp_state;
        {
            let mut resp = lock.lock().unwrap();
            *resp = None;
        }

        self.att.write_command(self.handles.command_write, &buf)?;

        let mut resp_guard = lock.lock().unwrap();
        while resp_guard.is_none() {
            let (new_guard, wait_res) = cvar.wait_timeout(resp_guard, timeout).unwrap();
            resp_guard = new_guard;
            if wait_res.timed_out() && resp_guard.is_none() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("Command 0x{:02X}/0x{:02X} timed out", command_id, subcommand_id),
                ));
            }
        }

        let resp = resp_guard.take().unwrap();
        if resp.len() < 8 || resp[0] != command_id || resp[1] != 0x01 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Unexpected command response: {:02X?}", resp),
            ));
        }

        Ok(resp[8..].to_vec())
    }

    /// Read memory from controller address.
    pub fn read_memory(&self, length: u8, address: u32) -> std::io::Result<Vec<u8>> {
        let mut payload = Vec::with_capacity(8);
        payload.push(length);
        payload.extend_from_slice(&[0x7E, 0x00, 0x00]);
        payload.extend_from_slice(&address.to_le_bytes());

        let data = self.write_command(
            COMMAND_MEMORY,
            SUBCOMMAND_MEMORY_READ,
            &payload,
            Duration::from_millis(1500),
        )?;

        if data.len() < 8 || data[0] != length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Invalid memory read response",
            ));
        }

        Ok(data[8..].to_vec())
    }

    pub fn read_controller_info(&self) -> std::io::Result<ControllerInfo> {
        let mem = self.read_memory(0x40, ADDRESS_CONTROLLER_INFO)?;
        ControllerInfo::from_bytes(&mem).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "Failed to parse controller info")
        })
    }

    pub fn set_player_leds(&self, player: u8) -> std::io::Result<()> {
        let idx = (player as usize).clamp(1, 8);
        let pattern = LED_PATTERNS[idx];
        let payload = [pattern, 0x00, 0x00, 0x00];
        let _ = self.write_command(
            COMMAND_LEDS,
            SUBCOMMAND_LEDS_SET_PLAYER,
            &payload,
            Duration::from_millis(1500),
        )?;
        Ok(())
    }

    pub fn enable_features(&self, flags: u8) -> std::io::Result<()> {
        let payload = [flags, 0x00, 0x00, 0x00];
        let _ = self.write_command(
            COMMAND_FEATURE,
            SUBCOMMAND_FEATURE_INIT,
            &payload,
            Duration::from_millis(1000),
        );
        let _ = self.write_command(
            COMMAND_FEATURE,
            SUBCOMMAND_FEATURE_ENABLE,
            &payload,
            Duration::from_millis(1000),
        );
        Ok(())
    }

    pub fn bond(&self, adapter_le: &[u8; 6]) -> std::io::Result<()> {
        let commands = build_bonding_commands(adapter_le);
        for cmd in commands {
            let _ = self.att.write_command(self.handles.command_write, &cmd);
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.att.is_connected()
    }

    pub fn sleep(&self) {
        let sleep_cmd = build_sleep_command();
        let _ = self.att.write_command(self.handles.command_write, &sleep_cmd);
        self.att.close();
    }
}
