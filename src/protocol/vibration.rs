//! HD vibration encoding (pure: no I/O, no timers).
//!
//! Wire format (analysis §8.1–8.3, architecture §5.2): a 40-bit little-endian
//! sample, packed three times after a rolling header byte. Pro Controller 2
//! duplicates the block (left/right actuator), everything else sends one.
//!
//! No protocol command is issued here: `play_vibration_preset` (0x0A/0x02) is
//! a separate feature.

use std::time::{Duration, Instant};

/// Sustained HD-rumble packet interval (~60 Hz).
pub const SUSTAIN_INTERVAL: Duration = Duration::from_millis(16);

/// Below/at this magnitude the target counts as stopped (legacy 0.001).
pub const STOP_THRESHOLD: f32 = 0.001;

/// One 40-bit HD-rumble sample.
///
/// Field widths: `lf_freq`/`hf_freq` 9 bits, amplitudes 10 bits. Values wider
/// than that are masked by [`Sample::to_u40`], never trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub lf_freq: u16,
    pub lf_en_tone: bool,
    pub lf_amp: u16,
    pub hf_freq: u16,
    pub hf_en_tone: bool,
    pub hf_amp: u16,
}

impl Default for Sample {
    fn default() -> Self {
        Self {
            // Neutral sample `e1 00 10 1e 00`: silent, frequencies preserved.
            lf_freq: 0x0E1,
            lf_en_tone: false,
            lf_amp: 0,
            hf_freq: 0x1E1,
            hf_en_tone: false,
            hf_amp: 0,
        }
    }
}

impl Sample {
    /// Neutral frequencies, given amplitudes.
    pub fn with_amplitudes(lf_amp: u16, hf_amp: u16) -> Self {
        Self {
            lf_amp,
            hf_amp,
            ..Self::default()
        }
    }

    /// Pack to the 5-byte (40-bit LE) wire form.
    ///
    /// ```text
    /// bits  0-8   lf_freq (0x1FF)
    /// bit      9  lf_en_tone
    /// bits 10-19  lf_amp  (0x3FF)
    /// bits 20-28  hf_freq (0x1FF)
    /// bit     29  hf_en_tone
    /// bits 30-39  hf_amp  (0x3FF)
    /// ```
    pub fn to_u40(self) -> [u8; 5] {
        let lf_freq = u64::from(self.lf_freq & 0x01FF);
        let lf_tone = u64::from(self.lf_en_tone as u8) << 9;
        let lf_amp = u64::from(self.lf_amp & 0x03FF) << 10;
        let hf_freq = u64::from(self.hf_freq & 0x01FF) << 20;
        let hf_tone = u64::from(self.hf_en_tone as u8) << 29;
        let hf_amp = u64::from(self.hf_amp & 0x03FF) << 30;

        let packed = lf_freq | lf_tone | lf_amp | hf_freq | hf_tone | hf_amp;
        let bytes = packed.to_le_bytes();
        [bytes[0], bytes[1], bytes[2], bytes[3], bytes[4]]
    }
}

/// Rolling header byte source: `0x50 + (seq & 0x0F)`, wrapping `0x50..=0x5F`.
#[derive(Debug, Clone)]
pub struct Sequencer {
    next: u8,
}

impl Default for Sequencer {
    fn default() -> Self {
        Self::new()
    }
}

impl Sequencer {
    pub fn new() -> Self {
        Self { next: 0 }
    }

    pub fn next_header(&mut self) -> u8 {
        let header = 0x50 + (self.next & 0x0F);
        self.next = self.next.wrapping_add(1);
        header
    }
}

/// One 16-byte block: header byte followed by the sample three times.
pub fn block(header: u8, sample: &Sample) -> [u8; 16] {
    let sample = sample.to_u40();
    let mut out = [0u8; 16];
    out[0] = header;
    for chunk in out[1..].chunks_exact_mut(5) {
        chunk.copy_from_slice(&sample);
    }
    out
}

/// Number of actuator blocks a controller expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    SingleBlock,
    DualBlock,
}

/// Pro Controller 2 drives two actuators; every other PID uses one block.
pub fn layout_for(pid: u16) -> Layout {
    if pid == crate::protocol::constants::PRO_CONTROLLER2_PID {
        Layout::DualBlock
    } else {
        Layout::SingleBlock
    }
}

