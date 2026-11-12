//! Companion evdev IMU node (accelerometer + gyroscope).
//!
//! Port of `.old/ngc/motion_evdev.py`: SDL3 links a gamepad to its motion
//! sensor only when both uinput nodes share the same `phys` string, because
//! uinput cannot set `uniq`. Both nodes therefore use
//! [`super::gamepad::phys_for_tag`] (`ngc/<MAC>`); the `uniq` shim falls back
//! to `phys` (`scripts/install-dusklight-gc.sh`,
//! `ngc_evdev_uniq_shim.c`).
//!
//! Values follow SDL's Linux joystick sensor scaling: the kernel axis carries
//! the raw IMU sample and `input_absinfo.resolution` declares the scale, so
//! SDL applies `value * g / resolution` (accel) or
//! `value * pi/180 / resolution` (gyro) — `motion_evdev.py:11-13`.

use std::io;

use evdev::{AbsoluteAxisType, BusType, EventType, PropType, Synchronization};
use tracing::info;

use super::gamepad::phys_for_tag;
use super::rawio::{AbsInfo, InputId, RawInputEvent, RawUinput};
use crate::protocol::constants::NINTENDO_VENDOR_ID;

/// Accelerometer LSB per g (`motion_evdev.py:29`).
pub const ACCEL_LSB_PER_G: i32 = 4096;
/// Gyroscope LSB per deg/s; the resolution field is 16.4 * 10
/// (`motion_evdev.py:30`).
pub const GYRO_LSB_PER_DPS: i32 = 164;

/// evdev absolute-axis range (`motion_evdev.py:32`).
pub const IMU_MIN: i32 = -32768;
pub const IMU_MAX: i32 = 32767;

/// The six axes in legacy order (`motion_evdev.py:34-41`), including the
/// `AbsInfo` resolution SDL reads.
pub const IMU_AXES: [(AbsoluteAxisType, AbsInfo); 6] = [
    (
        AbsoluteAxisType::ABS_X,
        AbsInfo::new(0, IMU_MIN, IMU_MAX, 0, 0, ACCEL_LSB_PER_G),
    ),
    (
        AbsoluteAxisType::ABS_Y,
        AbsInfo::new(0, IMU_MIN, IMU_MAX, 0, 0, ACCEL_LSB_PER_G),
    ),
    (
        AbsoluteAxisType::ABS_Z,
        AbsInfo::new(0, IMU_MIN, IMU_MAX, 0, 0, ACCEL_LSB_PER_G),
    ),
    (
        AbsoluteAxisType::ABS_RX,
        AbsInfo::new(0, IMU_MIN, IMU_MAX, 0, 0, GYRO_LSB_PER_DPS),
    ),
    (
        AbsoluteAxisType::ABS_RY,
        AbsInfo::new(0, IMU_MIN, IMU_MAX, 0, 0, GYRO_LSB_PER_DPS),
    ),
    (
        AbsoluteAxisType::ABS_RZ,
        AbsInfo::new(0, IMU_MIN, IMU_MAX, 0, 0, GYRO_LSB_PER_DPS),
    ),
];

/// Convert one raw IMU sample to the kernel axis value.
///
/// Mirrors `motion_evdev.py:83-90`: the axis carries the raw signed sample,
/// clamped to the evdev range; `lsb_per_unit` is published as
/// [`AbsInfo::resolution`] (declared at `motion_evdev.py:65-68`) and is what SDL
/// uses to convert to g / deg/s. So a raw ±4096 accelerometer sample becomes
/// ±4096 with resolution 4096, and a raw ±164 gyro sample becomes ±164 with
/// resolution 164.
pub fn axis_value(raw: i16, lsb_per_unit: f32) -> i32 {
    debug_assert!(
        lsb_per_unit > 0.0,
        "IMU resolution must be a positive LSB-per-unit value"
    );
    i32::from(raw).clamp(IMU_MIN, IMU_MAX)
}

/// Accelerometer raw triple -> `[ABS_X, ABS_Y, ABS_Z]` values.
pub fn accel_axes(accel_raw: (i16, i16, i16)) -> [i32; 3] {
    [
        axis_value(accel_raw.0, ACCEL_LSB_PER_G as f32),
        axis_value(accel_raw.1, ACCEL_LSB_PER_G as f32),
        axis_value(accel_raw.2, ACCEL_LSB_PER_G as f32),
    ]
}

