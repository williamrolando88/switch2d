//! Force-feedback decode layer (no I/O).
//!
//! Kernel side (`UI_BEGIN_FF_UPLOAD` / `UI_BEGIN_FF_ERASE`) lives in
//! [`super::rawio`]; this module only turns `EV_FF` / `EV_UINPUT` events and
//! uploaded [`FfEffect`]s into state.
//!
//! Deliberate deviation from legacy (analysis §8.3, ADR deviations #8):
//! `FF_GAIN` is a global multiplier and `FF_AUTOCENTER` is ignored. Legacy
//! let both fall through the effect-id path, where they aliased ids 96/97 and
//! silently killed rumble.

use std::collections::HashMap;

use super::rawio::{FfEffect, RawInputEvent};

pub const FF_RUMBLE: u16 = 0x50;
pub const FF_GAIN: u16 = 0x60;
pub const FF_AUTOCENTER: u16 = 0x61;

/// `EV_FF`.
const EV_FF: u16 = 0x15;
/// `EV_UINPUT` (kernel-internal FF request transport).
const EV_UINPUT: u16 = 0x0101;
/// `UI_FF_UPLOAD` / `UI_FF_ERASE` event codes on `EV_UINPUT`.
const UI_FF_UPLOAD: u16 = 1;
const UI_FF_ERASE: u16 = 2;
/// Full-scale gain in the kernel ABI.
const GAIN_MAX: u16 = u16::MAX;

/// Stored rumble magnitudes for one effect (0..=65535).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EffectMagnitudes {
    pub strong: u16,
    pub weak: u16,
}

/// What one decode step produced.
#[derive(Debug, Clone, PartialEq)]
pub enum FfEvent {
    /// A rumble effect was stored under `id`.
    Uploaded {
        id: i16,
        magnitudes: EffectMagnitudes,
    },
    /// The effect with `id` was removed.
    Erased { id: i16 },
    /// Effect `id` started playing with the current global `gain`.
    Played { id: i16, gain: f32 },
    /// `EV_FF` value 0: motors to (0, 0), stored magnitudes untouched.
    Stopped,
    /// Global gain changed.
    GainChanged(f32),
    /// Not relevant to rumble (non-rumble effect, autocenter, unknown code).
    Ignored,
}

/// Force-feedback state: stored effects, global gain, current target.
#[derive(Debug, Clone)]
pub struct FfState {
    effects: HashMap<i16, EffectMagnitudes>,
    /// Global gain in kernel units (0..=65535); scaled into `f32` on demand.
    gain_raw: u16,
    /// Gained target of the last play, frozen so later gain changes do not
    /// retroactively alter the running effect.
    active: (u16, u16),
}

impl Default for FfState {
    fn default() -> Self {
        Self::new()
    }
}

impl FfState {
    pub fn new() -> Self {
        Self {
            effects: HashMap::new(),
            gain_raw: GAIN_MAX,
            active: (0, 0),
        }
    }

    /// Decode one kernel event.
    ///
    /// `EV_UINPUT` upload/erase *requests* are acknowledged by the caller via
    /// the rawio ioctls (the effect payload is not in the event), so they decode
    /// to [`FfEvent::Ignored`] here.
    pub fn handle(&mut self, event: &RawInputEvent) -> FfEvent {
        match event.kind {
            EV_FF => match event.code {
                FF_GAIN => {
                    // 0..65535 -> 0.0..=1.0.
                    self.gain_raw = self.gain_raw_from_value(event.value);
                    FfEvent::GainChanged(self.gain())
                }
                FF_AUTOCENTER => FfEvent::Ignored,
                // Effect ids are allocated from 0 upward; anything in the
                // FF_* constant range is a control/type code, never one of our
                // (few) rumble ids. Ignoring them is what makes legacy bug 8
                // impossible rather than merely fixed for 0x60/0x61.
                code if code >= FF_RUMBLE => FfEvent::Ignored,
                id => {
                    if event.value == 0 {
                        self.active = (0, 0);
                        return FfEvent::Stopped;
                    }

                    let id = id as i16;
                    let magnitudes = self.effects.get(&id).copied().unwrap_or_default();
                    self.active = self.scale(magnitudes);
                    FfEvent::Played {
                        id,
                        gain: self.gain(),
                    }
                }
            },
            EV_UINPUT if event.code == UI_FF_UPLOAD || event.code == UI_FF_ERASE => {
                // The caller performs begin/end_ff_upload or begin/end_ff_erase.
                FfEvent::Ignored
            }
            _ => FfEvent::Ignored,
        }
    }