/// Build a vibration payload: `0x00 | block` (17 B) or
/// `0x00 | blockL | blockR` (33 B, byte-identical blocks sharing the header).
pub fn payload(layout: Layout, header: u8, sample: &Sample) -> Vec<u8> {
    let block = block(header, sample);
    let block_count = match layout {
        Layout::SingleBlock => 1,
        Layout::DualBlock => 2,
    };

    let mut out = Vec::with_capacity(1 + block_count * block.len());
    out.push(0x00);
    for _ in 0..block_count {
        out.extend_from_slice(&block);
    }
    out
}

/// Stop packet: amplitudes 0, frequencies preserved (`e1 00 10 1e 00`).
pub fn stop_payload(layout: Layout, header: u8) -> Vec<u8> {
    payload(layout, header, &Sample::default())
}

/// Mix `strong + 0.5 * weak` into the 10-bit LF amplitude.
///
/// Clamped to `0..=1.0` before scaling; NaN maps to 0 and infinities clamp
/// like any other out-of-range value.
pub fn amplitude(strong: f32, weak: f32) -> u16 {
    let mixed = strong + 0.5 * weak;
    // `f32::clamp` propagates NaN, and `NaN as u16` is 0 in Rust, so this is
    // NaN-safe without an extra branch.
    (mixed.clamp(0.0, 1.0) * f32::from(0x03FFu16)) as u16
}

/// Is this target quiet enough to stop? (Both channels at/under 0.001.)
pub fn is_stop(strong: f32, weak: f32) -> bool {
    strong <= STOP_THRESHOLD && weak <= STOP_THRESHOLD
}

/// Sustain/stop state machine for one controller. No threads, no sleeping:
/// the caller owns the clock and calls [`Sustainer::tick`].
#[derive(Debug, Clone)]
pub struct Sustainer {
    layout: Layout,
    sequencer: Sequencer,
    target: (f32, f32),
    /// "Sent something since the last stop" latch (legacy `active`).
    active: bool,
    /// Timestamp of the most recent emitted packet.
    last_sent: Option<Instant>,
}

impl Sustainer {
    pub fn new(pid: u16) -> Self {
        Self {
            layout: layout_for(pid),
            sequencer: Sequencer::new(),
            target: (0.0, 0.0),
            active: false,
            last_sent: None,
        }
    }

    /// Set the desired `(strong, weak)` magnitude.
    ///
    /// Raising the target after a stop clears the interval gate so the next
    /// tick emits immediately instead of waiting for [`SUSTAIN_INTERVAL`].
    pub fn set_target(&mut self, strong: f32, weak: f32) {
        self.target = (strong, weak);
        if !is_stop(strong, weak) && !self.active {
            self.last_sent = None;
        }
    }

