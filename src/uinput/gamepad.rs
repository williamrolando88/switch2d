//! Virtual gamepad built on the raw uinput layer (`super::rawio`).

use std::collections::HashMap;
use std::io;
use std::sync::Mutex;

use evdev::{AbsoluteAxisType, BusType, EventType, Key, Synchronization};
use tracing::info;

use super::rawio::{AbsInfo, InputId, RawInputEvent, RawUinput};
use crate::protocol::constants::NINTENDO_VENDOR_ID;

pub const STICK_MIN: i32 = -32768;
pub const STICK_MAX: i32 = 32767;
pub const TRIGGER_MIN: i32 = 0;
pub const TRIGGER_MAX: i32 = 255;

/// Prefix for every node this daemon exposes. udev rules and the evdev `uniq`
/// shim key off it, so it must not be changed casually.
const PHYS_PREFIX: &str = "ngc/";

/// Every button the virtual pad exposes (also used by the selftest plan).
pub const KEYS: [Key; 15] = [
    // Face buttons (standard evdev diamond positions)
    Key::BTN_SOUTH,
    Key::BTN_EAST,
    Key::BTN_WEST,
    Key::BTN_NORTH,
    // Shoulder & triggers
    Key::BTN_TL,
    Key::BTN_TR,
    Key::BTN_TL2,
    Key::BTN_TR2,
    // System & menu
    Key::BTN_SELECT,
    Key::BTN_START,
    Key::BTN_MODE,
    Key::BTN_Z,
    Key::BTN_C,
    // Thumbstick clicks
    Key::BTN_THUMBL,
    Key::BTN_THUMBR,
];

const STICK_ABS: AbsInfo = AbsInfo::new(0, STICK_MIN, STICK_MAX, 16, 128, 0);
const TRIGGER_ABS: AbsInfo = AbsInfo::new(0, TRIGGER_MIN, TRIGGER_MAX, 0, 0, 0);
const HAT_ABS: AbsInfo = AbsInfo::new(0, -1, 1, 0, 0, 0);

const AXES: [(AbsoluteAxisType, AbsInfo); 8] = [
    (AbsoluteAxisType::ABS_X, STICK_ABS),
    (AbsoluteAxisType::ABS_Y, STICK_ABS),
    (AbsoluteAxisType::ABS_RX, STICK_ABS),
    (AbsoluteAxisType::ABS_RY, STICK_ABS),
    (AbsoluteAxisType::ABS_Z, TRIGGER_ABS),
    (AbsoluteAxisType::ABS_RZ, TRIGGER_ABS),
    (AbsoluteAxisType::ABS_HAT0X, HAT_ABS),
    (AbsoluteAxisType::ABS_HAT0Y, HAT_ABS),
];

pub struct VirtualGamepad {
    device: RawUinput,
    last_keys: Mutex<HashMap<u16, i32>>,
    last_abs: Mutex<HashMap<u16, i32>>,
    pub name: String,
}

impl VirtualGamepad {
    /// Create a new virtual gamepad exposed to the Linux kernel via uinput.
    ///
    /// `tag` is the identity string (typically the controller MAC, or
    /// `switch2-combined`). The node is published with `phys = "ngc/<tag>"`;
    /// a tag that already starts with `ngc/` is used verbatim.
    pub fn new(name: &str, product_id: u16, tag: &str) -> io::Result<Self> {
        let phys = if tag.starts_with(PHYS_PREFIX) {
            tag.to_string()
        } else {
            format!("{PHYS_PREFIX}{tag}")
        };

        // Order matters: event types, then per-key/axis capabilities, then the
        // device identity, then phys, and only then create.
        let device = RawUinput::open()?;
        device.set_evbit(EventType::KEY)?;
        device.set_evbit(EventType::ABSOLUTE)?;

        for key in KEYS {
            device.set_keybit(key)?;
        }
        for (axis, absinfo) in AXES {
            device.abs_setup(axis, absinfo)?;
        }

        let id = InputId {
            bustype: BusType::BUS_BLUETOOTH.0,
            vendor: NINTENDO_VENDOR_ID,
            product: product_id,
            version: 0x0100,
        };
        device.device_setup(&id, name, 0)?;
        device.set_phys(&phys)?;
        device.create()?;

        info!("Exposed virtual gamepad '{}' (phys: {})", name, phys);

        Ok(Self {
            device,
            last_keys: Mutex::new(HashMap::new()),
            last_abs: Mutex::new(HashMap::new()),
            name: name.to_string(),
        })
    }

    /// Emit button state if changed.
    pub fn set_key(&self, key: Key, pressed: bool) -> bool {
        let code = key.0;
        let value = if pressed { 1 } else { 0 };

        if !store_if_changed(&self.last_keys, code, value) {
            return false;
        }

        let _ = self
            .device
            .emit(&[RawInputEvent::new(EventType::KEY.0, code, value)]);
        true
    }

