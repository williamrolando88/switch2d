//! Combined and split mapper for Switch 2 Joy-Cons and standard controllers.

use evdev::{AbsoluteAxisType, Key};
use std::sync::{Arc, Mutex, MutexGuard};

use super::gamepad::{PadSink, VirtualGamepad, STICK_MAX};
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

    /// Process input report from Joy-Con 2 Left held horizontally (Split Mode).
    /// Rotates stick 90° CCW, maps SL/SR to bumpers, and D-Pad to face buttons ABXY.
    pub fn update_horizontal_left(&self, report: &InputReport, calib: &StickCalibration) {
        let (lx, ly) = calib.apply_scaled_i16(report.left_stick_raw, 120);

        let mut changed = false;

        // Rotated stick: pushing physically UP (towards rail) is +X, pushing RIGHT is -Y
        let h_x = -ly;
        let h_y = -lx;
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_X, h_x);
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_Y, h_y);

        // Face buttons (D-pad cluster rotated 90° CCW):
        // Bottom (6 o'clock) = Left Arrow  -> BTN_SOUTH (B on Nintendo)
        // Right  (3 o'clock) = Down Arrow  -> BTN_EAST  (A on Nintendo)
        // Left   (9 o'clock) = Up Arrow    -> BTN_WEST  (Y on Nintendo)
        // Top   (12 o'clock) = Right Arrow -> BTN_NORTH (X on Nintendo)
        changed |= self.gamepad.set_key(Key::BTN_SOUTH, report.is_pressed(button_mask::LEFT));
        changed |= self.gamepad.set_key(Key::BTN_EAST, report.is_pressed(button_mask::DOWN));
        changed |= self.gamepad.set_key(Key::BTN_WEST, report.is_pressed(button_mask::UP));
        changed |= self.gamepad.set_key(Key::BTN_NORTH, report.is_pressed(button_mask::RIGHT));

        // Shoulder bumpers on the rail
        changed |= self.gamepad.set_key(Key::BTN_TL, report.is_pressed(button_mask::SL_L));
        changed |= self.gamepad.set_key(Key::BTN_TR, report.is_pressed(button_mask::SR_L));

        // Outer triggers (for extra flexibility)
        changed |= self.gamepad.set_key(Key::BTN_TL2, report.is_pressed(button_mask::L) || report.is_pressed(button_mask::ZL));

        // System buttons
        changed |= self.gamepad.set_key(Key::BTN_SELECT, report.is_pressed(button_mask::MINUS));
        changed |= self.gamepad.set_key(Key::BTN_START, report.is_pressed(button_mask::CAPTURE));
        changed |= self.gamepad.set_key(Key::BTN_THUMBL, report.is_pressed(button_mask::L_STICK_BTN));

        if changed {
            self.gamepad.sync();
        }
    }

    /// Process input report from Joy-Con 2 Right held horizontally (Split Mode).
    /// Rotates stick 90° CW, maps SL/SR to bumpers, and ABXY diamond to match horizontal grip.
    pub fn update_horizontal_right(&self, report: &InputReport, calib: &StickCalibration) {
        let (rx, ry) = calib.apply_scaled_i16(report.right_stick_raw, 120);

        let mut changed = false;

        // Rotated stick: pushing physically UP (towards rail) is -X, pushing RIGHT is +Y
        let h_x = ry;
        let h_y = rx;
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_X, h_x);
        changed |= self.gamepad.set_abs(AbsoluteAxisType::ABS_Y, h_y);

        // Face buttons (ABXY diamond rotated 90° CW):
        // Bottom (6 o'clock) = Physical A -> BTN_SOUTH (Primary confirm/action)
        // Right  (3 o'clock) = Physical X -> BTN_EAST  (Secondary action)
        // Left   (9 o'clock) = Physical B -> BTN_WEST
        // Top   (12 o'clock) = Physical Y -> BTN_NORTH
        changed |= self.gamepad.set_key(Key::BTN_SOUTH, report.is_pressed(button_mask::A));
        changed |= self.gamepad.set_key(Key::BTN_EAST, report.is_pressed(button_mask::X));
        changed |= self.gamepad.set_key(Key::BTN_WEST, report.is_pressed(button_mask::B));
        changed |= self.gamepad.set_key(Key::BTN_NORTH, report.is_pressed(button_mask::Y));

        // Shoulder bumpers on the rail
        changed |= self.gamepad.set_key(Key::BTN_TL, report.is_pressed(button_mask::SL_R));
        changed |= self.gamepad.set_key(Key::BTN_TR, report.is_pressed(button_mask::SR_R));

        // Outer triggers (for extra flexibility)
        changed |= self.gamepad.set_key(Key::BTN_TR2, report.is_pressed(button_mask::R) || report.is_pressed(button_mask::ZR));

        // System buttons
        changed |= self.gamepad.set_key(Key::BTN_START, report.is_pressed(button_mask::PLUS));
        changed |= self.gamepad.set_key(Key::BTN_MODE, report.is_pressed(button_mask::HOME));
        changed |= self.gamepad.set_key(Key::BTN_THUMBL, report.is_pressed(button_mask::R_STICK_BTN));

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


