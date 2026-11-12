//! Controller profiles: one module per model, one data table.
//!
//! This exists so a contributor with a GameCube pad can work inside
//! `profiles/gamecube.rs` without being able to regress the validated Joy-Con 2
//! path: PID-specific behaviour is *data* here, and `controller.rs` performs a
//! single lookup instead of `match pid` branches.
//!
//! Nothing in this module performs I/O; the profiles are plain static data.

pub mod gamecube;
pub mod joycon2;
pub mod pro_controller2;

use crate::protocol::constants::{
    CALIBRATION_JOYSTICK_1, CALIBRATION_JOYSTICK_2, CALIBRATION_USER_JOYSTICK_1,
    CALIBRATION_USER_JOYSTICK_2, LED_PATTERNS,
};

/// How much confidence the project has in this model's behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Exercised on real hardware by the validation rounds.
    Validated,
    /// Works, but a known defect is documented in the string.
    ValidatedWithKnownDefect(&'static str),
    /// Written from the protocol/legacy source, not yet validated on hardware.
    Speculative,
}

/// Per-stick calibration addresses (`None` for the trigger block on pads
/// without analog triggers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalibrationAddresses {
    pub user_left: u32,
    pub factory_left: u32,
    pub user_right: u32,
    pub factory_right: u32,
    pub gc_triggers: Option<u32>,
}

/// The calibration addresses shared by every Switch 2 pad we know about.
pub const STANDARD_CALIBRATION: CalibrationAddresses = CalibrationAddresses {
    user_left: CALIBRATION_USER_JOYSTICK_1,
    factory_left: CALIBRATION_JOYSTICK_1,
    user_right: CALIBRATION_USER_JOYSTICK_2,
    factory_right: CALIBRATION_JOYSTICK_2,
    gc_triggers: None,
};

/// Which calibration block(s) a model actually populates.
///
/// A single-stick Joy-Con 2 stores its stick calibration in the LEFT block even
/// when the physical stick reports on the right axis; `controller.rs` uses this
/// to avoid handing the right stick the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sticks {
    /// Only the left calibration block is populated.
    Left,
    /// Only the right calibration block is populated.
    Right,
    /// Both blocks are populated independently (Pro Controller 2, GameCube).
    Both,
}

/// How this model turns a force-feedback target into rumble packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RumbleKind {
    /// Sustained HD waveform: one 0x0A/0x10 packet per ~16 ms while active.
    Hd,
    /// Finite built-in clip (0x0A/0x02), retriggered on a wall-clock gate.
    PresetOnly,
}

/// Everything `controller.rs` needs to know about one controller model.
#[derive(Debug, Clone, Copy)]
pub struct ControllerProfile {
    pub pid: u16,
    pub name: &'static str,
    pub support: Support,
    /// Which calibration block(s) this model populates.
    pub sticks: Sticks,
    /// Vibration characteristic UUID (PID-specific on Joy-Con 2).
    pub vibration_uuid: &'static str,
    /// Single block (Joy-Con / GameCube) or dual block (Pro Controller 2).
    pub rumble_layout: crate::protocol::vibration::Layout,
    /// HD sustainer or finite preset clips (analysis §8.5).
    pub rumble: RumbleKind,
    /// True only for the NSO GameCube pad (`L`/`R` are real analog triggers).
    pub analog_triggers: bool,
    pub calibration: CalibrationAddresses,
    /// LED table lookup; must clamp the player slot to `1..=8`.
    pub led_pattern: fn(u8) -> u8,
}

/// Shared LED table lookup: player slots are clamped to `1..=8`.
///
/// This is the legacy `LED_PATTERNS` behaviour and the implementation every
/// profile points at unless it has a reason not to.
pub fn led_pattern_for(player: u8) -> u8 {
    LED_PATTERNS[(player as usize).clamp(1, 8)]
}

/// Every profile, in discovery/log order (Joy-Con left, right, Pro, GC).
const ALL: [&ControllerProfile; 4] = [
    &joycon2::LEFT,
    &joycon2::RIGHT,
    &pro_controller2::PROFILE,
    &gamecube::PROFILE,
];

/// All known profiles.
pub fn all_profiles() -> &'static [&'static ControllerProfile] {
    &ALL
}

/// Look up a profile by product id; `None` for a model we have no profile for.
pub fn profile_for(pid: u16) -> Option<&'static ControllerProfile> {
    ALL.iter().copied().find(|profile| profile.pid == pid)
}

/// Profile for `pid`, falling back to the Pro Controller 2 profile (the
/// generic one) for models we have no profile for.
///
/// Unknown PIDs are *not* refused (a future pad should still be testable); the
/// caller logs [`profile_is_known`] once so the fallback is never invisible.
pub fn profile_or_default(pid: u16) -> &'static ControllerProfile {
    profile_for(pid).unwrap_or(&pro_controller2::PROFILE)
}

