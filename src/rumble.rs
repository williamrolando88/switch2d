//! Pure force-feedback routing: FF target -> rumble packet decision.
//!
//! No sockets, threads, I/O, or sleeping live here. The caller owns the clock:
//! [`RumbleEngine::set_target`] on every drained FF event and
//! [`RumbleEngine::tick`] on a ~16 ms timer (or each input-loop pass).
//!
//! HD pads keep the legacy sustained-waveform lifetime
//! (`protocol::vibration::Sustainer`): one packet per 16 ms while active and
//! exactly one silent stop packet on release. The GameCube path only plays
//! finite built-in clips, so it uses the wall-clock `PresetGate` from analysis
//! §8.5. This module routes between those two proven state machines; it does
//! not re-derive their timing rules.

use std::time::Instant;

use crate::profiles::{ControllerProfile, RumbleKind};
use crate::protocol::vibration::{is_stop, Layout, Preset, PresetGate, Sustainer};

/// One decision for the caller to send to the controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RumbleAction {
    /// HD vibration payload (`0x00 | block...`, 17 or 33 bytes).
    HdPacket(Vec<u8>),
    /// GameCube `0x0A/0x02` built-in clip.
    Preset(Preset),
}

/// Model-specific rumble router. State is retained across calls so uploads,
/// gain changes, sustain timing, and the preset gate behave like one engine.
#[derive(Debug, Clone)]
pub struct RumbleEngine {
    kind: RumbleKind,
    layout: Layout,
    sustainer: Sustainer,
    gate: PresetGate,
    target: (f32, f32),
    /// A preset produced by `set_target` but not yet collected by `tick`.
    pending_preset: Option<Preset>,
}

impl RumbleEngine {
    /// Build the router for one controller profile. The sustainer takes the
    /// profile PID so its payload layout matches that model.
    pub fn new(profile: &'static ControllerProfile) -> Self {
        Self {
            kind: profile.rumble,
            layout: profile.rumble_layout,
            sustainer: Sustainer::new(profile.pid),
            gate: PresetGate::new(),
            target: (0.0, 0.0),
            pending_preset: None,
        }
    }

    /// Set the desired kernel-space magnitudes (`0..=65535`) and the event
    /// timestamp. HD pads forward this to the sustainer immediately; preset
    /// pads run the gate now and queue at most one preset for the next tick.
    pub fn set_target(&mut self, strong: u16, weak: u16, now: Instant) {
        let target = (to_unit(strong), to_unit(weak));
        self.target = target;

        match self.kind {
            RumbleKind::Hd => self.sustainer.set_target(target.0, target.1),
            RumbleKind::PresetOnly => {
                let preset = self.gate.consider(now, target.0, target.1);
                if is_stop(target.0, target.1) {
                    // A release before the timer fires cancels any queued clip;
                    // a silent target must not rumble afterwards.
                    self.pending_preset = None;
                } else if let Some(preset) = preset {
                    self.pending_preset = Some(preset);
                }
            }
        }
    }

    /// Ask for the next action at `now`. Called on the caller's ~16 ms timer.
    ///
    /// HD: at most one sustained packet per [`crate::protocol::vibration::SUSTAIN_INTERVAL`],
    /// or exactly one stop packet after the target goes quiet. Preset: yields a
    /// queued preset once, then `None` until the gate allows another.
    pub fn tick(&mut self, now: Instant) -> Option<RumbleAction> {
        match self.kind {
            RumbleKind::Hd => self.sustainer.tick(now).map(RumbleAction::HdPacket),
            RumbleKind::PresetOnly => self.pending_preset.take().map(RumbleAction::Preset),
        }
    }

    /// Last target in normalized `0.0..=1.0` units.
    pub fn target(&self) -> (f32, f32) {
        self.target
    }

    /// Payload layout this engine encodes for (profile-derived).
    pub fn layout(&self) -> Layout {
        self.layout
    }
}

