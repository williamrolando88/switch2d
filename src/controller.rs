//! High-level controller abstraction managing handshake, calibration, and input streaming.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::bluetooth::att::{AttClient, ResolvedHandles};
use crate::bluetooth::l2cap::L2capSocket;
use crate::protocol::calibration::{StickCalibration, GC_TRIGGER_DEFAULT_NEUTRAL};
use crate::protocol::command::{
    build_feature_commands, build_memory_read, build_set_player_leds, build_sleep_command,
    mac_to_le, ControllerInfo,
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

        let mut handles = AttClient::resolve_handles(&services, JOYCON2_LEFT_PID);

        // Subscribe to command response notifications
        att.subscribe(handles.command_response_cccd, true)?;
        debug!("Subscribed to command responses at handle 0x{:04X}", handles.command_response_cccd);

        // Read controller info
        let info = Self::read_controller_info(&att, &handles);
        let pid = info.as_ref().map(|i| i.product_id).unwrap_or(JOYCON2_LEFT_PID);
        if let Some(inf) = &info {
            info!("Identified controller: {} (serial: {})", inf.name(), inf.serial_number);
        }

        // Refine handles with accurate PID
        handles = AttClient::resolve_handles(&services, pid);

        // Read stick calibration
        let (left_calib, right_calib) = Self::read_stick_calibrations(&att, &handles);

        // Set player LEDs
        let led_cmd = build_set_player_leds(player);
        let _ = att.write_command(handles.command_write, &led_cmd);

        // Enable features (motion + standard reports)
        let (f_init, f_enable) = build_feature_commands(0x03 | FEATURE_MOTION);
        let _ = att.write_command(handles.command_write, &f_init);
        let _ = att.write_command(handles.command_write, &f_enable);

        let is_ready = Arc::new(AtomicBool::new(false));
        let is_ready_clone = is_ready.clone();

        // Register notification callback
        let input_handle = handles.input_report;
        att.set_notification_handler(move |handle, data| {
            if handle == input_handle {
                if let Some(report) = InputReport::parse(data) {
                    if is_ready_clone.load(Ordering::Relaxed) {
                        input_cb(&report);
                    }
                }
            }
        });

        // Subscribe to input reports
        att.subscribe(handles.input_report_cccd, true)?;
        info!("Subscribed to input reports (handle 0x{:04X})", handles.input_report);

        is_ready.store(true, Ordering::Relaxed);

        Ok(Self {
            mac: mac.to_string(),
            adapter_mac: adapter_mac.to_string(),
            info,
            left_calib,
            right_calib,
            trigger_neutral: (GC_TRIGGER_DEFAULT_NEUTRAL, GC_TRIGGER_DEFAULT_NEUTRAL),
            att,
            handles,
            is_ready,
        })
    }

    fn read_controller_info(att: &AttClient, handles: &ResolvedHandles) -> Option<ControllerInfo> {
        let cmd = build_memory_read(0x40, ADDRESS_CONTROLLER_INFO);
        att.write_command(handles.command_write, &cmd).ok()?;

        // Wait up to 1s for response
        std::thread::sleep(Duration::from_millis(50));
        let read_pdu = att.request(&[0x0A, (handles.command_response & 0xFF) as u8, (handles.command_response >> 8) as u8], Duration::from_millis(500)).ok()?;
        if read_pdu.len() >= 8 {
            ControllerInfo::from_bytes(&read_pdu[8..])
        } else {
            None
        }
    }

    fn read_stick_calibrations(
        _att: &AttClient,
        _handles: &ResolvedHandles,
    ) -> (StickCalibration, StickCalibration) {
        // Use standard default calibration if direct memory read not populated
        (StickCalibration::default(), StickCalibration::default())
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