/// Whether `pid` has a dedicated profile (false => generic fallback in use).
pub fn profile_is_known(pid: u16) -> bool {
    profile_for(pid).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::constants::{
        JOYCON2_LEFT_PID, JOYCON2_RIGHT_PID, NSO_GAMECUBE_PID, PRO_CONTROLLER2_PID,
    };

    #[test]
    fn stick_layout_matches_the_model() {
        assert_eq!(
            profile_for(JOYCON2_LEFT_PID).expect("left").sticks,
            Sticks::Left
        );
        assert_eq!(
            profile_for(JOYCON2_RIGHT_PID).expect("right").sticks,
            Sticks::Right
        );
        assert_eq!(
            profile_for(PRO_CONTROLLER2_PID).expect("pro").sticks,
            Sticks::Both
        );
        assert_eq!(
            profile_for(NSO_GAMECUBE_PID).expect("gc").sticks,
            Sticks::Both
        );
    }

    #[test]
    fn registry_covers_every_known_pid() {
        let pids = [
            JOYCON2_LEFT_PID,
            JOYCON2_RIGHT_PID,
            PRO_CONTROLLER2_PID,
            NSO_GAMECUBE_PID,
        ];

        for pid in pids {
            let profile = profile_for(pid).unwrap_or_else(|| panic!("no profile for 0x{pid:04X}"));
            assert_eq!(profile.pid, pid);
            assert!(profile_is_known(pid));
            assert!(!profile.name.is_empty());
            assert!(!profile.vibration_uuid.is_empty());
        }

        assert_eq!(all_profiles().len(), pids.len());
        for profile in all_profiles() {
            assert!(pids.contains(&profile.pid), "unexpected pid in registry");
        }
    }

    #[test]
    fn joycon_left_and_right_have_distinct_vibration_uuids() {
        let left = profile_for(JOYCON2_LEFT_PID).expect("left profile");
        let right = profile_for(JOYCON2_RIGHT_PID).expect("right profile");

        assert_ne!(left.vibration_uuid, right.vibration_uuid);
        assert_eq!(left.name, "Joy-Con 2 (Left)");
        assert_eq!(right.name, "Joy-Con 2 (Right)");
        assert_eq!(left.support, Support::Validated);
        assert!(matches!(
            right.support,
            Support::ValidatedWithKnownDefect(_)
        ));
    }

    #[test]
    fn gamecube_profile_is_marked_speculative_and_has_analog_triggers() {
        let gc = profile_for(NSO_GAMECUBE_PID).expect("gamecube profile");
        assert_eq!(gc.support, Support::Speculative);
        assert!(gc.analog_triggers);
        assert_eq!(
            gc.calibration.gc_triggers,
            Some(crate::protocol::constants::CALIBRATION_GC_TRIGGERS)
        );
        assert_eq!(gc.rumble_layout, crate::protocol::vibration::Layout::SingleBlock);

        // Every other model uses the digital ZL/ZR path and has no trigger block.
        for profile in all_profiles() {
            if profile.pid == NSO_GAMECUBE_PID {
                continue;
            }
            assert!(!profile.analog_triggers, "{}", profile.name);
            assert_eq!(profile.calibration.gc_triggers, None, "{}", profile.name);
        }
    }

    #[test]
    fn rumble_layout_matches_the_vibration_encoder() {
        // `protocol::vibration::layout_for` still owns the layout branch until
        // the Sustainer takes a profile; pin the two so they cannot drift.
        for profile in all_profiles() {
            assert_eq!(
                profile.rumble_layout,
                crate::protocol::vibration::layout_for(profile.pid),
                "{}",
                profile.name
            );
        }
    }

    #[test]
    fn unknown_pid_falls_back_to_the_generic_profile_with_a_warning_flag() {
        let unknown = 0xFFFF;
        assert!(!profile_is_known(unknown));
        let fallback = profile_or_default(unknown);
        assert_eq!(fallback.pid, PRO_CONTROLLER2_PID);
        // The fallback is the same object a real Pro Controller 2 gets.
        assert!(std::ptr::eq(
            fallback,
            profile_for(PRO_CONTROLLER2_PID).expect("pro profile")
        ));
    }

    #[test]
    fn led_pattern_is_clamped_for_every_profile() {
        for profile in all_profiles() {
            for player in 0u8..=9 {
                let expected = LED_PATTERNS[(player as usize).clamp(1, 8)];
                assert_eq!(
                    (profile.led_pattern)(player),
                    expected,
                    "{} player {player}",
                    profile.name
                );
            }
            // Slot 0 behaves as slot 1, slot 9 as slot 8 (legacy clamp).
            assert_eq!((profile.led_pattern)(0), (profile.led_pattern)(1));
            assert_eq!((profile.led_pattern)(9), (profile.led_pattern)(8));
        }
    }
}
