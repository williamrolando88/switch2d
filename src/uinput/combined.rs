//! Combined and split mapper for Switch 2 Joy-Cons and standard controllers.

use evdev::{AbsoluteAxisType, Key};
use std::sync::Arc;

use super::gamepad::VirtualGamepad;
use crate::protocol::calibration::StickCalibration;
use crate::protocol::constants::button_mask;
use crate::protocol::report::InputReport;

/// Coordinates two individual Joy-Cons (Left and Right) into a single virtual gamepad.
pub struct CombinedJoyConManager {
    gamepad: Arc<VirtualGamepad>,
}

impl CombinedJoyConManager {
    pub fn new(gamepad: Arc<VirtualGamepad>) -> Self {
        Self { gamepad }
    }

    /// Process input report from Joy-Con 2 Left.
    pub fn update_left(&self, report: &InputReport, calib: &StickCalibration) {
        let (lx, ly) = calib.apply_scaled_i16(report.left_stick_raw, 120);

        let mut changed = false;

        // Stick X and Y (Linux evdev standard: pushing stick UP produces negative Y)
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_X, lx);
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_Y, -ly);

        // D-Pad Hat
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_HAT0X, report.hat_x());
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_HAT0Y, report.hat_y());

        // Buttons
        changed |= self.gamepad.set_key(Key::BTN_SELECT, report.is_pressed(button_mask::MINUS));
        changed |= self.gamepad.set_key(Key::BTN_Z, report.is_pressed(button_mask::CAPTURE));
        changed |= self.gamepad.set_key(Key::BTN_TL, report.is_pressed(button_mask::L));
        changed |= self.gamepad.set_key(Key::BTN_TL2, report.is_pressed(button_mask::ZL));
        changed |= self.gamepad.set_key(Key::BTN_THUMBL, report.is_pressed(button_mask::L_STICK_BTN));

        // Digital trigger axis emulation for ZL
        let zl_val = if report.is_pressed(button_mask::ZL) { 255 } else { 0 };
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_Z, zl_val);

        if changed {
            self.gamepad.sync();
        }
    }

    /// Process input report from Joy-Con 2 Right.
    pub fn update_right(&self, report: &InputReport, calib: &StickCalibration) {
        let (rx, ry) = calib.apply_scaled_i16(report.right_stick_raw, 120);

        let mut changed = false;

        // Stick X and Y
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_RX, rx);
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_RY, -ry);

        // Face buttons (Nintendo layout to evdev standard diamond)
        changed |= self.gamepad.set_key(Key::BTN_SOUTH, report.is_pressed(button_mask::A));
        changed |= self.gamepad.set_key(Key::BTN_EAST, report.is_pressed(button_mask::B));
        changed |= self.gamepad.set_key(Key::BTN_WEST, report.is_pressed(button_mask::X));
        changed |= self.gamepad.set_key(Key::BTN_NORTH, report.is_pressed(button_mask::Y));

        // System & Menu
        changed |= self.gamepad.set_key(Key::BTN_START, report.is_pressed(button_mask::PLUS));
        changed |= self.gamepad.set_key(Key::BTN_MODE, report.is_pressed(button_mask::HOME));
        changed |= self.gamepad.set_key(Key::BTN_C, report.is_pressed(button_mask::C_BTN));

        // Bumpers & Triggers
        changed |= self.gamepad.set_key(Key::BTN_TR, report.is_pressed(button_mask::R));
        changed |= self.gamepad.set_key(Key::BTN_TR2, report.is_pressed(button_mask::ZR));
        changed |= self.gamepad.set_key(Key::BTN_THUMBR, report.is_pressed(button_mask::R_STICK_BTN));

        // Digital trigger axis emulation for ZR
        let zr_val = if report.is_pressed(button_mask::ZR) { 255 } else { 0 };
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_RZ, zr_val);

        if changed {
            self.gamepad.sync();
        }
    }

    /// Process a full controller report (Pro Controller or standalone dual-stick gamepad).
    pub fn update_full(
        &self,
        report: &InputReport,
        l_calib: &StickCalibration,
        r_calib: &StickCalibration,
    ) {
        let (lx, ly) = l_calib.apply_scaled_i16(report.left_stick_raw, 120);
        let (rx, ry) = r_calib.apply_scaled_i16(report.right_stick_raw, 120);

        let mut changed = false;

        // Sticks
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_X, lx);
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_Y, -ly);
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_RX, rx);
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_RY, -ry);

        // D-Pad
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_HAT0X, report.hat_x());
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_HAT0Y, report.hat_y());

        // Buttons
        changed |= self.gamepad.set_key(Key::BTN_SOUTH, report.is_pressed(button_mask::A));
        changed |= self.gamepad.set_key(Key::BTN_EAST, report.is_pressed(button_mask::B));
        changed |= self.gamepad.set_key(Key::BTN_WEST, report.is_pressed(button_mask::X));
        changed |= self.gamepad.set_key(Key::BTN_NORTH, report.is_pressed(button_mask::Y));

        changed |= self.gamepad.set_key(Key::BTN_TL, report.is_pressed(button_mask::L));
        changed |= self.gamepad.set_key(Key::BTN_TR, report.is_pressed(button_mask::R));
        changed |= self.gamepad.set_key(Key::BTN_TL2, report.is_pressed(button_mask::ZL));
        changed |= self.gamepad.set_key(Key::BTN_TR2, report.is_pressed(button_mask::ZR));

        changed |= self.gamepad.set_key(Key::BTN_SELECT, report.is_pressed(button_mask::MINUS));
        changed |= self.gamepad.set_key(Key::BTN_START, report.is_pressed(button_mask::PLUS));
        changed |= self.gamepad.set_key(Key::BTN_MODE, report.is_pressed(button_mask::HOME));
        changed |= self.gamepad.set_key(Key::BTN_Z, report.is_pressed(button_mask::CAPTURE));

        changed |= self.gamepad.set_key(Key::BTN_THUMBL, report.is_pressed(button_mask::L_STICK_BTN));
        changed |= self.gamepad.set_key(Key::BTN_THUMBR, report.is_pressed(button_mask::R_STICK_BTN));

        let zl_val = if report.is_pressed(button_mask::ZL) { 255 } else { 0 };
        let zr_val = if report.is_pressed(button_mask::ZR) { 255 } else { 0 };
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_Z, zl_val);
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_RZ, zr_val);

        if changed {
            self.gamepad.sync();
        }
    }
}
