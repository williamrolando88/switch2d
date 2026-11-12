//! Switch 2 BLE input report parsing (63-byte GATT notifications).

use crate::protocol::calibration::get_stick_xy;
use crate::protocol::constants::button_mask;

/// Parsed input report from the Switch 2 controller GATT characteristic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputReport {
    pub timestamp: u32,
    pub buttons: u32,
    pub left_stick_raw: (u16, u16),
    pub right_stick_raw: (u16, u16),
    pub battery_mv: u16,
    pub accel: (i16, i16, i16),
    pub gyro: (i16, i16, i16),
    pub left_trigger_raw: u8,
    pub right_trigger_raw: u8,
}

impl InputReport {
    /// Parse raw GATT notification bytes into an InputReport.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 16 {
            return None;
        }

        let timestamp = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let buttons = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let left_stick_raw = get_stick_xy(&data[10..13]);
        let right_stick_raw = get_stick_xy(&data[13..16]);

        let battery_mv = if data.len() >= 0x21 {
            u16::from_le_bytes([data[0x1F], data[0x20]])
        } else {
            0
        };

        let accel = if data.len() >= 0x36 {
            (
                i16::from_le_bytes([data[0x30], data[0x31]]),
                i16::from_le_bytes([data[0x32], data[0x33]]),
                i16::from_le_bytes([data[0x34], data[0x35]]),
            )
        } else {
            (0, 0, 0)
        };

        let gyro = if data.len() >= 0x3C {
            (
                i16::from_le_bytes([data[0x36], data[0x37]]),
                i16::from_le_bytes([data[0x38], data[0x39]]),
                i16::from_le_bytes([data[0x3A], data[0x3B]]),
            )
        } else {
            (0, 0, 0)
        };

        let left_trigger_raw = if data.len() > 0x3C { data[0x3C] } else { 0 };
        let right_trigger_raw = if data.len() > 0x3D { data[0x3D] } else { 0 };

        Some(Self {
            timestamp,
            buttons,
            left_stick_raw,
            right_stick_raw,
            battery_mv,
            accel,
            gyro,
            left_trigger_raw,
            right_trigger_raw,
        })
    }

    /// Check if a specific button bitmask is pressed.
    #[inline]
    pub fn is_pressed(&self, mask: u32) -> bool {
        (self.buttons & mask) != 0
    }

    /// Approximate battery percentage from mV (assumes Li-ion: ~3500mV empty, 4150mV full).
    pub fn battery_percentage(&self) -> u8 {
        if self.battery_mv < 3500 {
            0
        } else if self.battery_mv >= 4150 {
            100
        } else {
            let pct = ((self.battery_mv - 3500) as u32 * 100) / (4150 - 3500);
            pct.min(100) as u8
        }
    }

    /// D-pad Hat X axis (-1 = left, 1 = right, 0 = neutral)
    pub fn hat_x(&self) -> i32 {
        let left = self.is_pressed(button_mask::LEFT);
        let right = self.is_pressed(button_mask::RIGHT);
        match (left, right) {
            (true, false) => -1,
            (false, true) => 1,
            _ => 0,
        }
    }

    /// D-pad Hat Y axis (-1 = up, 1 = down, 0 = neutral)
    pub fn hat_y(&self) -> i32 {
        let up = self.is_pressed(button_mask::UP);
        let down = self.is_pressed(button_mask::DOWN);
        match (up, down) {
            (true, false) => -1,
            (false, true) => 1,
            _ => 0,
        }
    }
}
