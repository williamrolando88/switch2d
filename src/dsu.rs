//! DSU (cemuhook) UDP motion server.
//!
//! Rust port of the legacy Python server in `.old/ngc/dsu.py` (cemuhook protocol
//! version 1001, four slots, default `127.0.0.1:26760`). The wire format is
//! ported byte-for-byte; legacy line references are called out throughout:
//!
//! * protocol version / message ids: `dsu.py:29-34`
//! * slot state / model / connection ids: `dsu.py:37-40`
//! * IMU scaling and axis signs: `dsu.py:45-51`
//! * battery enum: `dsu.py:56-66`
//! * DSUS packet framing + CRC: `dsu.py:183-190`
//! * shared controller header: `dsu.py:192-197`
//! * controller info payload: `dsu.py:199-200`
//! * pad data payload: `dsu.py:202-270`
//! * request dispatch / subscriptions: `dsu.py:292-333`
//!
//! The server is std-only: a single blocking [`DsuServer::run`] loop owns the
//! socket reads while input producers call [`DsuServer::update`] from their own
//! threads. Everything shared is behind `Mutex`es; there is no `unsafe` and no
//! async runtime.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::protocol::constants::button_mask;

/// cemuhook protocol version (`dsu.py:29`).
pub const PROTOCOL_VERSION: u16 = 1001;

/// Request/reply message types shared by client and server (`dsu.py:32-34`).
pub const MSG_VERSION: u32 = 0x0010_0000;
pub const MSG_PORTS: u32 = 0x0010_0001;
pub const MSG_DATA: u32 = 0x0010_0002;

/// Slot state / model / connection constants (`dsu.py:37-40`).
pub const STATE_DISCONNECTED: u8 = 0;
pub const STATE_CONNECTED: u8 = 2;
pub const MODEL_FULL_GYRO: u8 = 2;
pub const CONN_BLUETOOTH: u8 = 2;

/// IMU scaling from the legacy defaults (±8 g, ±2000 deg/s) (`dsu.py:45-46`).
pub const ACCEL_G_PER_LSB: f32 = 1.0 / 4096.0;
pub const GYRO_DPS_PER_LSB: f32 = 1.0 / 16.4;

/// Raw IMU axis -> DSU axis signs (`dsu.py:50-51`).
pub const ACCEL_SIGN: [f32; 3] = [1.0, 1.0, 1.0];
pub const GYRO_SIGN: [f32; 3] = [1.0, 1.0, 1.0];

/// Drop clients that stop requesting data (`dsu.py:53`).
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Socket read timeout used by [`DsuServer::run`] to poll the stop flag.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Magic bytes of a client request / server reply (`dsu.py:185`, `dsu.py:297`).
pub const DSU_SERVER_MAGIC: [u8; 4] = *b"DSUS";
pub const DSU_CLIENT_MAGIC: [u8; 4] = *b"DSUC";

/// Number of DSU motion slots (`dsu.py:143`).
pub const DSU_SLOTS: usize = 4;

/// Length of the fixed DSU header shared by requests and replies.
pub const HEADER_LEN: usize = 16;

/// Length of the request header (fixed header + message type).
pub const REQUEST_HEADER_LEN: usize = 20;

/// Configuration for [`DsuServer::bind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsuConfig {
    /// Address to bind the UDP socket to. Use port `0` for tests.
    pub bind: SocketAddr,
}

impl Default for DsuConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 26760)),
        }
    }
}

/// Latest input snapshot fed into a slot.
///
/// `accel` and `gyro` are **raw IMU LSB samples** (the `i16` triples from
/// [`crate::protocol::InputReport`] converted to `f32`); scaling and axis signs
/// are applied inside the DSU payload builder exactly like `dsu.py:167-177`.
/// `sticks` is `[left_x, left_y, right_x, right_y]` in `0..=255` with `128`
/// neutral; `triggers` is `[lt, rt]` in `0..=255`; `buttons` is the Switch
/// 32-bit mask from [`button_mask`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionInput {
    pub accel: [f32; 3],
    pub gyro: [f32; 3],
    pub sticks: [u8; 4],
    pub triggers: [u8; 2],
    pub buttons: u32,
    pub timestamp_us: u64,
}

impl Default for MotionInput {
    fn default() -> Self {
        Self {
            accel: [0.0; 3],
            gyro: [0.0; 3],
            sticks: [128; 4],
            triggers: [0; 2],
            buttons: 0,
            timestamp_us: 0,
        }
    }
}

/// A parsed DSU request header (`dsu.py:292-307`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request<'a> {
    pub version: u16,
    pub length: u16,
    pub crc32: u32,
    pub id: u32,
    pub message_type: u32,
    pub payload: &'a [u8],
}