/// Which physical half of a combined pair this sink represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// One update's worth of uinput writes, used by the pure mapping tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PadState {
    /// `(key, 0|1)` for every key the side mapping touches.
    pub keys: Vec<(Key, u8)>,
    /// `(axis, value)` for every axis the side mapping touches.
    pub axes: Vec<(AbsoluteAxisType, i32)>,
}

/// Pure per-side mapping shared by `CombinedSide` and its tests.
///
/// This is a direct port of the mapping that lived in `daemon.rs`
/// (`CombinedSideSink`) and, before the worker split, in
/// `CombinedJoyConManager::update_left/right` and the horizontal variants.
/// Normal (non-horizontal) sides emit `ABS_Z`/`ABS_RZ` from the calibrated
/// trigger values, matching `gamepad.py:194-195` and the direct [`PadSink`].
/// The mapping does not force 255 when `ZL`/`ZR` is pressed: `calibrate`
/// already guarantees that, and duplicating it would collapse partial analog
/// pulls.
pub fn combined_side_state(
    side: Side,
    horizontal: bool,
    buttons: u32,
    sticks: (f32, f32, f32, f32),
    triggers: (u8, u8),
) -> PadState {
    let (lx, ly, rx, ry) = sticks;
    let pressed = |mask: u32| u8::from(buttons & mask != 0);
    let mut state = PadState::default();

    match (side, horizontal) {
        (Side::Left, false) => {
            state.axes.push((AbsoluteAxisType::ABS_X, axis_from_unit(lx)));
            state.axes.push((AbsoluteAxisType::ABS_Y, -axis_from_unit(ly)));
            state
                .axes
                .push((AbsoluteAxisType::ABS_HAT0X, hat_x(buttons)));
            state
                .axes
                .push((AbsoluteAxisType::ABS_HAT0Y, hat_y(buttons)));
            state
                .keys
                .push((Key::BTN_SELECT, pressed(button_mask::MINUS)));
            state
                .keys
                .push((Key::BTN_Z, pressed(button_mask::CAPTURE)));
            state.keys.push((Key::BTN_TL, pressed(button_mask::L)));
            state.keys.push((Key::BTN_TL2, pressed(button_mask::ZL)));
            state
                .keys
                .push((Key::BTN_THUMBL, pressed(button_mask::L_STICK_BTN)));
            state
                .axes
                .push((AbsoluteAxisType::ABS_Z, i32::from(triggers.0)));
        }
        (Side::Right, false) => {
            state.axes.push((AbsoluteAxisType::ABS_RX, axis_from_unit(rx)));
            state.axes.push((AbsoluteAxisType::ABS_RY, -axis_from_unit(ry)));
            state
                .keys
                .push((Key::BTN_SOUTH, pressed(button_mask::A)));
            state.keys.push((Key::BTN_EAST, pressed(button_mask::B)));
            state.keys.push((Key::BTN_WEST, pressed(button_mask::X)));
            state
                .keys
                .push((Key::BTN_NORTH, pressed(button_mask::Y)));
            state
                .keys
                .push((Key::BTN_START, pressed(button_mask::PLUS)));
            state
                .keys
                .push((Key::BTN_MODE, pressed(button_mask::HOME)));
            state
                .keys
                .push((Key::BTN_C, pressed(button_mask::C_BTN)));
            state.keys.push((Key::BTN_TR, pressed(button_mask::R)));
            state.keys.push((Key::BTN_TR2, pressed(button_mask::ZR)));
            state
                .keys
                .push((Key::BTN_THUMBR, pressed(button_mask::R_STICK_BTN)));
            state
                .axes
                .push((AbsoluteAxisType::ABS_RZ, i32::from(triggers.1)));
        }
        (Side::Left, true) => {
            // Split mode: 90 deg CCW, rail buttons on the bumpers.
            state.axes.push((AbsoluteAxisType::ABS_X, -axis_from_unit(ly)));
            state.axes.push((AbsoluteAxisType::ABS_Y, -axis_from_unit(lx)));
            state
                .keys
                .push((Key::BTN_SOUTH, pressed(button_mask::LEFT)));
            state
                .keys
                .push((Key::BTN_EAST, pressed(button_mask::DOWN)));
            state.keys.push((Key::BTN_WEST, pressed(button_mask::UP)));
            state
                .keys
                .push((Key::BTN_NORTH, pressed(button_mask::RIGHT)));
            state.keys.push((Key::BTN_TL, pressed(button_mask::SL_L)));
            state.keys.push((Key::BTN_TR, pressed(button_mask::SR_L)));
            state.keys.push((
                Key::BTN_TL2,
                pressed(button_mask::L) | pressed(button_mask::ZL),
            ));
            state
                .keys
                .push((Key::BTN_SELECT, pressed(button_mask::MINUS)));
            state
                .keys
                .push((Key::BTN_START, pressed(button_mask::CAPTURE)));
            state
                .keys
                .push((Key::BTN_THUMBL, pressed(button_mask::L_STICK_BTN)));
        }
        (Side::Right, true) => {
            // Split mode: 90 deg CW.
            state.axes.push((AbsoluteAxisType::ABS_X, axis_from_unit(ry)));
            state.axes.push((AbsoluteAxisType::ABS_Y, axis_from_unit(rx)));
            state
                .keys
                .push((Key::BTN_SOUTH, pressed(button_mask::A)));
            state.keys.push((Key::BTN_EAST, pressed(button_mask::X)));
            state.keys.push((Key::BTN_WEST, pressed(button_mask::B)));
            state
                .keys
                .push((Key::BTN_NORTH, pressed(button_mask::Y)));
            state.keys.push((Key::BTN_TL, pressed(button_mask::SL_R)));
            state.keys.push((Key::BTN_TR, pressed(button_mask::SR_R)));
            state.keys.push((
                Key::BTN_TR2,
                pressed(button_mask::R) | pressed(button_mask::ZR),
            ));
            state
                .keys
                .push((Key::BTN_START, pressed(button_mask::PLUS)));
            state
                .keys
                .push((Key::BTN_MODE, pressed(button_mask::HOME)));
            state
                .keys
                .push((Key::BTN_THUMBL, pressed(button_mask::R_STICK_BTN)));
        }
    }

    state
}

