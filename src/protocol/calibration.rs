//! Calibration decoders and mathematical normalization for Switch 2 joysticks and triggers.

use std::io;

use tracing::debug;

/// Uppercase hex preview capped to `limit` bytes for diagnostics.
fn hex_preview(data: &[u8], limit: usize) -> String {
    data.iter()
        .take(limit)
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Legacy `u24le(data[0:3]) == 0xFFFFFF` unwritten-area sentinel (`device.py:269`).
fn is_unwritten(data: &[u8]) -> bool {
    data.len() >= 3 && data[0..3] == [0xFF, 0xFF, 0xFF]
}

/// Decode 3 packed bytes into two 12-bit (0..4095) stick axis values (X, Y).
pub fn get_stick_xy(data: &[u8]) -> (u16, u16) {
    if data.len() < 3 {
        return (2048, 2048);
    }
    let value = u32::from_le_bytes([data[0], data[1], data[2], 0]);
    let x = (value & 0x0FFF) as u16;
    let y = ((value >> 12) & 0x0FFF) as u16;
    (x, y)
}

/// Factory and user calibration parameters for an analog stick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StickCalibration {
    pub center: (u16, u16),
    pub max: (u16, u16),
    pub min: (u16, u16),
}

impl Default for StickCalibration {
    fn default() -> Self {
        Self {
            center: (2048, 2048),
            max: (1700, 1700),
            min: (1700, 1700),
        }
    }
}

impl StickCalibration {
    /// Parse calibration data from 9 bytes of controller memory.
    ///
    /// Diagnostics only: logs the payload length, first eight bytes, the
    /// `FF FF FF` sentinel and the parsed centre/max/min at debug level. A
    /// zero-valued calibration is accepted (legacy semantics) but marked
    /// `dead_stick=true` because `apply()` then always returns 0.0.
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        match parse_calibration(data) {
            Ok(calibration) => {
                let dead_stick = calibration.max == (0, 0) || calibration.min == (0, 0);
                debug!(
                    "calib parse ok: len={} first8=[{}] center=({}, {}) max=({}, {}) \
                     min=({}, {}) dead_stick={}",
                    data.len(),
                    hex_preview(data, 8),
                    calibration.center.0,
                    calibration.center.1,
                    calibration.max.0,
                    calibration.max.1,
                    calibration.min.0,
                    calibration.min.1,
                    dead_stick
                );
                Some(calibration)
            }
            Err(reason) => {
                debug!(
                    "calib parse none: {} (len={} first8=[{}] ff_sentinel={})",
                    reason,
                    data.len(),
                    hex_preview(data, 8),
                    is_unwritten(data)
                );
                None
            }
        }
    }

    fn normalize_axis(raw: u16, center: u16, max_delta: u16, min_delta: u16, deadzone: i32) -> f32 {
        let signed = (raw as i32) - (center as i32);
        if signed > deadzone {
            let max_val = max_delta as f32;
            if max_val > 0.0 {
                ((signed as f32) / max_val).min(1.0)
            } else {
                0.0
            }
        } else if signed < -deadzone {
            let min_val = min_delta as f32;
            if min_val > 0.0 {
                -(((-signed) as f32) / min_val).min(1.0)
            } else {
                0.0
            }
        } else {
            0.0
        }
    }

    /// Normalizes raw stick reading to float values in the range [-1.0, 1.0].
    pub fn apply(&self, raw: (u16, u16), deadzone: i32) -> (f32, f32) {
        let x = Self::normalize_axis(raw.0, self.center.0, self.max.0, self.min.0, deadzone);
        let y = Self::normalize_axis(raw.1, self.center.1, self.max.1, self.min.1, deadzone);
        (x, y)
    }

    /// Normalizes raw stick reading directly to evdev signed 16-bit range [-32768, 32767].
    pub fn apply_scaled_i16(&self, raw: (u16, u16), deadzone: i32) -> (i32, i32) {
        let (fx, fy) = self.apply(raw, deadzone);
        let ix = (fx * 32767.0).clamp(-32768.0, 32767.0) as i32;
        let iy = (fy * 32767.0).clamp(-32768.0, 32767.0) as i32;
        (ix, iy)
    }
}

/// Pure parse used by [`StickCalibration::from_bytes`] and diagnostics.
fn parse_calibration(data: &[u8]) -> Result<StickCalibration, &'static str> {
    if data.len() < 9 {
        return Err("short data (<9 bytes)");
    }
    if is_unwritten(data) {
        // Unwritten user area: the controller wants the factory read.
        return Err("unwritten marker FF FF FF");
    }
    Ok(StickCalibration {
        center: get_stick_xy(&data[0..3]),
        max: get_stick_xy(&data[3..6]),
        min: get_stick_xy(&data[6..9]),
    })
}