/// Gyroscope raw triple -> `[ABS_RX, ABS_RY, ABS_RZ]` values.
pub fn gyro_axes(gyro_raw: (i16, i16, i16)) -> [i32; 3] {
    [
        axis_value(gyro_raw.0, GYRO_LSB_PER_DPS as f32),
        axis_value(gyro_raw.1, GYRO_LSB_PER_DPS as f32),
        axis_value(gyro_raw.2, GYRO_LSB_PER_DPS as f32),
    ]
}

/// Virtual accelerometer + gyroscope evdev node paired with a gamepad.
pub struct MotionNode {
    device: RawUinput,
    /// Last emitted value per [`IMU_AXES`] slot (diffed like the gamepad).
    last: [Option<i32>; 6],
    pub name: String,
}

impl MotionNode {
    /// Create the `<name> IMU` node sharing the gamepad's `phys`.
    ///
    /// `product_id` is the controller's gamepad product id
    /// (`bridge.py:556` passes the same `pid` to `MotionEvdev`, not the
    /// GameCube default); it is written to `UI_DEV_SETUP` verbatim.
    pub fn new(name: &str, mac: &str, product_id: u16) -> io::Result<Self> {
        let display_name = format!("{name} IMU");
        let phys = phys_for_tag(mac);

        // Same ordering as VirtualGamepad: event types, properties, per-axis
        // capabilities, identity, phys, then create.
        let device = RawUinput::open()?;
        device.set_evbit(EventType::ABSOLUTE)?;
        device.set_propbit(PropType::ACCELEROMETER.0)?;
        for (axis, absinfo) in IMU_AXES {
            device.abs_setup(axis, absinfo)?;
        }

        let id = InputId {
            bustype: BusType::BUS_BLUETOOTH.0,
            vendor: NINTENDO_VENDOR_ID,
            product: product_id,
            version: 0x0100,
        };
        device.device_setup(&id, &display_name, 0)?;
        device.set_phys(&phys)?;
        device.create()?;

        info!("Exposed motion evdev '{}' (phys: {})", display_name, phys);

        Ok(Self {
            device,
            last: [None; 6],
            name: display_name,
        })
    }

    /// Push one IMU frame. Only changed axes are emitted; a SYN_REPORT is
    /// always written, matching `motion_evdev.py:83-102`.
    pub fn update(
        &mut self,
        accel_raw: (i16, i16, i16),
        gyro_raw: (i16, i16, i16),
    ) -> io::Result<()> {
        let accel = accel_axes(accel_raw);
        let gyro = gyro_axes(gyro_raw);
        let values = [accel[0], accel[1], accel[2], gyro[0], gyro[1], gyro[2]];

        for (index, (axis, _absinfo)) in IMU_AXES.iter().enumerate() {
            let value = values[index];
            if self.last[index] == Some(value) {
                continue;
            }
            self.device
                .emit(&[RawInputEvent::new(EventType::ABSOLUTE.0, axis.0, value)])?;
            self.last[index] = Some(value);
        }

        self.device.emit(&[RawInputEvent::new(
            EventType::SYNCHRONIZATION.0,
            Synchronization::SYN_REPORT.0,
            0,
        )])?;
        Ok(())
    }
}

