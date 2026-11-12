//! Virtual gamepad built on the raw uinput layer (`super::rawio`).

use std::collections::HashMap;
use std::io;
use std::sync::Mutex;

use evdev::{AbsoluteAxisType, BusType, EventType, Key, Synchronization};
use tracing::info;

use super::rawio::{AbsInfo, InputId, RawInputEvent, RawUinput};
use crate::protocol::constants::{button_mask, NINTENDO_VENDOR_ID};

pub const STICK_MIN: i32 = -32768;
pub const STICK_MAX: i32 = 32767;
pub const TRIGGER_MIN: i32 = 0;
pub const TRIGGER_MAX: i32 = 255;

/// Prefix for every node this daemon exposes. udev rules and the evdev `uniq`
/// shim key off it, so it must not be changed casually.
const PHYS_PREFIX: &str = "ngc/";

/// Kernel `phys` string for a node tag (typically the controller MAC):
/// `ngc/<TAG>` with the tag uppercased. A leading `ngc/` prefix is stripped
/// first and re-applied, so the helper is idempotent.
///
/// Legacy `phys_for_mac()` is `f"ngc/{mac.upper()}"`
/// (`motion_evdev.py:44-45`). Shared by the gamepad and IMU nodes: the evdev
/// `uniq` shim links two nodes by matching this string, so both must be built
/// with this helper.
pub fn phys_for_tag(tag: &str) -> String {
    let tag = tag.strip_prefix(PHYS_PREFIX).unwrap_or(tag);
    format!("{PHYS_PREFIX}{}", tag.to_ascii_uppercase())
}

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

/// Destination for decoded pad state.
///
/// This is the seam that lets [`crate::worker::Worker`] be driven without
/// `/dev/uinput` (the daemon uses [`VirtualGamepad`], tests use a recorder).
pub trait PadSink {
    /// Apply one decoded report.
    ///
    /// `buttons` is the raw protocol mask, `sticks` is
    /// `(left.x, left.y, right.x, right.y)` in `-1.0..=1.0`, `triggers` is
    /// `(lt, rt)` in `0..=255`. Implementations diff against their own state.
    fn update(
        &mut self,
        buttons: u32,
        sticks: (f32, f32, f32, f32),
        triggers: (u8, u8),
    ) -> io::Result<()>;

    /// Neutralise every key and axis (teardown: the node stays alive).
    fn release_all(&mut self) -> io::Result<()>;

    /// Select the player slot (1..=8).
    fn set_player(&mut self, player: u8) -> io::Result<()>;
}

pub struct VirtualGamepad {
    device: RawUinput,
    last_keys: Mutex<HashMap<u16, i32>>,
    last_abs: Mutex<HashMap<u16, i32>>,
    player: u8,
    pub name: String,
}

