//! Face-button (A/B/X/Y) mapping: the one table the whole uinput side uses.
//!
//! # Why two layouts exist
//!
//! The kernel only has positional button codes (`BTN_SOUTH`, `BTN_EAST`,
//! `BTN_WEST`, `BTN_NORTH`); there is no `BTN_A`. Consumers differ on what they
//! mean:
//!
//! * Emulators and Switch-native games think in **Nintendo positions**: the
//!   physical bottom/right/top/left button of the pad. A player presses the
//!   button their thumb is on, so the labels must land where they physically
//!   are: A (east) -> `BTN_EAST`, B (south) -> `BTN_SOUTH`, X (north) ->
//!   `BTN_NORTH`, Y (west) -> `BTN_WEST`. This is [`FaceLayout::Positions`] and
//!   the default: it follows the hardware. It is also the fix for the install
//!   round defect, where pressing A fired the action under the thumb because we
//!   inherited the Xbox letter slots from the Python port.
//! * Xbox-style PC games think in **letters**: the primary action is always the
//!   bottom button (`BTN_SOUTH`), regardless of the label printed on the pad.
//!   Matching offline-Steam layouts needs A -> `BTN_SOUTH`, B -> `BTN_EAST`,
//!   X -> `BTN_WEST`, Y -> `BTN_NORTH`: [`FaceLayout::Labels`], i.e. the legacy
//!   `gamepad.py` table.
//!
//! One table cannot satisfy both, so the hardware convention is the default and
//! `face_button_layout` in the config selects the other.

use evdev::Key;

use crate::protocol::constants::button_mask;

/// Which convention the four face buttons follow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FaceLayout {
    /// Physical Nintendo positions (default): A east, B south, X north, Y west.
    #[default]
    Positions,
    /// Legacy/Xbox letter slots: A south, B east, X west, Y north.
    Labels,
}

impl FaceLayout {
    /// Lenient parse of the config value (`"positions"` / `"labels"`).
    ///
    /// Anything else is `None`; the config layer warns and falls back to the
    /// default instead of failing the whole load.
    pub fn from_name(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "positions" => Some(Self::Positions),
            "labels" => Some(Self::Labels),
            _ => None,
        }
    }
}

/// The four face buttons as `(evdev key, pressed)` for the chosen layout.
pub fn face_keys(layout: FaceLayout, buttons: u32) -> [(Key, bool); 4] {
    let pressed = |mask: u32| buttons & mask != 0;
    match layout {
        FaceLayout::Positions => [
            (Key::BTN_EAST, pressed(button_mask::A)),
            (Key::BTN_SOUTH, pressed(button_mask::B)),
            (Key::BTN_NORTH, pressed(button_mask::X)),
            (Key::BTN_WEST, pressed(button_mask::Y)),
        ],
        FaceLayout::Labels => [
            (Key::BTN_SOUTH, pressed(button_mask::A)),
            (Key::BTN_EAST, pressed(button_mask::B)),
            (Key::BTN_WEST, pressed(button_mask::X)),
            (Key::BTN_NORTH, pressed(button_mask::Y)),
        ],
    }
}

/// The right half held sideways (split mode, 90 degrees clockwise).
///
/// Rotating the pad moves the physical buttons, not their labels: A stays the
/// "primary" top button of the rotated grip, X the right one, and so on. The
/// face layout is deliberately *not* consulted here - a horizontal grip is a
/// different physical arrangement, and applying a letter remap on top of the
/// rotation would double-transform it.
pub fn horizontal_face_keys(buttons: u32) -> [(Key, bool); 4] {
    let pressed = |mask: u32| buttons & mask != 0;
    [
        (Key::BTN_SOUTH, pressed(button_mask::A)),
        (Key::BTN_EAST, pressed(button_mask::X)),
        (Key::BTN_WEST, pressed(button_mask::B)),
        (Key::BTN_NORTH, pressed(button_mask::Y)),
    ]
}