impl Drop for MotionNode {
    fn drop(&mut self) {
        // Best effort, same contract as RawUinput::drop (destroy is idempotent).
        let _ = self.device.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// `/dev/uinput` access is environment-dependent: node-creating tests skip
    /// instead of failing (same style as `uinput::gamepad`).
    fn uinput_writable() -> bool {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/uinput")
        {
            Ok(_) => true,
            Err(_) => {
                eprintln!("skipped: /dev/uinput not writable");
                false
            }
        }
    }

    #[test]
    fn axis_values_scale_like_legacy() {
        assert_eq!(ACCEL_LSB_PER_G, 4096);
        assert_eq!(GYRO_LSB_PER_DPS, 164);

        // Raw samples pass through with the legacy range clamp; the scale lives
        // in AbsInfo::resolution (motion_evdev.py:34-44).
        assert_eq!(axis_value(0, ACCEL_LSB_PER_G as f32), 0);
        assert_eq!(axis_value(4096, ACCEL_LSB_PER_G as f32), 4096);
        assert_eq!(axis_value(-4096, ACCEL_LSB_PER_G as f32), -4096);
        assert_eq!(axis_value(164, GYRO_LSB_PER_DPS as f32), 164);
        assert_eq!(axis_value(-164, GYRO_LSB_PER_DPS as f32), -164);
        assert_eq!(axis_value(i16::MIN, ACCEL_LSB_PER_G as f32), IMU_MIN);
        assert_eq!(axis_value(i16::MAX, ACCEL_LSB_PER_G as f32), IMU_MAX);

        assert_eq!(accel_axes((4096, -2048, 0)), [4096, -2048, 0]);
        assert_eq!(gyro_axes((164, -328, 0)), [164, -328, 0]);
    }

    #[test]
    fn imu_axes_cover_all_six_abs_codes() {
        let codes: Vec<u16> = IMU_AXES.iter().map(|(axis, _)| axis.0).collect();
        assert_eq!(
            codes,
            vec![
                AbsoluteAxisType::ABS_X.0,
                AbsoluteAxisType::ABS_Y.0,
                AbsoluteAxisType::ABS_Z.0,
                AbsoluteAxisType::ABS_RX.0,
                AbsoluteAxisType::ABS_RY.0,
                AbsoluteAxisType::ABS_RZ.0,
            ],
            "legacy axis order"
        );

        let unique: HashSet<u16> = codes.iter().copied().collect();
        assert_eq!(unique.len(), 6, "no duplicate ABS codes");

        // Accel axes use the g resolution; gyro axes use LSB-per-deg/s.
        for (axis, absinfo) in IMU_AXES {
            let expected = match axis {
                AbsoluteAxisType::ABS_X | AbsoluteAxisType::ABS_Y | AbsoluteAxisType::ABS_Z => {
                    ACCEL_LSB_PER_G
                }
                _ => GYRO_LSB_PER_DPS,
            };
            assert_eq!(absinfo.resolution, expected, "axis 0x{:02X}", axis.0);
            assert_eq!(absinfo.minimum, IMU_MIN);
            assert_eq!(absinfo.maximum, IMU_MAX);
            assert_eq!(absinfo.fuzz, 0);
            assert_eq!(absinfo.flat, 0);
        }
    }

    #[test]
    fn phys_string_matches_gamepad_contract() {
        // Shared with VirtualGamepad::new; must produce exactly ngc/<MAC> with
        // the MAC uppercased (motion_evdev.py:44-45) and stay idempotent for
        // callers that already pass the prefix.
        assert_eq!(phys_for_tag("aa:bb:cc:dd:ee:01"), "ngc/AA:BB:CC:DD:EE:01");
        assert_eq!(phys_for_tag("AA:BB:CC:DD:EE:FF"), "ngc/AA:BB:CC:DD:EE:FF");
        assert_eq!(
            phys_for_tag("ngc/AA:BB:CC:DD:EE:FF"),
            "ngc/AA:BB:CC:DD:EE:FF"
        );
        assert_eq!(
            phys_for_tag("ngc/aa:bb:cc:dd:ee:01"),
            "ngc/AA:BB:CC:DD:EE:01"
        );

        // Both VirtualGamepad::new and MotionNode::new call this helper with
        // the same MAC, so the nodes agree for the same input.
        let from_gamepad_tag = phys_for_tag("aa:bb:cc:dd:ee:01");
        let from_imu_mac = phys_for_tag("aa:bb:cc:dd:ee:01");
        assert_eq!(from_gamepad_tag, from_imu_mac);
    }

    fn sysfs_input_exists(name: &str) -> bool {
        let Ok(entries) = std::fs::read_dir("/sys/class/input") else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_input = path
                .file_name()
                .map(|file| file.to_string_lossy().starts_with("input"))
                .unwrap_or(false);
            if !is_input {
                continue;
            }
            if std::fs::read_to_string(path.join("name"))
                .map(|value| value.trim() == name)
                .unwrap_or(false)
            {
                return true;
            }
        }
        false
    }

