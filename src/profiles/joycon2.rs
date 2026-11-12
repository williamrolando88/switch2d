//! Joy-Con 2 (Left `0x2067`, Right `0x2066`).
//!
//! The left Joy-Con is the validated reference path: it connected on the first
//! dial round in the hardware rounds and streamed reports with zero drops.
//! Both halves share one calibration layout and the single-block rumble
//! payload; only the vibration characteristic UUID and the documented defect
//! differ.

use super::{led_pattern_for, CalibrationAddresses, ControllerProfile, Support};
use crate::protocol::constants::{
    JOYCON2_LEFT_PID, JOYCON2_RIGHT_PID, VIBRATION_JOYCON_L_UUID, VIBRATION_JOYCON_R_UUID,
};
use crate::protocol::vibration::Layout;

const CALIBRATION: CalibrationAddresses = super::STANDARD_CALIBRATION;

/// Joy-Con 2 (Left): validated on hardware.
pub static LEFT: ControllerProfile = ControllerProfile {
    pid: JOYCON2_LEFT_PID,
    name: "Joy-Con 2 (Left)",
    support: Support::Validated,
    vibration_uuid: VIBRATION_JOYCON_L_UUID,
    rumble_layout: Layout::SingleBlock,
    analog_triggers: false,
    calibration: CALIBRATION,
    led_pattern: led_pattern_for,
};

/// Joy-Con 2 (Right): validated, with the calibration fallback documented.
pub static RIGHT: ControllerProfile = ControllerProfile {
    pid: JOYCON2_RIGHT_PID,
    name: "Joy-Con 2 (Right)",
    support: Support::ValidatedWithKnownDefect(
        "right stick calibration falls back to defaults",
    ),
    vibration_uuid: VIBRATION_JOYCON_R_UUID,
    rumble_layout: Layout::SingleBlock,
    analog_triggers: false,
    calibration: CALIBRATION,
    led_pattern: led_pattern_for,
};
