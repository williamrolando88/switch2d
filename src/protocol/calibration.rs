//! Calibration decoders and mathematical normalization for Switch 2 joysticks and triggers.

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
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 9 {
            return None;
        }
        // If uncalibrated/all FF, return None so default is used
        if data[0..3] == [0xFF, 0xFF, 0xFF] {
            return None;
        }

        Some(Self {
            center: get_stick_xy(&data[0..3]),
            max: get_stick_xy(&data[3..6]),
            min: get_stick_xy(&data[6..9]),
        })
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

// GameCube analog trigger defaults
pub const GC_TRIGGER_DEFAULT_NEUTRAL: u8 = 30;
pub const GC_TRIGGER_DEADZONE: u8 = 35;
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
}