    fn find_sysfs_input(name: &str) -> Option<std::path::PathBuf> {
        for _ in 0..40 {
            let Ok(entries) = std::fs::read_dir("/sys/class/input") else {
                return None;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let is_input = path
                    .file_name()
                    .map(|file| file.to_string_lossy().starts_with("input"))
                    .unwrap_or(false);
                if !is_input {
                    continue;
                }
                if std::fs::read_to_string(path.join("name"))
                    .map(|value| value.trim() == name)
                    .unwrap_or(false)
                {
                    return Some(path);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        None
    }

    #[test]
    fn creates_imu_node_with_six_axes_and_gamepad_phys() {
        if !uinput_writable() {
            return;
        }

        let pid = std::process::id();
        let name = format!("switch2d imu test {pid}");
        let display_name = format!("{name} IMU");
        let pad_name = format!("switch2d imu pad test {pid}");
        let mac = "aa:bb:cc:dd:ee:01";
        let product_id = 0x2066; // pretend Joy-Con 2 (Right)
        let expected_phys = "ngc/AA:BB:CC:DD:EE:01";

        // Companion gamepad: both nodes carry the same controller product id
        // and must normalise the same lowercase MAC to the same phys string.
        let gamepad = crate::uinput::gamepad::VirtualGamepad::new(&pad_name, product_id, mac)
            .expect("create companion gamepad");
        let node = MotionNode::new(&name, mac, product_id).expect("create virtual IMU node");

        // sysfs is world-readable, so creation and the phys contract can be
        // checked even when the event node is not yet readable by this user.
        let sysfs = find_sysfs_input(&display_name)
            .unwrap_or_else(|| panic!("{display_name:?} did not appear in sysfs within 1s"));
        let motion_phys =
            std::fs::read_to_string(sysfs.join("phys")).expect("sysfs phys is readable");
        assert_eq!(
            motion_phys.trim(),
            expected_phys,
            "phys must match the gamepad/shim contract"
        );

        let pad_sysfs = find_sysfs_input(&pad_name)
            .unwrap_or_else(|| panic!("{pad_name:?} did not appear in sysfs within 1s"));
        let pad_phys = std::fs::read_to_string(pad_sysfs.join("phys"))
            .expect("gamepad sysfs phys is readable");
        assert_eq!(
            pad_phys.trim(),
            motion_phys.trim(),
            "IMU and gamepad phys must agree for the same MAC"
        );

        let motion_device = crate::uinput::gamepad::find_node(&display_name).map(|found| found.1);
        let pad_device = crate::uinput::gamepad::find_node(&pad_name).map(|found| found.1);

        match (motion_device, pad_device) {
            (Some(device), Some(pad)) => {
                assert_eq!(
                    device.physical_path(),
                    pad.physical_path(),
                    "evdev phys must agree between IMU and gamepad"
                );
                assert_eq!(device.physical_path(), Some(expected_phys));

                let id = device.input_id();
                assert_eq!(id.vendor(), NINTENDO_VENDOR_ID);
                assert_eq!(id.product(), product_id);
                assert_eq!(id.bus_type(), BusType::BUS_BLUETOOTH);

                let supported = device
                    .supported_absolute_axes()
                    .expect("node advertises absolute axes");
                for (axis, _) in IMU_AXES {
                    assert!(supported.contains(axis), "missing axis 0x{:02X}", axis.0);
                }

                let state = device.get_abs_state().expect("read abs state");
                for (axis, expected) in IMU_AXES {
                    let info = &state[axis.0 as usize];
                    assert_eq!(info.minimum, expected.minimum, "axis 0x{:02X} min", axis.0);
                    assert_eq!(info.maximum, expected.maximum, "axis 0x{:02X} max", axis.0);
                    assert_eq!(
                        info.resolution, expected.resolution,
                        "axis 0x{:02X} resolution",
                        axis.0
                    );
                    assert_eq!(info.fuzz, expected.fuzz, "axis 0x{:02X} fuzz", axis.0);
                    assert_eq!(info.flat, expected.flat, "axis 0x{:02X} flat", axis.0);
                }
                drop(pad);
            }
            _ => {
                eprintln!(
                    "skipped evdev axis assertions: {display_name:?} is not readable \
                     (needs the ngc uaccess udev rule); sysfs name/phys verified"
                );
            }
        }
        drop(node);
        drop(gamepad);

        for _ in 0..40 {
            if !sysfs_input_exists(&display_name) && !sysfs_input_exists(&pad_name) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            !sysfs_input_exists(&display_name),
            "uinput IMU node should disappear when the MotionNode is dropped"
        );
        assert!(
            !sysfs_input_exists(&pad_name),
            "uinput gamepad node should disappear when dropped"
        );
    }
}