/// One physical half of a shared combined node, driven by calibrated worker
/// values.
#[derive(Clone)]
pub struct CombinedSide {
    gamepad: Arc<Mutex<VirtualGamepad>>,
    side: Side,
    horizontal: bool,
}

impl CombinedSide {
    pub fn new(pad: Arc<Mutex<VirtualGamepad>>, side: Side, horizontal: bool) -> Self {
        Self {
            gamepad: pad,
            side,
            horizontal,
        }
    }
}

fn lock_gamepad(gamepad: &Arc<Mutex<VirtualGamepad>>) -> MutexGuard<'_, VirtualGamepad> {
    gamepad
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl PadSink for CombinedSide {
    fn update(
        &mut self,
        buttons: u32,
        sticks: (f32, f32, f32, f32),
        triggers: (u8, u8),
    ) -> std::io::Result<()> {
        // Single source of truth: the pure mapping below is exactly what the
        // tests exercise. Apply only, then flush once like the daemon did.
        let state = combined_side_state(self.side, self.horizontal, buttons, sticks, triggers);
        let gamepad = lock_gamepad(&self.gamepad);

        for (axis, value) in state.axes {
            gamepad.set_abs(axis, value);
        }
        for (key, value) in state.keys {
            gamepad.set_key(key, value != 0);
        }
        gamepad.sync();
        Ok(())
    }

    fn release_all(&mut self) -> std::io::Result<()> {
        // A combined node is one physical pad split in two, and the mapper
        // re-emits on the next report, so neutralising the whole node is safe
        // and leaves no half-stuck keys behind (same advisory as the daemon).
        let mut gamepad = lock_gamepad(&self.gamepad);
        gamepad.release_all();
        Ok(())
    }

    fn set_player(&mut self, _player: u8) -> std::io::Result<()> {
        // The combined node has a fixed identity; per-pad player LEDs are set
        // on the controller during the connect handshake.
        Ok(())
    }
}

/// `-1.0..=1.0` -> evdev stick value, truncating exactly like
/// `StickCalibration::apply_scaled_i16` and the daemon duplicate.
fn axis_from_unit(value: f32) -> i32 {
    let scaled = value.clamp(-1.0, 1.0) * STICK_MAX as f32;
    if scaled.is_nan() {
        0
    } else {
        scaled as i32
    }
}

fn hat_x(buttons: u32) -> i32 {
    i32::from(buttons & button_mask::RIGHT != 0) - i32::from(buttons & button_mask::LEFT != 0)
}

