//! End-to-end motion path: real UDP client -> DsuBridge -> DsuServer -> DSUS.
//!
//! Uses only std sockets and the public crate API. No controller/hardware.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use switch2d::controller::CalibratedInput;
use switch2d::dsu::{
    battery_enum, crc32, decode_header, DSU_CLIENT_MAGIC, DSU_SERVER_MAGIC, HEADER_LEN, MSG_DATA,
    PROTOCOL_VERSION, REQUEST_HEADER_LEN,
};
use switch2d::protocol::report::InputReport;
use switch2d::DsuBridge;

/// DSU fixed header + message type, then the 80-byte pad-data payload.
const DATA_PAYLOAD_OFFSET: usize = HEADER_LEN + 4;
/// `pad_data_payload` offsets from its own start (dsu.rs legacy layout).
const ACCEL_OFFSET: usize = 56;
const GYRO_OFFSET: usize = 68;

const READ_TIMEOUT: Duration = Duration::from_millis(500);

fn client_request(message_type: u32, payload: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(REQUEST_HEADER_LEN + payload.len());
    data.extend_from_slice(&DSU_CLIENT_MAGIC);
    data.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    data.extend_from_slice(&((4 + payload.len()) as u16).to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes()); // CRC not validated by the server
    data.extend_from_slice(&0x1234_5678u32.to_le_bytes());
    data.extend_from_slice(&message_type.to_le_bytes());
    data.extend_from_slice(payload);
    data
}

fn subscriber(addr: SocketAddr) -> UdpSocket {
    let socket = UdpSocket::bind(("127.0.0.1", 0)).expect("bind subscriber socket");
    socket
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("set subscriber timeout");
    socket.connect(addr).expect("connect subscriber");
    socket
}

fn recv_packet(socket: &UdpSocket, buffer: &mut [u8]) -> usize {
    match socket.recv(buffer) {
        Ok(len) => len,
        Err(err) => panic!("no DSUS reply within {READ_TIMEOUT:?}: {err}"),
    }
}