/// The left half held sideways (split mode, 90 degrees counter-clockwise).
///
/// The left Joy-Con has no A/B/X/Y diamond; its d-pad becomes the virtual
/// diamond, so this rotation is a d-pad mapping and (like
/// [`horizontal_face_keys`]) is independent of the face layout.
pub fn horizontal_dpad_keys(buttons: u32) -> [(Key, bool); 4] {
    let pressed = |mask: u32| buttons & mask != 0;
    [
        (Key::BTN_SOUTH, pressed(button_mask::LEFT)),
        (Key::BTN_EAST, pressed(button_mask::DOWN)),
        (Key::BTN_WEST, pressed(button_mask::UP)),
        (Key::BTN_NORTH, pressed(button_mask::RIGHT)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DaemonConfig;

    const ALL_FACE: u32 = button_mask::A | button_mask::B | button_mask::X | button_mask::Y;

    fn mapping(layout: FaceLayout) -> [(Key, bool); 4] {
        face_keys(layout, ALL_FACE)
    }

    #[test]
    fn positions_layout_matches_the_physical_diamond() {
        let keys = mapping(FaceLayout::Positions);
        assert_eq!(
            keys,
            [
                (Key::BTN_EAST, true),  // A
                (Key::BTN_SOUTH, true), // B
                (Key::BTN_NORTH, true), // X
                (Key::BTN_WEST, true),  // Y
            ]
        );

        // One button at a time, so the label -> slot link is unambiguous.
        for (mask, key) in [
            (button_mask::A, Key::BTN_EAST),
            (button_mask::B, Key::BTN_SOUTH),
            (button_mask::X, Key::BTN_NORTH),
            (button_mask::Y, Key::BTN_WEST),
        ] {
            let pressed: Vec<Key> = face_keys(FaceLayout::Positions, mask)
                .iter()
                .filter(|(_, down)| *down)
                .map(|(key, _)| *key)
                .collect();
            assert_eq!(pressed, vec![key]);
        }
    }

    #[test]
    fn labels_layout_matches_the_legacy_table() {
        assert_eq!(
            mapping(FaceLayout::Labels),
            [
                (Key::BTN_SOUTH, true), // A
                (Key::BTN_EAST, true),  // B
                (Key::BTN_WEST, true),  // X
                (Key::BTN_NORTH, true), // Y
            ]
        );
    }

    #[test]
    fn horizontal_layout_is_rotation_independent_of_face_layout() {
        // The horizontal helpers have no layout parameter at all: a config
        // change cannot alter a sideways grip. Pin both as fixed tables.
        assert_eq!(
            horizontal_face_keys(ALL_FACE),
            [
                (Key::BTN_SOUTH, true),
                (Key::BTN_EAST, true),
                (Key::BTN_WEST, true),
                (Key::BTN_NORTH, true),
            ]
        );
        assert_eq!(
            horizontal_dpad_keys(button_mask::LEFT | button_mask::DOWN),
            [
                (Key::BTN_SOUTH, true),
                (Key::BTN_EAST, true),
                (Key::BTN_WEST, false),
                (Key::BTN_NORTH, false),
            ]
        );

        // And they remain distinguishable from the vertical tables.
        assert_ne!(
            horizontal_face_keys(ALL_FACE),
            mapping(FaceLayout::Positions),
            "the sideways grip is its own rotation"
        );
    }

    #[test]
    fn config_defaults_to_positions() {
        assert_eq!(
            DaemonConfig::default().face_button_layout,
            FaceLayout::Positions
        );
        // Absent field (older config files) keeps the default.
        let parsed: DaemonConfig = serde_json::from_str("{}").expect("empty config parses");
        assert_eq!(parsed.face_button_layout, FaceLayout::Positions);
    }

    #[test]
    fn unknown_face_layout_value_falls_back_to_positions() {
        let parsed: DaemonConfig =
            serde_json::from_str(r#"{"face_button_layout": "xbox"}"#).expect("tolerant parse");
        assert_eq!(parsed.face_button_layout, FaceLayout::Positions);

        let labels: DaemonConfig =
            serde_json::from_str(r#"{"face_button_layout": "labels"}"#).expect("labels parse");
        assert_eq!(labels.face_button_layout, FaceLayout::Labels);

        let positions: DaemonConfig =
            serde_json::from_str(r#"{"face_button_layout": "positions"}"#)
                .expect("positions parse");
        assert_eq!(positions.face_button_layout, FaceLayout::Positions);

        // Non-string values are tolerated too.
        let odd: DaemonConfig =
            serde_json::from_str(r#"{"face_button_layout": 42}"#).expect("tolerant parse");
        assert_eq!(odd.face_button_layout, FaceLayout::Positions);
    }
}