fn to_unit(value: u16) -> f32 {
    f32::from(value) / f32::from(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{all_profiles, profile_for};
    use crate::protocol::constants::{JOYCON2_LEFT_PID, NSO_GAMECUBE_PID, PRO_CONTROLLER2_PID};
    use crate::protocol::vibration::{
        Layout, Preset, Sample, GC_SUSTAIN_INTERVAL, SUSTAIN_INTERVAL,
    };
    use std::time::{Duration, Instant};

    fn profile(pid: u16) -> &'static ControllerProfile {
        profile_for(pid).unwrap_or_else(|| panic!("missing profile for 0x{pid:04X}"))
    }

    fn sample(payload: &[u8]) -> &[u8] {
        &payload[2..7]
    }

    fn hd_packet(action: Option<RumbleAction>) -> Vec<u8> {
        match action {
            Some(RumbleAction::HdPacket(payload)) => payload,
            other => panic!("expected HdPacket, got {other:?}"),
        }
    }

    #[test]
    fn hd_target_ramps_then_stops_with_one_stop_packet() {
        let t0 = Instant::now();
        let mut engine = RumbleEngine::new(profile(JOYCON2_LEFT_PID));

        // First target is emitted immediately, then updated on the 16 ms grid.
        engine.set_target(0x8000, 0, t0);
        assert_eq!(engine.target(), (0.5000076, 0.0));
        let first = hd_packet(engine.tick(t0));
        assert_eq!(first.len(), 17, "Joy-Con uses one block");
        assert_ne!(sample(&first), &Sample::default().to_u40());

        engine.set_target(0xFFFF, 0, t0 + Duration::from_millis(1));
        assert!(engine.tick(t0 + Duration::from_millis(15)).is_none());
        let second = hd_packet(engine.tick(t0 + SUSTAIN_INTERVAL));
        assert_ne!(
            sample(&second),
            sample(&first),
            "the raised target reaches the wire on the next due packet"
        );

        engine.set_target(0, 0, t0 + SUSTAIN_INTERVAL);
        let stop = hd_packet(engine.tick(t0 + SUSTAIN_INTERVAL * 2));
        assert_eq!(sample(&stop), &Sample::default().to_u40());
        assert!(
            engine.tick(t0 + SUSTAIN_INTERVAL * 3).is_none(),
            "exactly one stop packet"
        );
    }

    #[test]
    fn hd_emits_at_most_one_packet_per_16ms_tick() {
        let t0 = Instant::now();
        let mut engine = RumbleEngine::new(profile(PRO_CONTROLLER2_PID));
        engine.set_target(0xFFFF, 0, t0);

        let first = hd_packet(engine.tick(t0));
        assert_eq!(first.len(), 33, "Pro Controller uses two blocks");
        assert!(
            engine.tick(t0).is_none(),
            "a second tick at the same instant must not produce a burst"
        );
        assert!(engine.tick(t0 + Duration::from_millis(15)).is_none());
        assert!(engine.tick(t0 + SUSTAIN_INTERVAL).is_some());
        assert!(
            engine.tick(t0 + SUSTAIN_INTERVAL).is_none(),
            "no catch-up packet at the same timestamp"
        );
    }

    #[test]
    fn preset_only_profile_emits_one_preset_per_gate_window() {
        let t0 = Instant::now();
        let mut engine = RumbleEngine::new(profile(NSO_GAMECUBE_PID));

        engine.set_target(0xFFFF, 0, t0);
        assert_eq!(
            engine.tick(t0),
            Some(RumbleAction::Preset(Preset::Strong)),
            "queued preset survives until the next tick"
        );
        assert_eq!(engine.tick(t0), None, "emitted exactly once");

        // Held target inside the 220 ms window: no new clip.
        engine.set_target(0xFFFF, 0, t0 + Duration::from_millis(100));
        assert_eq!(engine.tick(t0 + Duration::from_millis(100)), None);

        // Due again after the gate interval.
        let due = t0 + GC_SUSTAIN_INTERVAL;
        engine.set_target(0xFFFF, 0, due);
        assert_eq!(engine.tick(due), Some(RumbleAction::Preset(Preset::Strong)));
        assert_eq!(engine.tick(due), None, "still exactly one");
    }

    #[test]
    fn preset_only_never_emits_hd_packets() {
        let t0 = Instant::now();
        let mut engine = RumbleEngine::new(profile(NSO_GAMECUBE_PID));

        for seconds in [0u64, 1, 2, 3] {
            let now = t0 + Duration::from_secs(seconds);
            for magnitude in [0xFFFFu16, 0x8000, 0x4000, 0] {
                engine.set_target(magnitude, 0, now);
                for _ in 0..2 {
                    if let Some(action) = engine.tick(now) {
                        assert!(
                            matches!(action, RumbleAction::Preset(_)),
                            "GameCube must never produce an HD packet: {action:?}"
                        );
                    }
                }
            }
        }

        assert_eq!(
            engine.layout(),
            Layout::SingleBlock,
            "GameCube still uses the single-block layout constant"
        );
    }

    #[test]
    fn zero_target_never_produces_a_nonzero_amplitude() {
        let t0 = Instant::now();
        let mut hd = RumbleEngine::new(profile(JOYCON2_LEFT_PID));

        // Fresh engine + zero target: no packet at all.
        hd.set_target(0, 0, t0);
        assert_eq!(hd.tick(t0), None);

        // Active then released: the only packet after release is the silent stop.
        hd.set_target(0xFFFF, 0, t0);
        assert!(hd.tick(t0).is_some());
        hd.set_target(0, 0, t0 + SUSTAIN_INTERVAL);
        let stop = hd_packet(hd.tick(t0 + SUSTAIN_INTERVAL * 2));
        assert_eq!(sample(&stop), &Sample::default().to_u40());

        // Preset path: a zero target before the timer cancels the queued clip.
        let mut gc = RumbleEngine::new(profile(NSO_GAMECUBE_PID));
        gc.set_target(0xFFFF, 0, t0);
        gc.set_target(0, 0, t0);
        assert_eq!(gc.tick(t0), None);
    }

    #[test]
    fn profile_rumble_kind_matches_the_model() {
        for profile in all_profiles() {
            let expected = if profile.pid == NSO_GAMECUBE_PID {
                RumbleKind::PresetOnly
            } else {
                RumbleKind::Hd
            };
            assert_eq!(profile.rumble, expected, "{}", profile.name);
        }
    }
}
