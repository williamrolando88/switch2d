//! NSO GameCube controller (`0x2073`).
//!
//! Speculative: the only model with real analog `L`/`R` triggers, and its
//! rumble is a finite built-in preset (`0x0A/0x02`) rather than a sustained
//! waveform, so the haptic path differs from the HD-rumble pads.

use super::{
    led_pattern_for, CalibrationAddresses, ControllerProfile, RumbleKind, Sticks, Support,
};
use crate::protocol::constants::{
    CALIBRATION_GC_TRIGGERS, CALIBRATION_JOYSTICK_1, CALIBRATION_JOYSTICK_2,
    CALIBRATION_USER_JOYSTICK_1, CALIBRATION_USER_JOYSTICK_2, NSO_GAMECUBE_PID,
    VIBRATION_PRO_CONTROLLER_UUID,
};
use crate::protocol::vibration::Layout;

/// Same stick addresses as the other pads plus the trigger-neutral block.
const CALIBRATION: CalibrationAddresses = CalibrationAddresses {
    user_left: CALIBRATION_USER_JOYSTICK_1,
    factory_left: CALIBRATION_JOYSTICK_1,
    user_right: CALIBRATION_USER_JOYSTICK_2,
    factory_right: CALIBRATION_JOYSTICK_2,
    gc_triggers: Some(CALIBRATION_GC_TRIGGERS),
};

pub static PROFILE: ControllerProfile = ControllerProfile {
    pid: NSO_GAMECUBE_PID,
    name: "NSO GameCube Controller",
    support: Support::Speculative,
    sticks: Sticks::Both,
    vibration_uuid: VIBRATION_PRO_CONTROLLER_UUID,
    rumble_layout: Layout::SingleBlock,
    rumble: RumbleKind::PresetOnly,
    analog_triggers: true,
    calibration: CALIBRATION,
    led_pattern: led_pattern_for,
};