    /// Store an uploaded effect. Only `FF_RUMBLE` is kept.
    pub fn on_upload(&mut self, effect: &FfEffect) -> FfEvent {
        if effect.type_ != FF_RUMBLE {
            return FfEvent::Ignored;
        }

        let (strong, weak) = effect.rumble_magnitudes();
        let magnitudes = EffectMagnitudes { strong, weak };
        self.effects.insert(effect.id, magnitudes);

        FfEvent::Uploaded {
            id: effect.id,
            magnitudes,
        }
    }

    /// Remove an effect. Stored magnitudes go away; playback is unaffected
    /// until the next play/stop event.
    pub fn on_erase(&mut self, effect_id: u16) -> FfEvent {
        let id = effect_id as i16;
        self.effects.remove(&id);
        FfEvent::Erased { id }
    }

    /// Current motor target `(strong, weak)`, already gained.
    pub fn current_target(&self) -> (u16, u16) {
        self.active
    }

    fn gain(&self) -> f32 {
        f32::from(self.gain_raw) / f32::from(GAIN_MAX)
    }

    fn gain_raw_from_value(&self, value: i32) -> u16 {
        value.clamp(0, i32::from(GAIN_MAX)) as u16
    }

    /// Integer gain scaling (exact, deterministic: `mag * gain / 65535`).
    fn scale(&self, magnitudes: EffectMagnitudes) -> (u16, u16) {
        let scaled = |magnitude: u16| -> u16 {
            ((u32::from(magnitude) * u32::from(self.gain_raw)) / u32::from(GAIN_MAX)) as u16
        };
        (scaled(magnitudes.strong), scaled(magnitudes.weak))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: u16, code: u16, value: i32) -> RawInputEvent {
        RawInputEvent::new(kind, code, value)
    }

    fn upload(state: &mut FfState, id: i16, strong: u16, weak: u16) -> FfEvent {
        state.on_upload(&FfEffect::rumble(id, strong, weak))
    }

    #[test]
    fn uploads_store_strong_and_weak_magnitudes() {
        let mut state = FfState::new();

        assert_eq!(
            upload(&mut state, 3, 0xFFFF, 0x8000),
            FfEvent::Uploaded {
                id: 3,
                magnitudes: EffectMagnitudes {
                    strong: 0xFFFF,
                    weak: 0x8000,
                },
            }
        );

        // Stored values surface on play (gain defaults to 1.0).
        assert_eq!(
            state.handle(&event(EV_FF, 3, 1)),
            FfEvent::Played { id: 3, gain: 1.0 }
        );
        assert_eq!(state.current_target(), (0xFFFF, 0x8000));
    }

    #[test]
    fn upload_of_non_rumble_effect_is_ignored() {
        let mut state = FfState::new();
        let mut effect = FfEffect::rumble(9, 0xFFFF, 0xFFFF);
        effect.type_ = 0x51; // FF_PERIODIC

        assert_eq!(state.on_upload(&effect), FfEvent::Ignored);

        // Not stored: playing the id yields (0, 0).
        state.handle(&event(EV_FF, 9, 1));
        assert_eq!(state.current_target(), (0, 0));
    }

    #[test]
    fn erase_removes_effect() {
        let mut state = FfState::new();
        upload(&mut state, 4, 0x1234, 0x5678);

        assert_eq!(state.on_erase(4), FfEvent::Erased { id: 4 });
        assert_eq!(
            state.handle(&event(EV_FF, 4, 1)),
            FfEvent::Played { id: 4, gain: 1.0 }
        );
        assert_eq!(state.current_target(), (0, 0));

        // Erasing an unknown id is harmless.
        assert_eq!(state.on_erase(77), FfEvent::Erased { id: 77 });
    }

    #[test]
    fn play_unknown_id_targets_zero() {
        let mut state = FfState::new();
        assert_eq!(
            state.handle(&event(EV_FF, 42, 1)),
            FfEvent::Played { id: 42, gain: 1.0 }
        );
        assert_eq!(state.current_target(), (0, 0));
    }