/// Fixed 16-byte DSU header (`dsu.py:183-190`).
///
/// Layout: magic `[u8;4]`, protocol version `u16`, packet length `u16`, CRC32
/// `u32` (computed with this field zeroed), and the sender id `u32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsuHeader {
    pub magic: [u8; 4],
    pub version: u16,
    pub length: u16,
    pub crc32: u32,
    pub id: u32,
}

impl DsuHeader {
    pub const fn new(
        magic: [u8; 4],
        version: u16,
        length: u16,
        crc32: u32,
        id: u32,
    ) -> Self {
        Self {
            magic,
            version,
            length,
            crc32,
            id,
        }
    }
}

// ---------------------------------------------------------------------------
// CRC32 (IEEE, reflected, polynomial 0xEDB88320)
// ---------------------------------------------------------------------------

static CRC_TABLE: OnceLock<[u32; 256]> = OnceLock::new();

fn crc_table() -> &'static [u32; 256] {
    CRC_TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut crc = i as u32;
            for _ in 0..8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0xEDB8_8320;
                } else {
                    crc >>= 1;
                }
            }
            *entry = crc;
        }
        table
    })
}

/// CRC32 matching Python's `zlib.crc32` (`dsu.py:188`).
pub fn crc32(data: &[u8]) -> u32 {
    let table = crc_table();
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ table[index];
    }
    crc ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------------------
// Header / packet encoding
// ---------------------------------------------------------------------------

/// Encode a fixed header into its 16 little-endian bytes.
pub fn encode_header(header: &DsuHeader) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[0..4].copy_from_slice(&header.magic);
    out[4..6].copy_from_slice(&header.version.to_le_bytes());
    out[6..8].copy_from_slice(&header.length.to_le_bytes());
    out[8..12].copy_from_slice(&header.crc32.to_le_bytes());
    out[12..16].copy_from_slice(&header.id.to_le_bytes());
    out
}

/// Decode a fixed header; returns `None` when fewer than 16 bytes are present.
pub fn decode_header(data: &[u8]) -> Option<DsuHeader> {
    if data.len() < HEADER_LEN {
        return None;
    }
    Some(DsuHeader {
        magic: [data[0], data[1], data[2], data[3]],
        version: u16::from_le_bytes([data[4], data[5]]),
        length: u16::from_le_bytes([data[6], data[7]]),
        crc32: u32::from_le_bytes([data[8], data[9], data[10], data[11]]),
        id: u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
    })
}

/// Parse a `DSUC` request and expose its payload (`dsu.py:297-299`).
///
/// The legacy server only checks the 20-byte minimum length and the `DSUC`
/// magic; the version, length, and CRC fields are deliberately not validated
/// (`dsu.py:297-299`).
pub fn parse_request(data: &[u8]) -> Option<Request<'_>> {
    if data.len() < REQUEST_HEADER_LEN || data[0..4] != DSU_CLIENT_MAGIC {
        return None;
    }
    Some(Request {
        version: u16::from_le_bytes([data[4], data[5]]),
        length: u16::from_le_bytes([data[6], data[7]]),
        crc32: u32::from_le_bytes([data[8], data[9], data[10], data[11]]),
        id: u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
        message_type: u32::from_le_bytes([data[16], data[17], data[18], data[19]]),
        payload: &data[REQUEST_HEADER_LEN..],
    })
}

