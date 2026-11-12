//! Switch 2 BLE controller protocol constants and definitions.

pub const NINTENDO_VENDOR_ID: u16 = 0x057E;
pub const NINTENDO_COMPANY_ID: u16 = 0x0553;

pub const JOYCON2_RIGHT_PID: u16 = 0x2066;
pub const JOYCON2_LEFT_PID: u16 = 0x2067;
pub const PRO_CONTROLLER2_PID: u16 = 0x2069;
pub const NSO_GAMECUBE_PID: u16 = 0x2073;

pub fn controller_name(pid: u16) -> &'static str {
    match pid {
        JOYCON2_RIGHT_PID => "Joy-Con 2 (Right)",
        JOYCON2_LEFT_PID => "Joy-Con 2 (Left)",
        PRO_CONTROLLER2_PID => "Pro Controller 2",
        NSO_GAMECUBE_PID => "NSO GameCube Controller",
        _ => "Unknown Switch 2 Controller",
    }
}

// GATT Characteristic UUIDs
pub const INPUT_REPORT_UUID: &str = "ab7de9be-89fe-49ad-828f-118f09df7fd2";
pub const COMMAND_WRITE_UUID: &str = "649d4ac9-8eb7-4e6c-af44-1ea54fe5f005";
pub const COMMAND_RESPONSE_UUID: &str = "c765a961-d9d8-4d36-a20a-5315b111836a";

pub const VIBRATION_PRO_CONTROLLER_UUID: &str = "cc483f51-9258-427d-a939-630c31f72b05";
pub const VIBRATION_JOYCON_R_UUID: &str = "fa19b0fb-cd1f-46a7-84a1-bbb09e00c149";
pub const VIBRATION_JOYCON_L_UUID: &str = "289326cb-a471-485d-a8f4-240c14f18241";

pub fn vibration_uuid_for(pid: u16) -> &'static str {
    match pid {
        JOYCON2_LEFT_PID => VIBRATION_JOYCON_L_UUID,
        JOYCON2_RIGHT_PID => VIBRATION_JOYCON_R_UUID,
        _ => VIBRATION_PRO_CONTROLLER_UUID,
    }
}

// Commands / Subcommands
pub const COMMAND_LEDS: u8 = 0x09;
pub const SUBCOMMAND_LEDS_SET_PLAYER: u8 = 0x07;

pub const COMMAND_MEMORY: u8 = 0x02;
pub const SUBCOMMAND_MEMORY_READ: u8 = 0x04;

pub const COMMAND_PAIR: u8 = 0x15;
pub const SUBCOMMAND_PAIR_SET_MAC: u8 = 0x01;
pub const SUBCOMMAND_PAIR_LTK1: u8 = 0x04;
pub const SUBCOMMAND_PAIR_LTK2: u8 = 0x02;
pub const SUBCOMMAND_PAIR_FINISH: u8 = 0x03;

pub const COMMAND_FEATURE: u8 = 0x0C;
pub const SUBCOMMAND_FEATURE_INIT: u8 = 0x02;
pub const SUBCOMMAND_FEATURE_ENABLE: u8 = 0x04;

pub const FEATURE_MOTION: u8 = 0x04;

pub const COMMAND_HOST: u8 = 0x01;
pub const SUBCOMMAND_HOST_HCI_STATE: u8 = 0x06;
pub const HCI_STATE_SLEEP: u8 = 0x00;

pub const ADDRESS_CONTROLLER_INFO: u32 = 0x00013000;
pub const CALIBRATION_JOYSTICK_1: u32 = 0x000130A8;
pub const CALIBRATION_JOYSTICK_2: u32 = 0x000130E8;
pub const CALIBRATION_USER_JOYSTICK_1: u32 = 0x001FC042;
pub const CALIBRATION_USER_JOYSTICK_2: u32 = 0x001FC062;
pub const CALIBRATION_GC_TRIGGERS: u32 = 0x00013140;

// Player LED patterns (1-8)
pub const LED_PATTERNS: [u8; 9] = [0x00, 0x01, 0x03, 0x07, 0x0F, 0x09, 0x05, 0x0D, 0x06];

// Fixed LTK halves required during bonding
pub const PAIR_LTK1: [u8; 17] = [
    0x00, 0xEA, 0xBD, 0x47, 0x13, 0x89, 0x35, 0x42, 0xC6, 0x79, 0xEE, 0x07, 0xF2, 0x53, 0x2C,
    0x6C, 0x31,
];
pub const PAIR_LTK2: [u8; 17] = [
    0x00, 0x40, 0xB0, 0x8A, 0x5F, 0xCD, 0x1F, 0x9B, 0x41, 0x12, 0x5C, 0xAC, 0xC6, 0x3F, 0x38,
    0xA0, 0x73,
];

// Button bitmasks (32-bit LE)
pub mod button_mask {
    pub const Y: u32 = 0x00000001;
    pub const X: u32 = 0x00000002;
    pub const B: u32 = 0x00000004;
    pub const A: u32 = 0x00000008;
    pub const SR_R: u32 = 0x00000010;
    pub const SL_R: u32 = 0x00000020;
    pub const R: u32 = 0x00000040;
    pub const ZR: u32 = 0x00000080;
    pub const MINUS: u32 = 0x00000100;
    pub const PLUS: u32 = 0x00000200;
    pub const R_STICK_BTN: u32 = 0x00000400;
    pub const L_STICK_BTN: u32 = 0x00000800;
    pub const HOME: u32 = 0x00001000;
    pub const CAPTURE: u32 = 0x00002000;
    pub const C_BTN: u32 = 0x00004000;
    pub const DOWN: u32 = 0x00010000;
    pub const UP: u32 = 0x00020000;
    pub const RIGHT: u32 = 0x00040000;
    pub const LEFT: u32 = 0x00080000;
    pub const SR_L: u32 = 0x00100000;
    pub const SL_L: u32 = 0x00200000;
    pub const L: u32 = 0x00400000;
    pub const ZL: u32 = 0x00800000;
    pub const GR: u32 = 0x01000000;
    pub const GL: u32 = 0x02000000;
}

/// Frame a controller command according to the Switch 2 0x91 protocol.
pub fn build_command(command_id: u8, subcommand_id: u8, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(7 + data.len());
    buf.push(command_id);
    buf.extend_from_slice(&[0x91, 0x01]);
    buf.push(subcommand_id);
    buf.push(0x00);
    buf.push(data.len() as u8);
    buf.extend_from_slice(&[0x00, 0x00]);
    buf.extend_from_slice(data);
    buf
}