impl VirtualGamepad {
    /// Create a new virtual gamepad exposed to the Linux kernel via uinput.
    ///
    /// `tag` is the identity string (typically the controller MAC, or
    /// `switch2-combined`) and is upper-cased: the node is published with
    /// `phys = "ngc/<TAG>"`, matching the legacy `phys_for_mac()` contract that
    /// udev rules and the uniq shim key on. A leading `ngc/` is stripped first,
    /// so passing a complete phys string is harmless.
    pub fn new(name: &str, product_id: u16, tag: &str) -> io::Result<Self> {
        let phys = phys_for_tag(tag);

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
            player: 0,
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

    /// Neutralise the pad: centre both sticks, zero both triggers and the
    /// hat, release every key. Only changed values are emitted (the same diff
    /// path as [`Self::set_abs`]/[`Self::set_key`]), then one SYN_REPORT.
    ///
    /// Used on teardown: the uinput node itself stays alive (analysis §5.9,
    /// deviation 6) so emulators/Steam do not re-enumerate and reorder pads.
    pub fn release_all(&mut self) {
        for axis in [
            AbsoluteAxisType::ABS_X,
            AbsoluteAxisType::ABS_Y,
            AbsoluteAxisType::ABS_RX,
            AbsoluteAxisType::ABS_RY,
            AbsoluteAxisType::ABS_Z,
            AbsoluteAxisType::ABS_RZ,
            AbsoluteAxisType::ABS_HAT0X,
            AbsoluteAxisType::ABS_HAT0Y,
        ] {
            self.set_abs(axis, 0);
        }
        for key in KEYS {
            self.set_key(key, false);
        }
        self.sync();
    }

    /// Record the player slot. Slots are clamped to `1..=8`.
    ///
    /// The uinput node exposes no LED capability (`rawio` never sets
    /// `EV_LED`), and the name is fixed at creation, so the kernel-visible
    /// player indicator is the name chosen by the caller; the matching
    /// controller-side LED pattern is the BLE `0x09/0x07` command that the
    /// daemon sends (task 09B). This keeps the worker's view of the slot in
    /// sync with that.
    pub fn set_player(&mut self, player: u8) -> io::Result<()> {
        self.player = player.clamp(1, 8);
        info!(
            "Virtual gamepad '{}' assigned to player {}",
            self.name, self.player
        );
        Ok(())
    }

    /// Player slot last selected via [`Self::set_player`] (`0` = unset).
    pub fn player(&self) -> u8 {
        self.player
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

impl PadSink for VirtualGamepad {
    fn update(
        &mut self,
        buttons: u32,
        sticks: (f32, f32, f32, f32),
        triggers: (u8, u8),
    ) -> io::Result<()> {
        let (lx, ly, rx, ry) = sticks;
        let (lt, rt) = triggers;

        // Sticks: `-1.0..=1.0` -> evdev i16 range, Y inverted because evdev
        // has "up" negative (`combined::update_full` uses the same convention).
        self.set_abs(AbsoluteAxisType::ABS_X, axis_from_unit(lx));
        self.set_abs(AbsoluteAxisType::ABS_Y, -axis_from_unit(ly));
        self.set_abs(AbsoluteAxisType::ABS_RX, axis_from_unit(rx));
        self.set_abs(AbsoluteAxisType::ABS_RY, -axis_from_unit(ry));

        // D-pad hat is derived from the button mask (the sink signature has no
        // separate hat field).
        let hat_x = i32::from(buttons & button_mask::RIGHT != 0)
            - i32::from(buttons & button_mask::LEFT != 0);
        let hat_y =
            i32::from(buttons & button_mask::DOWN != 0) - i32::from(buttons & button_mask::UP != 0);
        self.set_abs(AbsoluteAxisType::ABS_HAT0X, hat_x);
        self.set_abs(AbsoluteAxisType::ABS_HAT0Y, hat_y);

        // Triggers are already normalised to 0..=255.
        self.set_abs(AbsoluteAxisType::ABS_Z, i32::from(lt));
        self.set_abs(AbsoluteAxisType::ABS_RZ, i32::from(rt));

        let pressed = |mask: u32| buttons & mask != 0;
        self.set_key(Key::BTN_SOUTH, pressed(button_mask::A));
        self.set_key(Key::BTN_EAST, pressed(button_mask::B));
        self.set_key(Key::BTN_WEST, pressed(button_mask::X));
        self.set_key(Key::BTN_NORTH, pressed(button_mask::Y));
        self.set_key(Key::BTN_TL, pressed(button_mask::L));
        self.set_key(Key::BTN_TR, pressed(button_mask::R));
        self.set_key(Key::BTN_TL2, pressed(button_mask::ZL));
        self.set_key(Key::BTN_TR2, pressed(button_mask::ZR));
        self.set_key(Key::BTN_SELECT, pressed(button_mask::MINUS));
        self.set_key(Key::BTN_START, pressed(button_mask::PLUS));
        self.set_key(Key::BTN_MODE, pressed(button_mask::HOME));
        self.set_key(Key::BTN_Z, pressed(button_mask::CAPTURE));
        self.set_key(Key::BTN_C, pressed(button_mask::C_BTN));
        self.set_key(Key::BTN_THUMBL, pressed(button_mask::L_STICK_BTN));
        self.set_key(Key::BTN_THUMBR, pressed(button_mask::R_STICK_BTN));

        self.sync();
        Ok(())
    }

    fn release_all(&mut self) -> io::Result<()> {
        VirtualGamepad::release_all(self);
        Ok(())
    }

    fn set_player(&mut self, player: u8) -> io::Result<()> {
        VirtualGamepad::set_player(self, player)
    }
}

/// `-1.0..=1.0` -> `-32768..=32767`, clamped and NaN-safe (NaN -> 0).
fn axis_from_unit(value: f32) -> i32 {
    let scaled = value.clamp(-1.0, 1.0) * (STICK_MAX as f32);
    if scaled.is_nan() {
        0
    } else {
        scaled.round() as i32
    }
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
    fn release_all_centres_axes_and_releases_keys_on_real_node() {
        if !uinput_writable() {
            return;
        }

        let name = format!("switch2d test release_all {}", std::process::id());
        let mut pad = VirtualGamepad::new(&name, 0x2069, "test-release-all").unwrap();
        pad.set_key(Key::BTN_SOUTH, true);
        pad.set_abs(AbsoluteAxisType::ABS_X, 30000);
        pad.set_abs(AbsoluteAxisType::ABS_Z, 255);
        pad.sync();

        pad.release_all();

        let device = find_device(&name);
        let keys = device.get_key_state().unwrap();
        assert!(!keys.contains(Key::BTN_SOUTH), "keys released");
        let abs = device.get_abs_state().unwrap();
        assert_eq!(abs[AbsoluteAxisType::ABS_X.0 as usize].value, 0);
        assert_eq!(abs[AbsoluteAxisType::ABS_Z.0 as usize].value, 0);
        assert_eq!(abs[AbsoluteAxisType::ABS_RZ.0 as usize].value, 0);
        assert_eq!(abs[AbsoluteAxisType::ABS_HAT0X.0 as usize].value, 0);

        // Sink slot bookkeeping is clamped to the LED table's 1..=8.
        pad.set_player(3).unwrap();
        assert_eq!(pad.player(), 3);
        pad.set_player(0).unwrap();
        assert_eq!(pad.player(), 1);
        pad.set_player(99).unwrap();
        assert_eq!(pad.player(), 8);
    }

    #[test]
    fn pad_sink_update_maps_mask_sticks_and_triggers_on_real_node() {
        if !uinput_writable() {
            return;
        }

        let name = format!("switch2d test sink update {}", std::process::id());
        let mut pad = VirtualGamepad::new(&name, 0x2069, "test-sink-update").unwrap();
        PadSink::update(
            &mut pad,
            crate::protocol::constants::button_mask::A
                | crate::protocol::constants::button_mask::RIGHT
                | crate::protocol::constants::button_mask::UP,
            (0.5, -0.25, -0.75, 1.0),
            (12, 200),
        )
        .unwrap();

        let device = find_device(&name);
        let keys = device.get_key_state().unwrap();
        assert!(keys.contains(Key::BTN_SOUTH), "A maps to BTN_SOUTH");
        assert_eq!(keys.iter().count(), 1, "only A is held");

        let abs = device.get_abs_state().unwrap();
        assert_eq!(abs[AbsoluteAxisType::ABS_X.0 as usize].value, 16384);
        assert_eq!(abs[AbsoluteAxisType::ABS_Y.0 as usize].value, 8192);
        assert_eq!(abs[AbsoluteAxisType::ABS_RX.0 as usize].value, -24575);
        assert_eq!(abs[AbsoluteAxisType::ABS_RY.0 as usize].value, -32767);
        assert_eq!(abs[AbsoluteAxisType::ABS_Z.0 as usize].value, 12);
        assert_eq!(abs[AbsoluteAxisType::ABS_RZ.0 as usize].value, 200);
        assert_eq!(abs[AbsoluteAxisType::ABS_HAT0X.0 as usize].value, 1);
        assert_eq!(abs[AbsoluteAxisType::ABS_HAT0Y.0 as usize].value, -1);
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