/// Log one calibration memory read and its parse outcome (diagnostics only).
///
/// Intended to be called from the controller's per-stick calibration reader
/// once for the user area and once for the factory area. `result` is the
/// payload returned by `read_memory` (the 8-byte command header already
/// stripped); `requested_len` is the value passed to `read_memory`.
///
/// Emits a single debug line per read with the address, requested length,
/// returned length, first eight hex bytes, the `FF FF FF` sentinel, whether
/// the payload is long enough, and the parsed centre/max/min (or why parsing
/// failed). A failed read logs the exact error kind and whether it was a
/// timeout.
pub fn log_calibration_read(
    stick: &str,
    area: &str,
    address: u32,
    requested_len: u8,
    result: &io::Result<Vec<u8>>,
) {
    match result {
        Ok(data) => {
            let parsed = match parse_calibration(data) {
                Ok(calibration) => format!(
                    "center=({}, {}) max=({}, {}) min=({}, {}) dead_stick={}",
                    calibration.center.0,
                    calibration.center.1,
                    calibration.max.0,
                    calibration.max.1,
                    calibration.min.0,
                    calibration.min.1,
                    calibration.max == (0, 0) || calibration.min == (0, 0)
                ),
                Err(reason) => format!("none ({reason})"),
            };
            debug!(
                "calib read stick={} area={} addr=0x{:08X} req_len={} returned_len={} \
                 len_ok={} first8=[{}] ff_sentinel={} parse={}",
                stick,
                area,
                address,
                requested_len,
                data.len(),
                data.len() >= usize::from(requested_len),
                hex_preview(data, 8),
                is_unwritten(data),
                parsed
            );
        }
        Err(err) => {
            let kind = err.kind();
            debug!(
                "calib read stick={} area={} addr=0x{:08X} req_len={} FAILED kind={:?} \
                 timeout={} err={}",
                stick,
                area,
                address,
                requested_len,
                kind,
                matches!(kind, io::ErrorKind::TimedOut),
                err
            );
        }
    }
}

// GameCube analog trigger defaults
pub const GC_TRIGGER_DEFAULT_NEUTRAL: u8 = 30;pub const GC_TRIGGER_DEADZONE: u8 = 35;
pub const GC_TRIGGER_CLICK_THRESHOLD: u8 = 209;

/// Normalize a raw 0..255 GameCube trigger reading to 0..255 using neutral rest position.
pub fn normalize_gc_trigger(raw: u8, neutral: u8) -> u8 {
    if neutral >= 255 {
        return 0;
    }
    let neutral_u32 = neutral as u32;
    let raw_u32 = raw as u32;

    let val = if raw_u32 > neutral_u32 {
        ((raw_u32 - neutral_u32) * 255) / (255 - neutral_u32)
    } else {
        0
    };

    apply_trigger_deadzone(val as u8, GC_TRIGGER_DEADZONE)
}

/// Zero small trigger noise and snap to full scale past the click threshold.
pub fn apply_trigger_deadzone(value: u8, deadzone: u8) -> u8 {
    if value <= deadzone {
        return 0;
    }
    if value >= GC_TRIGGER_CLICK_THRESHOLD {
        return 255;
    }
    let span = (GC_TRIGGER_CLICK_THRESHOLD - deadzone) as u32;
    if span == 0 {
        return 255;
    }
    let scaled = ((value - deadzone) as u32 * 255) / span;
    scaled.min(255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stick_unpacking() {
        // x = 2048 (0x800), y = 2048 (0x800)
        // val = 0x800 | (0x800 << 12) = 0x800800 -> [0x00, 0x08, 0x80]
        let packed = [0x00, 0x08, 0x80];
        let (x, y) = get_stick_xy(&packed);
        assert_eq!(x, 2048);
        assert_eq!(y, 2048);
    }

    #[test]
    fn test_calibration_neutral() {
        let cal = StickCalibration::default();
        let (fx, fy) = cal.apply((2048, 2048), 50);
        assert_eq!(fx, 0.0);
        assert_eq!(fy, 0.0);
    }

    #[test]
    fn from_bytes_rejects_short_and_unwritten_samples() {
        assert_eq!(StickCalibration::from_bytes(&[0u8; 8]), None);
        assert_eq!(StickCalibration::from_bytes(&[0xFF; 11]), None);
        assert_eq!(StickCalibration::from_bytes(&[]), None);

        // Real values parse: centre 2048/2048, max 1700/1700, min 1700/1700.
        let parsed = StickCalibration::from_bytes(&[
            0x00, 0x08, 0x80, 0xA4, 0x46, 0x6A, 0xA4, 0x46, 0x6A, 0x00, 0x00,
        ])
        .expect("valid calibration parses");
        assert_eq!(parsed.center, (2048, 2048));
        assert_eq!(parsed.max, (1700, 1700));
        assert_eq!(parsed.min, (1700, 1700));
    }

    #[test]
    fn calibration_hex_preview_is_uppercase_and_capped() {
        let bytes: Vec<u8> = (0..16).collect();
        assert_eq!(
            hex_preview(&bytes, 8),
            "00 01 02 03 04 05 06 07"
        );
        assert_eq!(hex_preview(&[], 8), "");
    }

    #[test]
    fn log_calibration_read_covers_ok_and_error() {
        // No subscriber in tests: these just exercise both formatting branches.
        log_calibration_read(
            "right",
            "user",
            0x001F_C062,
            0x0B,
            &Ok(vec![0xFF; 11]),
        );
        log_calibration_read(
            "right",
            "factory",
            0x0001_30E8,
            0x0B,
            &Err(io::Error::new(io::ErrorKind::TimedOut, "no reply")),
        );
    }
}
