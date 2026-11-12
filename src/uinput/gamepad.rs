//! Virtual gamepad implementation using Linux `/dev/uinput` and `evdev`.

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{
    AbsInfo, AbsoluteAxisType, AttributeSet, BusType, EventType, InputEvent, InputId, Key,
    Synchronization, UinputAbsSetup,
};
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::info;

use crate::protocol::constants::NINTENDO_VENDOR_ID;

pub const STICK_MIN: i32 = -32768;
pub const STICK_MAX: i32 = 32767;
pub const TRIGGER_MIN: i32 = 0;
pub const TRIGGER_MAX: i32 = 255;

pub struct VirtualGamepad {
    device: Mutex<VirtualDevice>,
    last_keys: Mutex<HashMap<u16, i32>>,
    last_abs: Mutex<HashMap<u16, i32>>,
    pub name: String,
}

impl VirtualGamepad {
    /// Create a new virtual gamepad exposed to the Linux kernel via uinput.
    pub fn new(name: &str, product_id: u16, phys: &str) -> std::io::Result<Self> {
        let mut keys = AttributeSet::<Key>::new();

        // Face buttons (standard evdev diamond positions)
        keys.insert(Key::BTN_SOUTH); // A
        keys.insert(Key::BTN_EAST);  // B
        keys.insert(Key::BTN_WEST);  // X
        keys.insert(Key::BTN_NORTH); // Y

        // Shoulder & triggers
        keys.insert(Key::BTN_TL);    // L / Left Bumper
        keys.insert(Key::BTN_TR);    // R / Right Bumper
        keys.insert(Key::BTN_TL2);   // ZL
        keys.insert(Key::BTN_TR2);   // ZR

        // System & Menu
        keys.insert(Key::BTN_SELECT); // Minus
        keys.insert(Key::BTN_START);  // Plus
        keys.insert(Key::BTN_MODE);   // Home
        keys.insert(Key::BTN_Z);      // Capture / Screenshot
        keys.insert(Key::BTN_C);      // C Button

        // Thumbstick clicks
        keys.insert(Key::BTN_THUMBL); // Left Stick Click
        keys.insert(Key::BTN_THUMBR); // Right Stick Click

        // Analog axis definitions
        let stick_abs = AbsInfo::new(0, STICK_MIN, STICK_MAX, 16, 128, 0);
        let trigger_abs = AbsInfo::new(0, TRIGGER_MIN, TRIGGER_MAX, 0, 0, 0);
        let dpad_abs = AbsInfo::new(0, -1, 1, 0, 0, 0);

        let axis_setups = [
            UinputAbsSetup::new(AbsoluteAxisType::ABS_X, stick_abs),
            UinputAbsSetup::new(AbsoluteAxisType::ABS_Y, stick_abs),
            UinputAbsSetup::new(AbsoluteAxisType::ABS_RX, stick_abs),
            UinputAbsSetup::new(AbsoluteAxisType::ABS_RY, stick_abs),
            UinputAbsSetup::new(AbsoluteAxisType::ABS_Z, trigger_abs),
            UinputAbsSetup::new(AbsoluteAxisType::ABS_RZ, trigger_abs),
            UinputAbsSetup::new(AbsoluteAxisType::ABS_HAT0X, dpad_abs),
            UinputAbsSetup::new(AbsoluteAxisType::ABS_HAT0Y, dpad_abs),
        ];

        let mut builder = VirtualDeviceBuilder::new()?
            .name(name)
            .input_id(InputId::new(
                BusType::BUS_BLUETOOTH,
                NINTENDO_VENDOR_ID,
                product_id,
                0x0100,
            ))
            .with_keys(&keys)?;

        for axis in &axis_setups {
            builder = builder.with_absolute_axis(axis)?;
        }

        let device = builder.build()?;
        info!("Exposed virtual gamepad '{}' (phys: {})", name, phys);

        Ok(Self {
            device: Mutex::new(device),
            last_keys: Mutex::new(HashMap::new()),
            last_abs: Mutex::new(HashMap::new()),
            name: name.to_string(),
        })
    }

    /// Emit button state if changed.
    pub fn set_key(&self, key: Key, pressed: bool) -> bool {
        let code = key.0;
        let val = if pressed { 1 } else { 0 };
        let mut last = self.last_keys.lock().unwrap();
        if last.get(&code).copied() != Some(val) {
            last.insert(code, val);
            let mut dev = self.device.lock().unwrap();
            let ev = InputEvent::new(EventType::KEY, code, val);
            let _ = dev.emit(&[ev]);
            true
        } else {
            false
        }
    }

    /// Emit absolute axis state if changed.
    pub fn set_abs(&self, axis: AbsoluteAxisType, value: i32) -> bool {
        let code = axis.0;
        let mut last = self.last_abs.lock().unwrap();
        if last.get(&code).copied() != Some(value) {
            last.insert(code, value);
            let mut dev = self.device.lock().unwrap();
            let ev = InputEvent::new(EventType::ABSOLUTE, code, value);
            let _ = dev.emit(&[ev]);
            true
        } else {
            false
        }
    }

    /// Flush pending events with SYN_REPORT.
    pub fn sync(&self) {
        let mut dev = self.device.lock().unwrap();
        let ev = InputEvent::new(EventType::SYNCHRONIZATION, Synchronization::SYN_REPORT.0, 0);
        let _ = dev.emit(&[ev]);
    }
}