    /// Emit absolute axis state if changed.
    pub fn set_abs(&self, axis: AbsoluteAxisType, value: i32) -> bool {
        let code = axis.0;

        if !store_if_changed(&self.last_abs, code, value) {
            return false;
        }

        let _ = self
            .device
            .emit(&[RawInputEvent::new(EventType::ABSOLUTE.0, code, value)]);
        true
    }

    /// Flush a frame with a single SYN_REPORT.
    pub fn sync(&self) {
        let _ = self.device.emit(&[RawInputEvent::new(
            EventType::SYNCHRONIZATION.0,
            Synchronization::SYN_REPORT.0,
            0,
        )]);
    }
}

/// Locate a device's evdev node by name, polling up to 1 s.
///
/// uinput registration is synchronous in sysfs, but `/dev/input/eventN` appears
/// via devtmpfs a moment later, so callers must poll briefly. Shared by the
/// tests below and `switch2d selftest`.
///
/// (Kernel-visible naming: this is the helper the selftest uses to print the
/// node path and capabilities the kernel reports, not the values we requested.)
pub fn find_node(name: &str) -> Option<(std::path::PathBuf, evdev::Device)> {
    for _ in 0..40 {
        if let Some(found) = evdev::enumerate().find(|(_, device)| device.name() == Some(name)) {
            return Some(found);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    None
}

/// Record `value` for `code` and report whether it changed.
fn store_if_changed(state: &Mutex<HashMap<u16, i32>>, code: u16, value: i32) -> bool {
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if state.get(&code).copied() == Some(value) {
        return false;
    }
    state.insert(code, value);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/dev/uinput` access is environment-dependent (CI containers often lack
    /// it or deny access): node-creating tests skip instead of failing.
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

    fn find_device(name: &str) -> evdev::Device {
        find_node(name)
            .unwrap_or_else(|| panic!("device {name:?} did not appear in evdev within 1s"))
            .1
    }

    #[test]
    fn creates_node_with_phys_and_input_id() {
        if !uinput_writable() {
            return;
        }

        let name = format!("switch2d test {}", std::process::id());
        let product_id = 0x2069;

        let pad = VirtualGamepad::new(&name, product_id, "AA:BB:CC:DD:EE:FF")
            .expect("create virtual gamepad");

        let device = find_device(&name);
        assert_eq!(
            device.physical_path(),
            Some("ngc/AA:BB:CC:DD:EE:FF"),
            "phys must carry the udev contract"
        );

        let id = device.input_id();
        assert_eq!(id.vendor(), NINTENDO_VENDOR_ID);
        assert_eq!(id.product(), product_id);
        assert_eq!(id.bus_type(), BusType::BUS_BLUETOOTH);

        drop(device);
        drop(pad);

        // The node is destroyed on drop.
        assert!(
            evdev::enumerate().all(|(_, device)| device.name() != Some(name.as_str())),
            "uinput node should disappear when the pad is dropped"
        );
    }

    #[test]
    fn double_prefix_is_not_applied() {
        if !uinput_writable() {
            return;
        }

        let name = format!("switch2d test prefix {}", std::process::id());
        let pad = VirtualGamepad::new(&name, 0x2069, "ngc/X").expect("create virtual gamepad");

        let device = find_device(&name);
        assert_eq!(device.physical_path(), Some("ngc/X"));

        drop(device);
        drop(pad);
    }

    #[test]
    fn set_key_and_set_abs_only_report_changes() {
        // Pure diff state: runs with or without /dev/uinput.
        let state = Mutex::new(HashMap::new());
        assert!(store_if_changed(&state, 304, 1));
        assert!(!store_if_changed(&state, 304, 1));
        assert!(store_if_changed(&state, 304, 0));
        assert!(!store_if_changed(&state, 304, 0));
        assert!(store_if_changed(&state, 0, STICK_MIN));
        assert!(!store_if_changed(&state, 0, STICK_MIN));
        assert!(store_if_changed(&state, 0, STICK_MAX));

        let abs_state = Mutex::new(HashMap::new());
        assert!(store_if_changed(&abs_state, 0, -32768));
        assert!(!store_if_changed(&abs_state, 0, -32768));

        // Public API path, when the kernel node is available.
        if !uinput_writable() {
            return;
        }

        let name = format!("switch2d test diff {}", std::process::id());
        let pad = VirtualGamepad::new(&name, 0x2069, "AA:BB:CC:DD:EE:00")
            .expect("create virtual gamepad");

        assert!(pad.set_key(Key::BTN_SOUTH, true));
        assert!(!pad.set_key(Key::BTN_SOUTH, true));
        assert!(pad.set_key(Key::BTN_SOUTH, false));
        assert!(!pad.set_key(Key::BTN_SOUTH, false));

        assert!(pad.set_abs(AbsoluteAxisType::ABS_X, STICK_MIN));
        assert!(!pad.set_abs(AbsoluteAxisType::ABS_X, STICK_MIN));
        assert!(pad.set_abs(AbsoluteAxisType::ABS_X, 0));

        pad.sync();
        drop(pad);
    }
}