/// Build a complete `DSUS` packet (`dsu.py:183-190`).
///
/// `payload` is the message body **after** the 4-byte message type. The length
/// field counts the message type plus payload, and the CRC covers the whole
/// packet with the CRC field zeroed.
fn build_packet(server_id: u32, message_type: u32, payload: &[u8]) -> Vec<u8> {
    let body_len = 4 + payload.len();
    let mut packet = Vec::with_capacity(HEADER_LEN + body_len);
    packet.extend_from_slice(&DSU_SERVER_MAGIC);
    packet.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    packet.extend_from_slice(&(body_len as u16).to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
    packet.extend_from_slice(&server_id.to_le_bytes());
    packet.extend_from_slice(&message_type.to_le_bytes());
    packet.extend_from_slice(payload);

    let crc = crc32(&packet);
    packet[8..12].copy_from_slice(&crc.to_le_bytes());
    packet
}

/// Version reply payload: protocol version as `u16` (`dsu.py:306`).
pub fn version_payload() -> [u8; 2] {
    PROTOCOL_VERSION.to_le_bytes()
}

/// Complete version reply packet (`dsu.py:305-306`).
pub fn version_response(server_id: u32) -> Vec<u8> {
    build_packet(server_id, MSG_VERSION, &version_payload())
}

// ---------------------------------------------------------------------------
// Pure payload builders
// ---------------------------------------------------------------------------

/// Parse the slot list carried by a `MSG_PORTS` request.
///
/// Layout is `i32le count` followed by one byte per slot (`dsu.py:323-333`).
/// The count is clamped to `0..=4` and out-of-range slot bytes are skipped,
/// matching the legacy behaviour. `MSG_DATA` subscriptions deliberately do
/// **not** use this: legacy ignores their payload entirely (`dsu.py:309-321`).
pub fn parse_slot_list(payload: &[u8]) -> Vec<u8> {
    if payload.len() < 4 {
        return Vec::new();
    }
    let count = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let count = count.clamp(0, DSU_SLOTS as i32) as usize;
    payload[4..]
        .iter()
        .take(count)
        .copied()
        .filter(|slot| (*slot as usize) < DSU_SLOTS)
        .collect()
}

/// Map a 1-based player number to its DSU slot (`dsu.py:495` in `bridge.py`).
///
/// `slot = player.saturating_sub(1).min(3)`, so players 5-8 all land in slot 3.
pub fn slot_for_player(player: u8) -> u8 {
    player.saturating_sub(1).min((DSU_SLOTS - 1) as u8)
}

/// Map millivolts to the DSU battery enum (`dsu.py:56-66`).
pub fn battery_enum(battery_mv: u16) -> u8 {
    if battery_mv == 0 {
        0x00
    } else if battery_mv >= 4000 {
        0x05
    } else if battery_mv >= 3800 {
        0x04
    } else if battery_mv >= 3600 {
        0x03
    } else if battery_mv >= 3400 {
        0x02
    } else {
        0x01
    }
}

/// Convert `AA:BB:CC:DD:EE:FF` into bytes, zero-padding/truncating to six
/// (`dsu.py:87-91`). Any malformed component falls back to all zeros.
pub fn mac_to_bytes(mac: &str) -> [u8; 6] {
    let mut out = [0u8; 6];
    if mac.is_empty() {
        return out;
    }
    for (index, part) in mac.split(':').enumerate() {
        if index >= out.len() {
            break;
        }
        match u8::from_str_radix(part, 16) {
            Ok(value) => out[index] = value,
            Err(_) => return [0u8; 6],
        }
    }
    out
}

/// Scale raw IMU samples with the legacy constants and axis signs
/// (`dsu.py:45-51`, `dsu.py:167-177`).
pub fn scale_motion(accel_raw: [f32; 3], gyro_raw: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let mut accel = [0.0f32; 3];
    let mut gyro = [0.0f32; 3];
    for axis in 0..3 {
        accel[axis] = accel_raw[axis] * ACCEL_G_PER_LSB * ACCEL_SIGN[axis];
        gyro[axis] = gyro_raw[axis] * GYRO_DPS_PER_LSB * GYRO_SIGN[axis];
    }
    (accel, gyro)
}

/// Shared 11-byte controller header (`dsu.py:192-197`).
fn shared_header(slot: u8, connected: bool, mac: &[u8; 6], battery: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(11);
    out.push(slot.min((DSU_SLOTS - 1) as u8));
    out.push(if connected {
        STATE_CONNECTED
    } else {
        STATE_DISCONNECTED
    });
    out.push(if connected { MODEL_FULL_GYRO } else { 0 });
    out.push(if connected { CONN_BLUETOOTH } else { 0 });
    out.extend_from_slice(mac);
    out.push(battery);
    out
}

fn info_payload_bytes(
    slot: u8,
    connected: bool,
    mac: &[u8; 6],
    battery: u8,
) -> Vec<u8> {
    // `dsu.py:199-200`: shared header + one zero padding byte.
    let mut out = shared_header(slot, connected, mac, battery);
    out.push(0);
    out
}

/// Build a controller-info payload (`dsu.py:199-200`). Always 12 bytes.
pub fn info_payload(slot: u8, connected: bool, mac: &str, battery: u8) -> Vec<u8> {
    info_payload_bytes(slot, connected, &mac_to_bytes(mac), battery)
}

fn digital_buttons(buttons: u32) -> (u8, u8) {
    // `dsu.py:221-240`.
    let mut d0 = 0u8;
    if buttons & button_mask::LEFT != 0 {
        d0 |= 0x80;
    }
    if buttons & button_mask::DOWN != 0 {
        d0 |= 0x40;
    }
    if buttons & button_mask::RIGHT != 0 {
        d0 |= 0x20;
    }
    if buttons & button_mask::UP != 0 {
        d0 |= 0x10;
    }
    if buttons & button_mask::PLUS != 0 {
        d0 |= 0x08;
    }
    if buttons & button_mask::R_STICK_BTN != 0 {
        d0 |= 0x04;
    }
    if buttons & button_mask::L_STICK_BTN != 0 {
        d0 |= 0x02;
    }
    if buttons & button_mask::MINUS != 0 {
        d0 |= 0x01;
    }

    let mut d1 = 0u8;
    if buttons & button_mask::Y != 0 {
        d1 |= 0x80;
    }
    if buttons & button_mask::B != 0 {
        d1 |= 0x40;
    }
    if buttons & button_mask::A != 0 {
        d1 |= 0x20;
    }
    if buttons & button_mask::X != 0 {
        d1 |= 0x10;
    }
    if buttons & button_mask::R != 0 {
        d1 |= 0x08;
    }
    if buttons & button_mask::L != 0 {
        d1 |= 0x04;
    }
    if buttons & button_mask::ZR != 0 {
        d1 |= 0x02;
    }
    if buttons & button_mask::ZL != 0 {
        d1 |= 0x01;
    }
    (d0, d1)
}

fn pad_data_payload_bytes(
    slot: u8,
    connected: bool,
    mac: &[u8; 6],
    battery: u8,
    packet_number: u32,
    frame: &MotionInput,
) -> Vec<u8> {
    // `dsu.py:202-270`, in exactly the legacy field order.
    let mut out = shared_header(slot, connected, mac, battery);

    out.push(u8::from(connected));
    out.extend_from_slice(&packet_number.to_le_bytes());

    let (d0, d1) = digital_buttons(frame.buttons);
    out.push(d0);
    out.push(d1);
    out.push(u8::from(frame.buttons & button_mask::HOME != 0));
    out.push(0); // touch button

    out.extend_from_slice(&frame.sticks);

    let analog = |mask: u32| if frame.buttons & mask != 0 { 255 } else { 0 };
    // Analog dpad.
    out.extend_from_slice(&[
        analog(button_mask::LEFT),
        analog(button_mask::DOWN),
        analog(button_mask::RIGHT),
        analog(button_mask::UP),
    ]);
    // Analog face buttons.
    out.extend_from_slice(&[
        analog(button_mask::Y),
        analog(button_mask::B),
        analog(button_mask::A),
        analog(button_mask::X),
    ]);
    // Analog R1/L1/R2/L2.
    out.extend_from_slice(&[
        analog(button_mask::R),
        analog(button_mask::L),
        frame.triggers[1],
        frame.triggers[0],
    ]);

    // Two inactive touch points.
    out.extend_from_slice(&[0u8; 12]);

    out.extend_from_slice(&frame.timestamp_us.to_le_bytes());
    let (accel, gyro) = scale_motion(frame.accel, frame.gyro);
    for value in accel {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in gyro {
        out.extend_from_slice(&value.to_le_bytes());
    }

    debug_assert_eq!(out.len(), 80);
    out
}

/// Build a pad-data payload (`dsu.py:202-270`). Always 80 bytes.
pub fn pad_data_payload(
    slot: u8,
    connected: bool,
    mac: &str,
    battery: u8,
    packet_number: u32,
    frame: &MotionInput,
) -> Vec<u8> {
    pad_data_payload_bytes(
        slot,
        connected,
        &mac_to_bytes(mac),
        battery,
        packet_number,
        frame,
    )
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct SlotState {
    connected: bool,
    mac: [u8; 6],
    battery: u8,
    packet_number: u32,
    frame: MotionInput,
}

impl Default for SlotState {
    fn default() -> Self {
        Self {
            connected: false,
            mac: [0u8; 6],
            battery: 0,
            packet_number: 0,
            frame: MotionInput::default(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SlotSnapshot {
    connected: bool,
    mac: [u8; 6],
    battery: u8,
    packet_number: u32,
    frame: MotionInput,
}

impl From<&SlotState> for SlotSnapshot {
    fn from(slot: &SlotState) -> Self {
        Self {
            connected: slot.connected,
            mac: slot.mac,
            battery: slot.battery,
            packet_number: slot.packet_number,
            frame: slot.frame,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

static SERVER_ID_SEQ: AtomicU32 = AtomicU32::new(0);

fn random_server_id() -> u32 {
    // No `rand` dependency: mix wall clock, pid, and a per-process sequence.
    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    SERVER_ID_SEQ
        .fetch_add(1, Ordering::Relaxed)
        .hash(&mut hasher);
    hasher.finish() as u32
}

/// Four-slot DSU/cemuhook server (`dsu.py:94-333`).
///
/// Call [`DsuServer::bind`] once, then move the server into a dedicated thread
/// and run [`DsuServer::run`]. Input producers call [`DsuServer::set_slot`],
/// [`DsuServer::update`], and [`DsuServer::drop_slot`] from any thread.
#[derive(Debug)]
pub struct DsuServer {
    socket: UdpSocket,
    server_id: u32,
    slots: Mutex<[SlotState; DSU_SLOTS]>,
    clients: Mutex<HashMap<SocketAddr, Instant>>,
    warned_clamps: Mutex<HashSet<(String, u8)>>,
}

impl DsuServer {
    /// Bind the UDP socket (`dsu.py:112-126`). Use a port-0 address for tests.
    pub fn bind(cfg: DsuConfig) -> io::Result<Self> {
        let socket = UdpSocket::bind(cfg.bind)?;
        socket.set_read_timeout(Some(READ_TIMEOUT))?;
        Ok(Self {
            socket,
            server_id: random_server_id(),
            slots: Mutex::new([SlotState::default(); DSU_SLOTS]),
            clients: Mutex::new(HashMap::new()),
            warned_clamps: Mutex::new(HashSet::new()),
        })
    }

    /// Address the server is actually bound to (useful with port 0).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Register or clear a slot (`dsu.py:141-151`).
    ///
    /// `slot` is clamped to `0..=3`; slots above 3 (i.e. raw `player - 1` for
    /// players 5-8) emit one warning per `(mac, player)`. Callers that already
    /// map through [`slot_for_player`] pass an in-range slot and can use
    /// [`DsuServer::set_player_slot`] to get the warning.
    pub fn set_slot(&self, slot: u8, connected: bool, mac: &str, battery_mv: u16) {
        if slot as usize >= DSU_SLOTS && connected {
            self.warn_clamp_once(mac, slot.saturating_add(1));
        }
        self.write_slot(slot, connected, mac, battery_mv);
    }

    /// Player-oriented wrapper around [`DsuServer::set_slot`] that clamps
    /// players 5-8 to slot 3 and warns once per `(mac, player)`.
    pub fn set_player_slot(
        &self,
        player: u8,
        connected: bool,
        mac: &str,
        battery_mv: u16,
    ) {
        if connected && player > DSU_SLOTS as u8 {
            self.warn_clamp_once(mac, player);
        }
        self.write_slot(slot_for_player(player), connected, mac, battery_mv);
    }

    fn write_slot(&self, slot: u8, connected: bool, mac: &str, battery_mv: u16) {
        let index = (slot as usize).min(DSU_SLOTS - 1);
        let mut slots = lock(&self.slots);
        let state = &mut slots[index];
        state.connected = connected;
        state.mac = mac_to_bytes(mac);
        state.battery = battery_enum(battery_mv);
        if !connected {
            // `dsu.py:149-151`: clear motion when the slot disconnects.
            state.frame.accel = [0.0; 3];
            state.frame.gyro = [0.0; 3];
        }
    }

    fn warn_clamp_once(&self, mac: &str, player: u8) {
        let key = (mac.to_string(), player);
        let mut warned = lock(&self.warned_clamps);
        if warned.insert(key) {
            tracing::warn!(
                mac = %mac,
                player = player,
                slot = slot_for_player(player),
                "DSU has four motion slots; players 5-8 share slot 3"
            );
        }
    }

    /// Push the latest input snapshot for a slot and broadcast it to subscribed
    /// clients (`dsu.py:153-181`). `frame.accel`/`frame.gyro` are raw IMU LSB
    /// samples; scaling happens when the packet is built.
    pub fn update(&self, slot: u8, frame: &MotionInput) {
        if slot as usize >= DSU_SLOTS {
            return;
        }
        {
            let mut slots = lock(&self.slots);
            slots[slot as usize].frame = *frame;
        }
        self.broadcast(slot);
    }

    /// Fully clear a slot (used on teardown; `dsu.py:141-151`).
    pub fn drop_slot(&self, slot: u8) {
        if slot as usize >= DSU_SLOTS {
            return;
        }
        let mut slots = lock(&self.slots);
        slots[slot as usize] = SlotState::default();
    }

    /// Blocking receive loop. The 500 ms socket read timeout lets the loop
    /// notice `stop` and exit cleanly.
    pub fn run(&self, stop: Arc<AtomicBool>) -> io::Result<()> {
        let mut buffer = [0u8; 1024];
        while !stop.load(Ordering::Relaxed) {
            match self.socket.recv_from(&mut buffer) {
                Ok((len, addr)) => self.handle_datagram(&buffer[..len], addr),
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                            | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn handle_datagram(&self, data: &[u8], addr: SocketAddr) {
        let Some(request) = parse_request(data) else {
            return;
        };
        match request.message_type {
            MSG_VERSION => {
                // `dsu.py:305-306`.
                let packet = version_response(self.server_id);
                let _ = self.socket.send_to(&packet, addr);
            }
            MSG_PORTS => {
                // `dsu.py:323-333`.
                for slot in parse_slot_list(request.payload) {
                    let packet = self.info_packet(slot);
                    let _ = self.socket.send_to(&packet, addr);
                }
            }
            MSG_DATA => {
                // `dsu.py:309-321`: the legacy server never reads the request
                // payload for data subscriptions. Register the subscriber and
                // reply for every connected slot, regardless of payload bytes
                // (clients send 0xFF or a bare slot byte for "all").
                {
                    let mut clients = lock(&self.clients);
                    clients.insert(addr, Instant::now());
                }
                for slot in self.connected_slots() {
                    let packet = self.pad_data_packet(slot);
                    let _ = self.socket.send_to(&packet, addr);
                }
            }
            _ => {}
        }
    }

    fn connected_slots(&self) -> Vec<u8> {
        let slots = lock(&self.slots);
        slots
            .iter()
            .enumerate()
            .filter(|(_, state)| state.connected)
            .map(|(index, _)| index as u8)
            .collect()
    }

    fn info_packet(&self, slot: u8) -> Vec<u8> {
        let snapshot = {
            let slots = lock(&self.slots);
            slots
                .get(slot as usize)
                .map(SlotSnapshot::from)
                .unwrap_or(SlotSnapshot {
                    connected: false,
                    mac: [0u8; 6],
                    battery: 0,
                    packet_number: 0,
                    frame: MotionInput::default(),
                })
        };
        let payload = info_payload_bytes(
            slot,
            snapshot.connected,
            &snapshot.mac,
            snapshot.battery,
        );
        build_packet(self.server_id, MSG_PORTS, &payload)
    }

    fn pad_data_packet(&self, slot: u8) -> Vec<u8> {
        let snapshot = {
            let mut slots = lock(&self.slots);
            let state = &mut slots[slot as usize];
            state.packet_number = state.packet_number.wrapping_add(1);
            SlotSnapshot::from(&*state)
        };
        let payload = pad_data_payload_bytes(
            slot,
            snapshot.connected,
            &snapshot.mac,
            snapshot.battery,
            snapshot.packet_number,
            &snapshot.frame,
        );
        build_packet(self.server_id, MSG_DATA, &payload)
    }

    fn broadcast(&self, slot: u8) {
        // `dsu.py:276-288`: prune clients that stopped polling, then send one
        // freshly numbered packet to every remaining subscriber.
        let now = Instant::now();
        let clients: Vec<SocketAddr> = {
            let mut clients = lock(&self.clients);
            clients.retain(|_, last_seen| {
                now.duration_since(*last_seen) <= CLIENT_TIMEOUT
            });
            clients.keys().copied().collect()
        };
        if clients.is_empty() {
            return;
        }
        let packet = self.pad_data_packet(slot);
        for addr in clients {
            let _ = self.socket.send_to(&packet, addr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn request(message_type: u32, payload: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(REQUEST_HEADER_LEN + payload.len());
        data.extend_from_slice(&DSU_CLIENT_MAGIC);
        data.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        data.extend_from_slice(&((4 + payload.len()) as u16).to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes()); // CRC is not validated
        data.extend_from_slice(&0x1234_5678u32.to_le_bytes()); // client id
        data.extend_from_slice(&message_type.to_le_bytes());
        data.extend_from_slice(payload);
        data
    }

    fn crc_is_valid(packet: &[u8]) -> bool {
        if packet.len() < HEADER_LEN {
            return false;
        }
        let stored = u32::from_le_bytes([packet[8], packet[9], packet[10], packet[11]]);
        let mut copy = packet.to_vec();
        copy[8..12].fill(0);
        crc32(&copy) == stored
    }

    fn test_server() -> DsuServer {
        DsuServer::bind(DsuConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        })
        .expect("bind ephemeral DSU socket")
    }

    #[test]
    fn crc32_matches_known_vectors() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"0123456789abcdef0123456789abcdef"),
            0x7759_B50E
        );
    }

    #[test]
    fn header_round_trip() {
        let header = DsuHeader::new(DSU_SERVER_MAGIC, PROTOCOL_VERSION, 84, 0x1234_5678, 0x9ABC_DEF0);
        let encoded = encode_header(&header);
        assert_eq!(encoded.len(), HEADER_LEN);
        assert_eq!(&encoded[0..4], b"DSUS");
        assert_eq!(decode_header(&encoded), Some(header));
        assert_eq!(decode_header(&encoded[..15]), None);
    }

    #[test]
    fn version_response_payload_matches_legacy() {
        let packet = version_response(0xDEAD_BEEF);
        assert_eq!(packet.len(), 22);
        assert_eq!(&packet[0..4], b"DSUS");
        assert_eq!(
            u16::from_le_bytes([packet[4], packet[5]]),
            PROTOCOL_VERSION
        );
        assert_eq!(u16::from_le_bytes([packet[6], packet[7]]), 6);
        assert!(crc_is_valid(&packet));
        assert_eq!(
            u32::from_le_bytes([packet[12], packet[13], packet[14], packet[15]]),
            0xDEAD_BEEF
        );
        assert_eq!(
            u32::from_le_bytes([packet[16], packet[17], packet[18], packet[19]]),
            MSG_VERSION
        );
        assert_eq!(&packet[20..22], &version_payload());
        assert_eq!(version_payload(), [0xE9, 0x03]);
        assert_eq!(&packet[20..22], &[0xE9, 0x03]);
    }

    #[test]
    fn info_response_covers_four_slots() {
        let mac = "AA:BB:CC:DD:EE:FF";
        for slot in 0..DSU_SLOTS as u8 {
            let payload = info_payload(slot, true, mac, battery_enum(4000));
            assert_eq!(payload.len(), 12);
            assert_eq!(payload[0], slot);
            assert_eq!(payload[1], STATE_CONNECTED);
            assert_eq!(payload[2], MODEL_FULL_GYRO);
            assert_eq!(payload[3], CONN_BLUETOOTH);
            assert_eq!(&payload[4..10], &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
            assert_eq!(payload[10], 0x05);
            assert_eq!(payload[11], 0x00);
        }

        let disconnected = info_payload(3, false, "", 0);
        assert_eq!(disconnected[0], 3);
        assert_eq!(disconnected[1], STATE_DISCONNECTED);
        assert_eq!(disconnected[2], 0);
        assert_eq!(disconnected[3], 0);
        assert_eq!(&disconnected[4..10], &[0u8; 6]);
        assert_eq!(disconnected[10], 0);
    }

    #[test]
    fn subscription_request_is_parsed() {
        let payload = [2, 0, 0, 0, 0, 2]; // count = 2, slots = [0, 2]
        let data = request(MSG_DATA, &payload);
        let parsed = parse_request(&data).expect("request parses");
        assert_eq!(parsed.version, PROTOCOL_VERSION);
        assert_eq!(parsed.id, 0x1234_5678);
        assert_eq!(parsed.message_type, MSG_DATA);
        assert_eq!(parsed.payload, &payload);

        // The count-prefixed slot list is only interpreted for MSG_PORTS.
        let ports = request(MSG_PORTS, &payload);
        let parsed_ports = parse_request(&ports).expect("ports request parses");
        assert_eq!(parsed_ports.message_type, MSG_PORTS);
        assert_eq!(parse_slot_list(parsed_ports.payload), vec![0, 2]);
    }

    #[test]
    fn data_subscription_ignores_payload_slot_list() {
        // Legacy ignores the MSG_DATA payload and replies for every connected
        // slot. Clients send a bare slot byte, 0xFF, or a count prefix; none of
        // those may filter the reply set.
        let server = test_server();
        let addr = server.local_addr().expect("local addr");
        server.set_slot(0, true, "AA:BB:CC:DD:EE:00", 4000);
        server.set_slot(2, true, "AA:BB:CC:DD:EE:02", 4000);

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || server.run(thread_stop));

        let client = UdpSocket::bind(("127.0.0.1", 0)).expect("client bind");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("client timeout");

        let payloads: [&[u8]; 4] = [
            &[0x00],          // one-byte "slot 0"
            &[0xFF],          // one-byte "all"
            &[0, 0, 0, 0],    // count = 0
            &[1, 0, 0, 0, 0], // count = 1, slot = [0] (legacy would ignore)
        ];

        for payload in payloads {
            client
                .send_to(&request(MSG_DATA, payload), addr)
                .expect("send subscription");

            let mut slots_seen = Vec::new();
            for _ in 0..2 {
                let mut buffer = [0u8; 256];
                let (len, from) = client.recv_from(&mut buffer).expect("pad data reply");
                assert_eq!(from, addr);
                let packet = &buffer[..len];
                assert_eq!(packet.len(), 100);
                assert_eq!(&packet[0..4], b"DSUS");
                assert!(crc_is_valid(packet));
                assert_eq!(u16::from_le_bytes([packet[6], packet[7]]), 84);
                assert_eq!(
                    u32::from_le_bytes([packet[16], packet[17], packet[18], packet[19]]),
                    MSG_DATA
                );
                slots_seen.push(packet[20]);
            }
            slots_seen.sort_unstable();
            assert_eq!(
                slots_seen,
                vec![0, 2],
                "payload {payload:?} must not filter the reply set"
            );
        }

        stop.store(true, Ordering::SeqCst);
        handle.join().expect("server thread joins").expect("run ok");
    }

    #[test]
    fn slot_clamps_for_players_five_to_eight() {
        assert_eq!(slot_for_player(0), 0);
        assert_eq!(slot_for_player(1), 0);
        assert_eq!(slot_for_player(4), 3);
        for player in 5..=8 {
            assert_eq!(slot_for_player(player), 3);
        }
        assert_eq!(slot_for_player(255), 3);

        let mac = "AA:BB:CC:DD:EE:FF";
        let server = test_server();
        server.set_player_slot(7, true, mac, 4000);
        {
            let slots = lock(&server.slots);
            assert!(slots[3].connected);
            assert_eq!(slots[3].mac, mac_to_bytes(mac));
        }
        {
            let warned = lock(&server.warned_clamps);
            assert!(warned.contains(&(mac.to_string(), 7)));
        }
        // A second call for the same (mac, player) must not warn again.
        server.set_player_slot(7, true, mac, 4000);
        assert_eq!(lock(&server.warned_clamps).len(), 1);
    }

    #[test]
    fn battery_enum_boundaries() {
        assert_eq!(battery_enum(0), 0);
        assert_eq!(battery_enum(3399), 1);
        assert_eq!(battery_enum(3400), 2);
        assert_eq!(battery_enum(3599), 2);
        assert_eq!(battery_enum(3600), 3);
        assert_eq!(battery_enum(3799), 3);
        assert_eq!(battery_enum(3800), 4);
        assert_eq!(battery_enum(3999), 4);
        assert_eq!(battery_enum(4000), 5);
        assert_eq!(battery_enum(u16::MAX), 5);
    }

    #[test]
    fn motion_scaling_matches_legacy() {
        let (accel, gyro) = scale_motion(
            [4096.0, -2048.0, 0.0],
            [16.4, -32.8, 0.0],
        );
        assert!((accel[0] - 1.0).abs() < 1e-6);
        assert!((accel[1] + 0.5).abs() < 1e-6);
        assert_eq!(accel[2], 0.0);
        assert!((gyro[0] - 1.0).abs() < 1e-5);
        assert!((gyro[1] + 2.0).abs() < 1e-5);
        assert_eq!(gyro[2], 0.0);

        // Axis signs are identity, like ACCEL_SIGN/GYRO_SIGN in dsu.py.
        assert_eq!(ACCEL_SIGN, [1.0, 1.0, 1.0]);
        assert_eq!(GYRO_SIGN, [1.0, 1.0, 1.0]);

        // The pad-data payload embeds the scaled floats at the legacy offsets.
        let frame = MotionInput {
            accel: [4096.0, 0.0, 0.0],
            gyro: [0.0, 0.0, 16.4],
            sticks: [1, 2, 3, 4],
            triggers: [5, 6],
            buttons: button_mask::A,
            timestamp_us: 0x0102_0304_0506_0708,
        };
        let payload = pad_data_payload(0, true, "AA:BB:CC:DD:EE:FF", 5, 7, &frame);
        assert_eq!(payload.len(), 80);
        // shared header 11 + connected 1 + packet 4 + buttons 4 + sticks 4
        // + dpad 4 + face 4 + shoulders 4 + touch 12 + timestamp 8 = 56.
        let accel_offset = 56;
        let gyro_offset = 68;
        assert_eq!(
            f32::from_le_bytes([
                payload[accel_offset],
                payload[accel_offset + 1],
                payload[accel_offset + 2],
                payload[accel_offset + 3],
            ]),
            1.0
        );
        assert_eq!(
            f32::from_le_bytes([
                payload[gyro_offset + 8],
                payload[gyro_offset + 9],
                payload[gyro_offset + 10],
                payload[gyro_offset + 11],
            ]),
            1.0
        );
    }

    #[test]
    fn loopback_version_exchange() {
        let server = test_server();
        let addr = server.local_addr().expect("local addr");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || server.run(thread_stop));

        let client = UdpSocket::bind(("127.0.0.1", 0)).expect("client bind");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("client timeout");
        client
            .send_to(&request(MSG_VERSION, &[]), addr)
            .expect("send version request");

        let mut buffer = [0u8; 256];
        let (len, from) = client.recv_from(&mut buffer).expect("receive version reply");
        assert_eq!(from, addr);
        let packet = &buffer[..len];

        assert_eq!(packet.len(), 22);
        assert_eq!(&packet[0..4], b"DSUS");
        assert!(crc_is_valid(packet));
        let header = decode_header(packet).expect("header");
        assert_eq!(header.version, PROTOCOL_VERSION);
        assert_eq!(header.length, 6);
        assert_eq!(
            u32::from_le_bytes([packet[16], packet[17], packet[18], packet[19]]),
            MSG_VERSION
        );
        assert_eq!(&packet[20..22], &[0xE9, 0x03]);

        stop.store(true, Ordering::SeqCst);
        handle.join().expect("server thread joins").expect("run ok");
    }
}