fn hat_y(buttons: u32) -> i32 {
    i32::from(buttons & button_mask::DOWN != 0) - i32::from(buttons & button_mask::UP != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::constants::button_mask as bm;
    use crate::uinput::gamepad::KEYS;
    use std::collections::{BTreeMap, BTreeSet};

    fn axis_map(state: &PadState) -> BTreeMap<u16, i32> {
        state
            .axes
            .iter()
            .map(|(axis, value)| (axis.0, *value))
            .collect()
    }

    fn key_map(state: &PadState) -> BTreeMap<u16, u8> {
        state
            .keys
            .iter()
            .map(|(key, value)| (key.0, *value))
            .collect()
    }

    fn pressed_keys(state: &PadState) -> BTreeSet<u16> {
        state
            .keys
            .iter()
            .filter(|(_, value)| *value != 0)
            .map(|(key, _)| key.0)
            .collect()
    }

    const NEUTRAL: (f32, f32, f32, f32) = (0.0, 0.0, 0.0, 0.0);

    #[test]
    fn combined_side_left_matches_legacy_rotation() {
        let sticks = (0.25, -0.5, 0.0, 0.0);

        let normal = combined_side_state(Side::Left, false, 0, sticks, (0, 0));
        let axes = axis_map(&normal);
        assert_eq!(
            axes.get(&AbsoluteAxisType::ABS_X.0),
            Some(&axis_from_unit(0.25))
        );
        assert_eq!(
            axes.get(&AbsoluteAxisType::ABS_Y.0),
            Some(&-axis_from_unit(-0.5)),
            "normal left keeps the -ly inversion"
        );

        let horizontal = combined_side_state(Side::Left, true, 0, sticks, (0, 0));
        let axes = axis_map(&horizontal);
        assert_eq!(
            axes.get(&AbsoluteAxisType::ABS_X.0),
            Some(&-axis_from_unit(-0.5)),
            "horizontal left: physical up -> +X"
        );
        assert_eq!(
            axes.get(&AbsoluteAxisType::ABS_Y.0),
            Some(&-axis_from_unit(0.25)),
            "horizontal left: physical right -> -Y"
        );
    }

    #[test]
    fn combined_side_right_matches_legacy_rotation() {
        let sticks = (0.0, 0.0, 0.75, -0.25);

        let normal = combined_side_state(Side::Right, false, 0, sticks, (0, 0));
        let axes = axis_map(&normal);
        assert_eq!(
            axes.get(&AbsoluteAxisType::ABS_RX.0),
            Some(&axis_from_unit(0.75))
        );
        assert_eq!(
            axes.get(&AbsoluteAxisType::ABS_RY.0),
            Some(&-axis_from_unit(-0.25)),
            "normal right keeps the -ry inversion"
        );

        let horizontal = combined_side_state(Side::Right, true, 0, sticks, (0, 0));
        let axes = axis_map(&horizontal);
        assert_eq!(
            axes.get(&AbsoluteAxisType::ABS_X.0),
            Some(&axis_from_unit(-0.25)),
            "horizontal right: physical up -> -X"
        );
        assert_eq!(
            axes.get(&AbsoluteAxisType::ABS_Y.0),
            Some(&axis_from_unit(0.75)),
            "horizontal right: physical right -> +Y"
        );
    }

    #[test]
    fn horizontal_mode_applies_the_90_degree_rotation_for_both_sides() {
        let sticks = (0.4, -0.3, 0.2, 0.6);

        let left = axis_map(&combined_side_state(Side::Left, true, 0, sticks, (0, 0)));
        assert_eq!(
            left.get(&AbsoluteAxisType::ABS_X.0),
            Some(&axis_from_unit(0.3))
        );
        assert_eq!(
            left.get(&AbsoluteAxisType::ABS_Y.0),
            Some(&-axis_from_unit(0.4))
        );

        let right = axis_map(&combined_side_state(Side::Right, true, 0, sticks, (0, 0)));
        assert_eq!(
            right.get(&AbsoluteAxisType::ABS_X.0),
            Some(&axis_from_unit(0.6))
        );
        assert_eq!(
            right.get(&AbsoluteAxisType::ABS_Y.0),
            Some(&axis_from_unit(0.2))
        );

        // Neither horizontal path emits the other side's stick or hat.
        assert!(!left.contains_key(&AbsoluteAxisType::ABS_RX.0));
        assert!(!right.contains_key(&AbsoluteAxisType::ABS_RX.0));
        assert!(!left.contains_key(&AbsoluteAxisType::ABS_HAT0X.0));
    }

    #[test]
    fn trigger_and_hat_mapping_matches_gamepad() {
        let left = combined_side_state(
            Side::Left,
            false,
            bm::ZL | bm::RIGHT | bm::UP,
            NEUTRAL,
            (255, 255),
        );
        let axes = axis_map(&left);
        assert_eq!(axes.get(&AbsoluteAxisType::ABS_Z.0), Some(&255));
        assert_eq!(axes.get(&AbsoluteAxisType::ABS_HAT0X.0), Some(&1));
        assert_eq!(axes.get(&AbsoluteAxisType::ABS_HAT0Y.0), Some(&-1));

        let right = combined_side_state(
            Side::Right,
            false,
            bm::ZR | bm::LEFT | bm::DOWN,
            NEUTRAL,
            (255, 255),
        );
        let axes = axis_map(&right);
        assert_eq!(axes.get(&AbsoluteAxisType::ABS_RZ.0), Some(&255));
    }

    #[test]
    fn partial_analog_trigger_reaches_the_axis() {
        let left = combined_side_state(Side::Left, false, 0, NEUTRAL, (128, 200));
        assert_eq!(axis_map(&left).get(&AbsoluteAxisType::ABS_Z.0), Some(&128));

        let right = combined_side_state(Side::Right, false, 0, NEUTRAL, (128, 200));
        assert_eq!(
            axis_map(&right).get(&AbsoluteAxisType::ABS_RZ.0),
            Some(&200)
        );
    }

    #[test]
    fn digital_zl_still_arrives_as_full_scale() {
        let left = combined_side_state(Side::Left, false, bm::ZL | bm::ZR, NEUTRAL, (255, 255));
        assert_eq!(axis_map(&left).get(&AbsoluteAxisType::ABS_Z.0), Some(&255));

        let right = combined_side_state(Side::Right, false, bm::ZL | bm::ZR, NEUTRAL, (255, 255));
        assert_eq!(
            axis_map(&right).get(&AbsoluteAxisType::ABS_RZ.0),
            Some(&255)
        );
    }

    #[test]
    fn horizontal_mapping_still_has_no_trigger_axis() {
        for side in [Side::Left, Side::Right] {
            let state = combined_side_state(side, true, 0, NEUTRAL, (128, 200));
            let axes = axis_map(&state);
            assert!(!axes.contains_key(&AbsoluteAxisType::ABS_Z.0));
            assert!(!axes.contains_key(&AbsoluteAxisType::ABS_RZ.0));
        }
    }

    #[test]
    fn keys_are_the_same_set_as_the_direct_gamepad_mapping() {
        // Direct VirtualGamepad::update sets these keys for a left-side mask.
        let left_mask = bm::MINUS | bm::CAPTURE | bm::L | bm::ZL | bm::L_STICK_BTN;
        let left = combined_side_state(Side::Left, false, left_mask, NEUTRAL, (0, 0));
        let expected_left: BTreeSet<u16> = [
            Key::BTN_SELECT,
            Key::BTN_Z,
            Key::BTN_TL,
            Key::BTN_TL2,
            Key::BTN_THUMBL,
        ]
        .iter()
        .map(|key| key.0)
        .collect();
        assert_eq!(pressed_keys(&left), expected_left);
        assert!(pressed_keys(&left)
            .iter()
            .all(|code| KEYS.iter().any(|key| key.0 == *code)));

        let right_mask = bm::A
            | bm::B
            | bm::X
            | bm::Y
            | bm::PLUS
            | bm::HOME
            | bm::C_BTN
            | bm::R
            | bm::ZR
            | bm::R_STICK_BTN;
        let right = combined_side_state(Side::Right, false, right_mask, NEUTRAL, (0, 0));
        let expected_right: BTreeSet<u16> = [
            Key::BTN_SOUTH,
            Key::BTN_EAST,
            Key::BTN_WEST,
            Key::BTN_NORTH,
            Key::BTN_START,
            Key::BTN_MODE,
            Key::BTN_C,
            Key::BTN_TR,
            Key::BTN_TR2,
            Key::BTN_THUMBR,
        ]
        .iter()
        .map(|key| key.0)
        .collect();
        assert_eq!(pressed_keys(&right), expected_right);
        assert!(pressed_keys(&right)
            .iter()
            .all(|code| KEYS.iter().any(|key| key.0 == *code)));

        // Every key entry is present even when released, like the direct
        // gamepad mapper (which always sets the full side's key set).
        assert_eq!(key_map(&left).len(), expected_left.len());
        assert_eq!(key_map(&right).len(), expected_right.len());
    }
}