fn f32_at(packet: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes([
        packet[offset],
        packet[offset + 1],
        packet[offset + 2],
        packet[offset + 3],
    ])
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

fn report(accel: (i16, i16, i16), gyro: (i16, i16, i16)) -> InputReport {
    InputReport {
        timestamp: 0,
        buttons: switch2d::protocol::constants::button_mask::A,
        left_stick_raw: (0, 0),
        right_stick_raw: (0, 0),
        battery_mv: 3700,
        accel,
        gyro,
        left_trigger_raw: 0,
        right_trigger_raw: 0,
    }
}

fn calibrated_circle_edge() -> CalibratedInput {
    CalibratedInput {
        left: (1.0, 0.0),
        right: (0.0, -1.0),
        lt: 12,
        rt: 200,
    }
}

#[test]
fn dsu_bridge_loopback_reports_motion_and_disconnects() {
    let bridge = DsuBridge::start(SocketAddr::from(([127, 0, 0, 1], 0))).expect("start DSU bridge");
    let server_addr = bridge.local_addr().expect("ephemeral DSU address");

    let player = 2; // slot 1, so the shared-header slot is not just the default 0
    let mac = "AA:BB:CC:DD:EE:01";
    let battery_mv = 3700;
    let slot = bridge.session_started(player, mac, battery_mv);
    assert_eq!(slot, 1);

    let cal = calibrated_circle_edge();
    // A few updates: the last one is the frame the subscription must see.
    bridge.update(slot, &report((0, 0, 0), (0, 0, 0)), &cal);
    bridge.update(slot, &report((4096, -2048, 0), (0, 0, 0)), &cal);
    bridge.update(slot, &report((4096, -2048, 0), (164, -328, 0)), &cal);

    let socket = subscriber(server_addr);
    socket
        .send(&client_request(MSG_DATA, &[0xFF]))
        .expect("send data subscription");

    let mut buffer = [0u8; 256];
    let len = recv_packet(&socket, &mut buffer);
    let packet = &buffer[..len];

    // Header/framing.
    assert_eq!(&packet[0..4], &DSU_SERVER_MAGIC, "DSUS magic");
    let header = decode_header(packet).expect("DSUS header");
    assert_eq!(header.version, PROTOCOL_VERSION, "protocol version");
    assert_eq!(header.length, 84, "4-byte message type + 80-byte payload");
    assert_eq!(
        packet.len(),
        HEADER_LEN + header.length as usize,
        "header length covers the whole body"
    );
    assert!(
        crc_is_valid(packet),
        "CRC32 over the packet with field zeroed"
    );
    assert_eq!(
        u32::from_le_bytes([packet[16], packet[17], packet[18], packet[19]]),
        MSG_DATA,
        "message type is MSG_DATA"
    );

    // Shared 11-byte controller header.
    let payload = &packet[DATA_PAYLOAD_OFFSET..];
    assert_eq!(payload[0], slot, "slot in shared header");
    assert_eq!(payload[1], switch2d::dsu::STATE_CONNECTED);
    assert_eq!(payload[2], switch2d::dsu::MODEL_FULL_GYRO);
    assert_eq!(payload[3], switch2d::dsu::CONN_BLUETOOTH);
    assert_eq!(
        &payload[4..10],
        &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01],
        "MAC in display order"
    );
    assert_eq!(
        payload[10],
        battery_enum(battery_mv),
        "battery enum from the session-start mV"
    );

    // Sticks: left (1.0, 0.0) / right (0.0, -1.0) at the circle edge.
    assert_eq!(&payload[20..24], &[255, 128, 128, 1]);

    // Raw IMU scaled by the legacy factors: 1/4096 g and 1/16.4 dps.
    let accel = [
        f32_at(packet, DATA_PAYLOAD_OFFSET + ACCEL_OFFSET),
        f32_at(packet, DATA_PAYLOAD_OFFSET + ACCEL_OFFSET + 4),
        f32_at(packet, DATA_PAYLOAD_OFFSET + ACCEL_OFFSET + 8),
    ];
    let gyro = [
        f32_at(packet, DATA_PAYLOAD_OFFSET + GYRO_OFFSET),
        f32_at(packet, DATA_PAYLOAD_OFFSET + GYRO_OFFSET + 4),
        f32_at(packet, DATA_PAYLOAD_OFFSET + GYRO_OFFSET + 8),
    ];
    assert!((accel[0] - 1.0).abs() < 1e-6, "accel x = {accel:?}");
    assert!((accel[1] + 0.5).abs() < 1e-6, "accel y = {accel:?}");
    assert!(accel[2].abs() < 1e-6, "accel z = {accel:?}");
    assert!((gyro[0] - 10.0).abs() < 1e-4, "gyro x = {gyro:?}");
    assert!((gyro[1] + 20.0).abs() < 1e-4, "gyro y = {gyro:?}");
    assert!(gyro[2].abs() < 1e-6, "gyro z = {gyro:?}");

    // Teardown: a fresh subscription must not return motion for that slot.
    bridge.session_ended(slot);
    socket
        .send(&client_request(MSG_DATA, &[0xFF]))
        .expect("send post-teardown subscription");

    match socket.recv(&mut buffer) {
        Ok(len) => panic!(
            "expected no data for a torn-down slot, got {len} bytes: {:02X?}",
            &buffer[..len]
        ),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            // Expected: no connected slots, so MSG_DATA produces no reply.
        }
        Err(err) => panic!("unexpected error after teardown: {err}"),
    }
}

#[test]
fn dsu_bridge_start_survives_unavailable_port() {
    // Linux UDP bind without SO_REUSEADDR rejects a second bind to the same
    // exact address, so an already-bound ephemeral port is a portable-enough
    // way to force start()'s failure path here.
    let occupied = UdpSocket::bind(("127.0.0.1", 0)).expect("bind occupied UDP port");
    let occupied_addr = occupied.local_addr().expect("occupied local addr");

    let bridge = DsuBridge::start(occupied_addr)
        .expect("start must return a no-op bridge when motion is unavailable");

    if bridge.local_addr().is_ok() {
        eprintln!(
            "skipped unavailable-port assertion: could bind {occupied_addr} twice on this platform"
        );
        return;
    }

    // The no-op bridge must still be usable by the daemon's call sites.
    let slot = bridge.session_started(1, "AA:BB:CC:DD:EE:FF", 3700);
    assert_eq!(slot, 0);
    bridge.update(
        slot,
        &report((0, 0, 0), (0, 0, 0)),
        &calibrated_circle_edge(),
    );
    bridge.session_ended(slot);
    assert!(
        bridge.local_addr().is_err(),
        "local_addr reports that motion is disabled"
    );
}