    #[test]
    fn play_scales_by_gain() {
        let mut state = FfState::new();
        upload(&mut state, 5, 0xFFFF, 0x8000);

        // GAIN 0x8000 == 0.50000763...
        let gain = f32::from(0x8000u16) / f32::from(u16::MAX);
        assert_eq!(
            state.handle(&event(EV_FF, FF_GAIN, 0x8000)),
            FfEvent::GainChanged(gain)
        );

        state.handle(&event(EV_FF, 5, 1));
        assert_eq!(state.current_target(), (0x8000, 0x4000));

        // Gain 0 silences without losing the stored effect.
        state.handle(&event(EV_FF, FF_GAIN, 0));
        state.handle(&event(EV_FF, 5, 1));
        assert_eq!(state.current_target(), (0, 0));
    }

    #[test]
    fn gain_change_does_not_disturb_active_effect() {
        let mut state = FfState::new();
        upload(&mut state, 6, 0xFFFF, 0xFFFF);
        state.handle(&event(EV_FF, 6, 1));
        assert_eq!(state.current_target(), (0xFFFF, 0xFFFF));

        let gain = f32::from(0x4000u16) / f32::from(u16::MAX);
        assert_eq!(
            state.handle(&event(EV_FF, FF_GAIN, 0x4000)),
            FfEvent::GainChanged(gain)
        );
        assert_eq!(
            state.current_target(),
            (0xFFFF, 0xFFFF),
            "a running effect is frozen at its play-time gain"
        );

        // The new gain applies to the next play.
        state.handle(&event(EV_FF, 6, 1));
        assert_eq!(state.current_target(), (0x4000, 0x4000));
    }

    #[test]
    fn autocenter_and_unknown_codes_are_ignored() {
        let mut state = FfState::new();
        upload(&mut state, 1, 0xFFFF, 0xFFFF);
        state.handle(&event(EV_FF, 1, 1));
        let active = state.current_target();
        assert_ne!(active, (0, 0));

        assert_eq!(
            state.handle(&event(EV_FF, FF_AUTOCENTER, 0x1234)),
            FfEvent::Ignored
        );
        assert_eq!(state.handle(&event(EV_FF, 0x63, 1)), FfEvent::Ignored);
        assert_eq!(state.handle(&event(0x02, 0x00, 1)), FfEvent::Ignored);
        assert_eq!(
            state.handle(&event(EV_UINPUT, UI_FF_UPLOAD, 1)),
            FfEvent::Ignored
        );
        assert_eq!(
            state.handle(&event(EV_UINPUT, UI_FF_ERASE, 1)),
            FfEvent::Ignored
        );
        assert_eq!(state.handle(&event(EV_UINPUT, 0x77, 1)), FfEvent::Ignored);

        // Ignored codes never disturb effect state (legacy bug 8).
        assert_eq!(state.current_target(), active);
        // And autocenter/gain did not alias an effect id.
        assert_eq!(
            state.handle(&event(EV_FF, FF_AUTOCENTER, 0)),
            FfEvent::Ignored
        );
        assert_eq!(state.current_target(), active);
    }

    #[test]
    fn stop_zeroes_target_but_keeps_stored_magnitudes() {
        let mut state = FfState::new();
        upload(&mut state, 8, 0x8000, 0x4000);
        state.handle(&event(EV_FF, 8, 1));
        assert_eq!(state.current_target(), (0x8000, 0x4000));

        assert_eq!(state.handle(&event(EV_FF, 8, 0)), FfEvent::Stopped);
        assert_eq!(state.current_target(), (0, 0));

        // The effect is still stored: replaying restores the target.
        state.handle(&event(EV_FF, 8, 1));
        assert_eq!(state.current_target(), (0x8000, 0x4000));
    }

    #[test]
    fn current_target_reflects_last_play() {
        let mut state = FfState::new();
        upload(&mut state, 10, 0x1000, 0x2000);
        upload(&mut state, 11, 0x3000, 0x4000);

        state.handle(&event(EV_FF, 10, 1));
        assert_eq!(state.current_target(), (0x1000, 0x2000));

        state.handle(&event(EV_FF, 11, 1));
        assert_eq!(state.current_target(), (0x3000, 0x4000));

        // Stopping resets to zero regardless of which effect was last.
        state.handle(&event(EV_FF, 11, 0));
        assert_eq!(state.current_target(), (0, 0));
    }
}