    /// Emit at most one packet if one is due.
    ///
    /// Active target: one payload per elapsed [`SUSTAIN_INTERVAL`], never a
    /// burst. Released target: exactly one stop packet, then `None` until the
    /// target rises again.
    pub fn tick(&mut self, now: Instant) -> Option<Vec<u8>> {
        let (strong, weak) = self.target;

        if is_stop(strong, weak) {
            if !self.active {
                return None;
            }

            let header = self.sequencer.next_header();
            self.active = false;
            self.last_sent = Some(now);
            return Some(stop_payload(self.layout, header));
        }

        if let Some(last) = self.last_sent {
            // A monotonic clock never goes backwards; `saturating_duration_since`
            // keeps a test-supplied/odd clock from stalling the loop.
            if now.saturating_duration_since(last) < SUSTAIN_INTERVAL {
                return None;
            }
        }

        let header = self.sequencer.next_header();
        let sample = Sample::with_amplitudes(amplitude(strong, weak), 0);
        self.active = true;
        self.last_sent = Some(now);
        Some(payload(self.layout, header, &sample))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::constants::{JOYCON2_LEFT_PID, PRO_CONTROLLER2_PID};

    fn sample_bytes(payload: &[u8]) -> &[u8] {
        &payload[2..7]
    }

    #[test]
    fn u40_packing_default_sample() {
        assert_eq!(Sample::default().to_u40(), [0xE1, 0x00, 0x10, 0x1E, 0x00]);
        assert_eq!(
            Sample::with_amplitudes(0, 0).to_u40(),
            [0xE1, 0x00, 0x10, 0x1E, 0x00]
        );
    }

    #[test]
    fn u40_packing_full_lf_amplitude() {
        assert_eq!(
            Sample::with_amplitudes(0x03FF, 0).to_u40(),
            [0xE1, 0xFC, 0x1F, 0x1E, 0x00]
        );
        assert_eq!(
            Sample::with_amplitudes(0x03FF, 0x03FF).to_u40(),
            [0xE1, 0xFC, 0x1F, 0xDE, 0xFF]
        );
    }

    #[test]
    fn u40_masks_fields_to_their_widths() {
        let sample = Sample {
            lf_freq: 0xFFFF,
            lf_en_tone: true,
            lf_amp: 0xFFFF,
            hf_freq: 0x1E1,
            hf_en_tone: false,
            hf_amp: 0x0400, // bit 10 set: outside the 10-bit amplitude
        };
        assert_eq!(sample.to_u40(), [0xFF, 0xFF, 0x1F, 0x1E, 0x00]);

        // Tone bits land on bit 9 (LF) and bit 29 (HF) only.
        let lf_tone = Sample {
            lf_en_tone: true,
            ..Sample::default()
        };
        assert_eq!(lf_tone.to_u40(), [0xE1, 0x02, 0x10, 0x1E, 0x00]);
        let hf_tone = Sample {
            hf_en_tone: true,
            ..Sample::default()
        };
        assert_eq!(hf_tone.to_u40(), [0xE1, 0x00, 0x10, 0x3E, 0x00]);
    }

    #[test]
    fn header_wraps_0x50_to_0x5f() {
        let mut seq = Sequencer::new();
        let headers: Vec<u8> = (0..18).map(|_| seq.next_header()).collect();

        let expected: Vec<u8> = (0..18).map(|i| 0x50 + (i % 16)).collect();
        assert_eq!(headers, expected);
        assert_eq!(headers[..16], (0x50..=0x5F).collect::<Vec<u8>>()[..]);
        assert_eq!(headers[16], 0x50);
        assert_eq!(headers[17], 0x51);
    }

    #[test]
    fn pro_payload_is_33_bytes_with_identical_blocks() {
        let sample = Sample::with_amplitudes(0x0123, 0);
        let payload = payload(Layout::DualBlock, 0x5A, &sample);

        assert_eq!(payload.len(), 33);
        assert_eq!(payload[0], 0x00);
        assert_eq!(payload[1], 0x5A);
        assert_eq!(payload[1..17], payload[17..33]);
        assert_eq!(
            layout_for(PRO_CONTROLLER2_PID),
            Layout::DualBlock,
            "Pro Controller 2 drives both actuators"
        );
    }

    #[test]
    fn single_block_payload_is_17_bytes() {
        let sample = Sample::with_amplitudes(0x0123, 0);
        let payload = payload(Layout::SingleBlock, 0x53, &sample);

        assert_eq!(payload.len(), 17);
        assert_eq!(payload[0], 0x00);
        assert_eq!(payload[1], 0x53);
        assert_eq!(&payload[1..], &block(0x53, &sample)[..]);
        assert_eq!(layout_for(JOYCON2_LEFT_PID), Layout::SingleBlock);
    }

    #[test]
    fn stop_payload_keeps_frequencies_and_zeroes_amplitudes() {
        let stop = stop_payload(Layout::SingleBlock, 0x57);
        assert_eq!(stop.len(), 17);
        assert_eq!(stop[0], 0x00);
        assert_eq!(stop[1], 0x57);
        assert_eq!(sample_bytes(&stop), [0xE1, 0x00, 0x10, 0x1E, 0x00]);
        // Byte-identical to a payload built from the default (silent) sample.
        assert_eq!(stop, payload(Layout::SingleBlock, 0x57, &Sample::default()));

        let dual = stop_payload(Layout::DualBlock, 0x57);
        assert_eq!(dual.len(), 33);
        assert_eq!(sample_bytes(&dual), [0xE1, 0x00, 0x10, 0x1E, 0x00]);
        assert_eq!(dual[1..17], dual[17..33]);
    }

    #[test]
    fn amplitude_formula_clamps_and_mixes() {
        assert_eq!(amplitude(0.0, 0.0), 0);
        assert_eq!(amplitude(0.5, 0.0), 0x1FF);
        assert_eq!(amplitude(0.0, 0.5), 0xFF); // 0.25 * 0x3FF = 255.75
        assert_eq!(amplitude(0.5, 1.0), 0x3FF); // 0.5 + 0.5 = 1.0
        assert_eq!(amplitude(1.0, 0.0), 0x3FF);
        assert_eq!(amplitude(2.0, 0.0), 0x3FF, "clamped high");
        assert_eq!(amplitude(-1.0, 4.0), 0x3FF, "mix saturates above 1.0");
        assert_eq!(amplitude(-1.0, 1.0), 0, "negative mix clamps to 0");
        assert_eq!(amplitude(-5.0, 0.0), 0, "clamped low");
        assert_eq!(amplitude(f32::NAN, 0.0), 0, "NaN is safe");
        assert_eq!(amplitude(0.0, f32::NAN), 0, "NaN is safe in weak");

        assert!(is_stop(0.0, 0.0));
        assert!(is_stop(0.001, 0.001), "threshold is inclusive");
        assert!(!is_stop(0.0011, 0.0));
        assert!(!is_stop(0.0, 0.0011));
    }

    #[test]
    fn sustainer_emits_every_16ms() {
        let mut sustainer = Sustainer::new(JOYCON2_LEFT_PID);
        let t0 = Instant::now();

        sustainer.set_target(0.5, 0.0);
        let first = sustainer.tick(t0).expect("first packet is immediate");
        assert_eq!(first.len(), 17);
        assert_eq!(first[1], 0x50);

        assert!(sustainer.tick(t0 + Duration::from_millis(15)).is_none());
        let second = sustainer
            .tick(t0 + Duration::from_millis(16))
            .expect("due after 16 ms");
        assert_eq!(second[1], 0x51, "header advances per packet");
        assert!(sustainer
            .tick(t0 + Duration::from_millis(32))
            .is_some_and(|p| p[1] == 0x52));

        // A long gap still produces a single packet; the following tick at the
        // same instant produces nothing (no catch-up burst).
        assert!(sustainer.tick(t0 + Duration::from_secs(10)).is_some());
        assert!(sustainer.tick(t0 + Duration::from_secs(10)).is_none());
    }

    #[test]
    fn sustainer_emits_exactly_one_stop_packet() {
        let mut sustainer = Sustainer::new(PRO_CONTROLLER2_PID);
        let t0 = Instant::now();

        sustainer.set_target(0.8, 0.4);
        assert!(sustainer.tick(t0).is_some());

        sustainer.set_target(0.0, 0.0);
        let stop = sustainer
            .tick(t0 + Duration::from_millis(16))
            .expect("release emits a stop packet");
        assert_eq!(stop.len(), 33);
        assert_eq!(stop[1], 0x51);
        assert_eq!(sample_bytes(&stop), [0xE1, 0x00, 0x10, 0x1E, 0x00]);
        assert_eq!(stop, stop_payload(Layout::DualBlock, 0x51));

        assert!(sustainer.tick(t0 + Duration::from_millis(32)).is_none());
        assert!(sustainer.tick(t0 + Duration::from_secs(5)).is_none());
    }

    #[test]
    fn sustainer_resumes_immediately_after_parking() {
        let mut sustainer = Sustainer::new(JOYCON2_LEFT_PID);
        let t0 = Instant::now();

        sustainer.set_target(0.5, 0.0);
        assert!(sustainer.tick(t0).is_some());

        sustainer.set_target(0.0, 0.0);
        let stop = sustainer
            .tick(t0 + Duration::from_millis(16))
            .expect("stop packet");
        assert_eq!(sample_bytes(&stop), [0xE1, 0x00, 0x10, 0x1E, 0x00]);
        assert!(sustainer.tick(t0 + Duration::from_millis(17)).is_none());

        // Raise the target again: the next tick must emit right away, even at
        // the same instant as the stop packet.
        sustainer.set_target(0.5, 0.0);
        let resumed = sustainer
            .tick(t0 + Duration::from_millis(16))
            .expect("resume is immediate, not interval-gated");
        assert_eq!(resumed.len(), 17);
        assert_ne!(
            sample_bytes(&resumed),
            [0xE1, 0x00, 0x10, 0x1E, 0x00],
            "resumed packet carries the new amplitude"
        );
        assert!(sustainer.tick(t0 + Duration::from_millis(16)).is_none());
    }
}
