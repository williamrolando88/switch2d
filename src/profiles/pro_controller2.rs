//! Pro Controller 2 (`0x2069`).
//!
//! Speculative: the profile is derived from the validated Joy-Con 2 path plus
//! the dual-block rumble layout and the Pro vibration UUID in the protocol
//! constants, but no Pro Controller 2 has been through a hardware round yet.

use super::{led_pattern_for, ControllerProfile, Support};
use crate::protocol::constants::{PRO_CONTROLLER2_PID, VIBRATION_PRO_CONTROLLER_UUID};
use crate::protocol::vibration::Layout;

pub static PROFILE: ControllerProfile = ControllerProfile {
    pid: PRO_CONTROLLER2_PID,
    name: "Pro Controller 2",
    support: Support::Speculative,
    vibration_uuid: VIBRATION_PRO_CONTROLLER_UUID,
    rumble_layout: Layout::DualBlock,
    analog_triggers: false,
    calibration: super::STANDARD_CALIBRATION,
    led_pattern: led_pattern_for,
};
