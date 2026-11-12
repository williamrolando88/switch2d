//! Switch 2 command framing, memory reads, and controller info decoding.

use crate::protocol::constants::{
    build_command, controller_name, COMMAND_FEATURE, COMMAND_HOST, COMMAND_LEDS,
    COMMAND_MEMORY, COMMAND_PAIR, HCI_STATE_SLEEP, LED_PATTERNS, PAIR_LTK1, PAIR_LTK2,
    SUBCOMMAND_FEATURE_ENABLE, SUBCOMMAND_FEATURE_INIT, SUBCOMMAND_HOST_HCI_STATE,
    SUBCOMMAND_LEDS_SET_PLAYER, SUBCOMMAND_MEMORY_READ, SUBCOMMAND_PAIR_FINISH,
    SUBCOMMAND_PAIR_LTK1, SUBCOMMAND_PAIR_LTK2, SUBCOMMAND_PAIR_SET_MAC,
};

/// Basic information read from the controller's factory memory (address 0x00013000).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerInfo {
    pub serial_number: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub colors: [[u8; 3]; 4],
}

impl ControllerInfo {
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 37 {
            return None;
        }

        let serial_raw = &data[2..16];
        let serial_number = String::from_utf8_lossy(serial_raw)
            .trim_matches('\0')
            .trim()
            .to_string();

        let vendor_id = u16::from_le_bytes([data[18], data[19]]);
        let product_id = u16::from_le_bytes([data[20], data[21]]);

        let mut colors = [[0u8; 3]; 4];
        for i in 0..4 {
            let offset = 25 + i * 3;
            colors[i].copy_from_slice(&data[offset..offset + 3]);
        }

        Some(Self {
            serial_number,
            vendor_id,
            product_id,
            colors,
        })
    }

    pub fn name(&self) -> &'static str {
        controller_name(self.product_id)
    }
}

/// Convert a MAC address string ("AA:BB:CC:DD:EE:FF") to a 6-byte big-endian array.
pub fn parse_mac(mac: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(part, 16).ok()?;
    }
    Some(out)
}

/// Convert a MAC string to 6-byte little-endian for BLE L2CAP / bonding.
pub fn mac_to_le(mac: &str) -> Option<[u8; 6]> {
    let mut bytes = parse_mac(mac)?;
    bytes.reverse();
    Some(bytes)
}

/// Format a 6-byte MAC array to a standard colon string.
pub fn format_mac(bytes: &[u8; 6]) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
    )
}

/// Build a memory read command payload.
pub fn build_memory_read(length: u8, address: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8);
    payload.push(length);
    payload.extend_from_slice(&[0x7E, 0x00, 0x00]);
    payload.extend_from_slice(&address.to_le_bytes());
    build_command(COMMAND_MEMORY, SUBCOMMAND_MEMORY_READ, &payload)
}

/// Build a player LED set command payload.
pub fn build_set_player_leds(player: u8) -> Vec<u8> {
    let idx = (player as usize).clamp(1, 8);
    let pattern = LED_PATTERNS[idx];
    let payload = [pattern, 0x00, 0x00, 0x00];
    build_command(COMMAND_LEDS, SUBCOMMAND_LEDS_SET_PLAYER, &payload)
}

/// Build feature initialization and enable payloads (0x03 | FEATURE_MOTION).
pub fn build_feature_commands(flags: u8) -> (Vec<u8>, Vec<u8>) {
    let payload = [flags, 0x00, 0x00, 0x00];
    let init = build_command(COMMAND_FEATURE, SUBCOMMAND_FEATURE_INIT, &payload);
    let enable = build_command(COMMAND_FEATURE, SUBCOMMAND_FEATURE_ENABLE, &payload);
    (init, enable)
}

/// Build controller sleep command.
pub fn build_sleep_command() -> Vec<u8> {
    let payload = [HCI_STATE_SLEEP, 0x00, 0x00, 0x00];
    build_command(COMMAND_HOST, SUBCOMMAND_HOST_HCI_STATE, &payload)
}

/// Build bonding sequence commands for storing the host MAC and LTK keys.
pub fn build_bonding_commands(adapter_mac_le: &[u8; 6]) -> Vec<Vec<u8>> {
    let mut mac_payload = vec![0x00, 0x02];
    mac_payload.extend_from_slice(adapter_mac_le);
    mac_payload.extend_from_slice(adapter_mac_le);

    vec![
        build_command(COMMAND_PAIR, SUBCOMMAND_PAIR_SET_MAC, &mac_payload),
        build_command(COMMAND_PAIR, SUBCOMMAND_PAIR_LTK1, &PAIR_LTK1),
        build_command(COMMAND_PAIR, SUBCOMMAND_PAIR_LTK2, &PAIR_LTK2),
        build_command(COMMAND_PAIR, SUBCOMMAND_PAIR_FINISH, &[0x00]),
    ]
}
