//! ATT (Attribute Protocol) client over raw L2CAP socket for Switch 2 controllers.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use super::l2cap::L2capSocket;

pub const ATT_ERROR_RSP: u8 = 0x01;
pub const ATT_EXCHANGE_MTU_REQ: u8 = 0x02;
pub const ATT_EXCHANGE_MTU_RSP: u8 = 0x03;
pub const ATT_FIND_INFO_REQ: u8 = 0x04;
pub const ATT_FIND_INFO_RSP: u8 = 0x05;
pub const ATT_READ_BY_TYPE_REQ: u8 = 0x08;
pub const ATT_READ_BY_TYPE_RSP: u8 = 0x09;
pub const ATT_READ_REQ: u8 = 0x0A;
pub const ATT_READ_RSP: u8 = 0x0B;
pub const ATT_READ_BY_GROUP_REQ: u8 = 0x10;
pub const ATT_READ_BY_GROUP_RSP: u8 = 0x11;
pub const ATT_WRITE_REQ: u8 = 0x12;
pub const ATT_WRITE_RSP: u8 = 0x13;
pub const ATT_HANDLE_VALUE_NTF: u8 = 0x1B;
pub const ATT_HANDLE_VALUE_IND: u8 = 0x1D;
pub const ATT_HANDLE_VALUE_CFM: u8 = 0x1E;
pub const ATT_WRITE_CMD: u8 = 0x52;

pub const UUID_PRIMARY_SERVICE: u16 = 0x2800;
pub const UUID_CHARACTERISTIC: u16 = 0x2803;
pub const UUID_CCCD: u16 = 0x2902;

/// ATT "Attribute Not Found" error code: the normal end-of-range answer for
/// `ATT_FIND_INFO_REQ` when nothing remains in the requested range.
pub const ATT_ERR_ATTRIBUTE_NOT_FOUND: u8 = 0x0A;

/// Upper bound on FIND_INFO pages per characteristic (MTU-sized replies can
/// need several round-trips for descriptor-heavy characteristics).
const MAX_FIND_INFO_PAGES: usize = 32;

/// Log the discovered characteristic UUID list once per process (info level),
/// so the first hardware connect shows exactly what the walk returned.
static GATT_UUID_LIST_LOGGED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
pub struct Characteristic {
    pub decl_handle: u16,
    pub properties: u8,
    pub value_handle: u16,
    pub uuid: String,
    pub cccd_handle: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct Service {
    pub start_handle: u16,
    pub end_handle: u16,
    pub uuid: String,
    pub characteristics: Vec<Characteristic>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolvedHandles {
    pub input_report: u16,
    pub input_report_cccd: u16,
    pub command_write: u16,
    pub command_response: u16,
    pub command_response_cccd: u16,
    pub vibration: u16,
}

/// Which of the [`ResolvedHandles`] values came from real GATT discovery
/// rather than the hard-coded NSO GameCube safety-net defaults.
///
/// `true` = discovered; `false` = fallback was substituted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HandleProvenance {
    pub input_report: bool,
    pub input_report_cccd: bool,
    pub command_write: bool,
    pub command_response: bool,
    pub command_response_cccd: bool,
    pub vibration: bool,
}

/// Parse an `ATT_FIND_INFO_RSP` body into `(handle, raw uuid bytes)` entries.
///
/// Accepts format `0x01` (16-bit UUIDs, 4-byte items) and `0x02` (128-bit
/// UUIDs, 18-byte items). Returns `None` for a wrong opcode, an unknown format,
/// or a body that is not a whole number of items.
pub fn parse_find_info_rsp(pdu: &[u8]) -> Option<Vec<(u16, Vec<u8>)>> {
    if pdu.len() < 2 || pdu[0] != ATT_FIND_INFO_RSP {
        return None;
    }
    let uuid_len = match pdu[1] {
        0x01 => 2,
        0x02 => 16,
        _ => return None,
    };
    let body = &pdu[2..];
    if body.is_empty() || body.len() % (2 + uuid_len) != 0 {
        return None;
    }

    Some(
        body.chunks_exact(2 + uuid_len)
            .map(|item| (u16::from_le_bytes([item[0], item[1]]), item[2..].to_vec()))
            .collect(),
    )
}

/// Descriptor handle range belonging to a characteristic.
///
/// Starts at `value_handle + 1` and ends at `next_decl_handle - 1` when a
/// following characteristic exists in the same service, otherwise at
/// `service_end`. Returns `None` when the range would be inverted or overflow.
pub fn descriptor_range(
    value_handle: u16,
    next_decl_handle: Option<u16>,
    service_end: u16,
) -> Option<(u16, u16)> {
    let start = value_handle.checked_add(1)?;
    let end = match next_decl_handle {
        Some(next_decl) => next_decl.checked_sub(1)?,
        None => service_end,
    };
    if start > end {
        return None;
    }
    Some((start, end))
}

/// Pick the CCCD handle from `(handle, 16-bit uuid)` entries.
///
/// Only `0x2902` entries inside the inclusive `start..=end` range qualify; the
/// highest handle wins (legacy assigns every match and never breaks).
pub fn select_cccd(entries: &[(u16, u16)], start: u16, end: u16) -> Option<u16> {
    entries
        .iter()
        .filter(|(handle, uuid)| *uuid == UUID_CCCD && (start..=end).contains(handle))
        .map(|(handle, _)| *handle)
        .max()
}

/// Response opcode expected for a request opcode (notifications/indications and
/// write commands have none).
fn expected_response(request_opcode: u8) -> Option<u8> {
    match request_opcode {
        ATT_EXCHANGE_MTU_REQ => Some(ATT_EXCHANGE_MTU_RSP),
        ATT_FIND_INFO_REQ => Some(ATT_FIND_INFO_RSP),
        ATT_READ_BY_TYPE_REQ => Some(ATT_READ_BY_TYPE_RSP),
        ATT_READ_REQ => Some(ATT_READ_RSP),
        ATT_READ_BY_GROUP_REQ => Some(ATT_READ_BY_GROUP_RSP),
        ATT_WRITE_REQ => Some(ATT_WRITE_RSP),
        _ => None,
    }
}

/// Whether `pdu` may complete a request expecting `expected`.
///
/// Errors always correlate (they carry the failed request opcode); otherwise
/// the opcode must match. Unmapped request opcodes keep the historical
/// lenient behaviour of accepting any non-error reply.
fn is_expected_response(expected: Option<u8>, pdu: &[u8]) -> bool {
    match pdu.first() {
        Some(&ATT_ERROR_RSP) => true,
        Some(&opcode) => expected.map_or(true, |want| opcode == want),
        None => false,
    }
}

pub fn uuid_to_str(raw: &[u8]) -> String {
    if raw.len() == 2 {
        format!("{:04x}", u16::from_le_bytes([raw[0], raw[1]]))
    } else if raw.len() == 16 {
        // Switch 2 UUIDs are 128-bit little-endian in GATT responses
        let mut b = raw.to_vec();
        b.reverse();
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
        )
    } else {
        hex_str(raw)
    }
}

fn hex_str(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Structured ATT error response so callers can distinguish the normal
/// end-of-range `ATTRIBUTE_NOT_FOUND` from a real failure.
#[derive(Debug)]
struct AttRequestError {
    code: u8,
}

impl std::fmt::Display for AttRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ATT Error response code: 0x{:02X}", self.code)
    }
}

impl std::error::Error for AttRequestError {}

fn att_error(code: u8) -> io::Error {
    io::Error::new(io::ErrorKind::Other, AttRequestError { code })
}

/// ATT error code carried by `err`, when the peer answered with `ATT_ERROR_RSP`.
pub fn att_error_code(err: &io::Error) -> Option<u8> {
    err.get_ref()
        .and_then(|inner| inner.downcast_ref::<AttRequestError>())
        .map(|att| att.code)
}

pub type NotificationHandler = Box<dyn Fn(u16, &[u8]) + Send + Sync + 'static>;
pub type DisconnectHandler = Box<dyn Fn() + Send + Sync + 'static>;

pub struct AttClient {
    socket: Arc<L2capSocket>,
    running: Arc<AtomicBool>,
    req_lock: Mutex<()>,
    resp_state: Arc<(Mutex<Option<Vec<u8>>>, Condvar)>,
    notification_cb: Arc<Mutex<Option<NotificationHandler>>>,
    disconnect_cb: Arc<Mutex<Option<DisconnectHandler>>>,
}

impl AttClient {
    pub fn new(socket: L2capSocket) -> Self {
        let sock = Arc::new(socket);
        let running = Arc::new(AtomicBool::new(true));
        let resp_state = Arc::new((Mutex::new(None), Condvar::new()));
        let notification_cb: Arc<Mutex<Option<NotificationHandler>>> = Arc::new(Mutex::new(None));
        let disconnect_cb: Arc<Mutex<Option<DisconnectHandler>>> = Arc::new(Mutex::new(None));

        let client = Self {
            socket: sock.clone(),
            running: running.clone(),
            req_lock: Mutex::new(()),
            resp_state: resp_state.clone(),
            notification_cb: notification_cb.clone(),
            disconnect_cb: disconnect_cb.clone(),
        };

        // Spawn background reader thread
        thread::Builder::new()
            .name("att-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 512];
                while running.load(Ordering::Relaxed) {
                    match sock.recv(&mut buf) {
                        Ok(n) if n > 0 => {
                            let pdu = &buf[..n];
                            match pdu[0] {
                                ATT_HANDLE_VALUE_NTF if n >= 3 => {
                                    let handle = u16::from_le_bytes([pdu[1], pdu[2]]);
                                    let data = &pdu[3..];
                                    if let Some(cb) = notification_cb.lock().unwrap().as_ref() {
                                        cb(handle, data);
                                    }
                                }
                                ATT_HANDLE_VALUE_IND if n >= 3 => {
                                    // Confirmations are sent from the reader thread so the
                                    // peer can release its pending indication immediately.
                                    if let Err(err) = sock.send(&[ATT_HANDLE_VALUE_CFM]) {
                                        debug!("Failed to confirm ATT indication: {}", err);
                                    }
                                    let handle = u16::from_le_bytes([pdu[1], pdu[2]]);
                                    let data = &pdu[3..];
                                    if let Some(cb) = notification_cb.lock().unwrap().as_ref() {
                                        cb(handle, data);
                                    }
                                }
                                _ => {
                                    // Request response
                                    let (lock, cvar) = &*resp_state;
                                    let mut resp = lock.lock().unwrap();
                                    *resp = Some(pdu.to_vec());
                                    cvar.notify_one();
                                }
                            }
                        }
                        _ => {
                            // Disconnected or read error
                            break;
                        }
                    }
                }

                running.store(false, Ordering::Relaxed);
                if let Some(cb) = disconnect_cb.lock().unwrap().as_ref() {
                    cb();
                }
            })
            .expect("Failed to spawn att-reader thread");

        client
    }

    pub fn set_notification_handler<F>(&self, handler: F)
    where
        F: Fn(u16, &[u8]) + Send + Sync + 'static,
    {
        *self.notification_cb.lock().unwrap() = Some(Box::new(handler));
    }

    pub fn set_disconnect_handler<F>(&self, handler: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.disconnect_cb.lock().unwrap() = Some(Box::new(handler));
    }

    pub fn is_connected(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.socket.shutdown();
    }

    /// Perform a synchronous request/response ATT exchange.
    ///
    /// A reply is accepted only when it is an ATT error or matches the response
    /// opcode for the request. Anything else received while waiting is dropped
    /// and the wait continues on the original deadline.
    pub fn request(&self, req_pdu: &[u8], timeout: Duration) -> std::io::Result<Vec<u8>> {
        let _guard = self.req_lock.lock().unwrap();
        let (lock, cvar) = &*self.resp_state;

        // Clear previous response
        {
            let mut resp = lock.lock().unwrap();
            *resp = None;
        }

        // Send request
        self.socket.send(req_pdu)?;

        let expected = req_pdu
            .first()
            .and_then(|opcode| expected_response(*opcode));
        let deadline = Instant::now() + timeout;
        let timeout_err =
            || std::io::Error::new(std::io::ErrorKind::TimedOut, "ATT request timed out");

        let mut resp_guard = lock.lock().unwrap();
        loop {
            match resp_guard.as_ref() {
                Some(pdu) if is_expected_response(expected, pdu) => break,
                Some(_) => {
                    // Unrelated PDU: drop it without resetting the deadline.
                    *resp_guard = None;
                }
                None => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(timeout_err());
                    }
                    let (new_guard, wait_res) =
                        cvar.wait_timeout(resp_guard, deadline - now).unwrap();
                    resp_guard = new_guard;
                    if wait_res.timed_out() && resp_guard.is_none() {
                        return Err(timeout_err());
                    }
                }
            }
        }

        let resp = resp_guard.take().unwrap_or_default();
        if !resp.is_empty() && resp[0] == ATT_ERROR_RSP {
            let code = if resp.len() >= 5 { resp[4] } else { 0 };
            return Err(att_error(code));
        }

        Ok(resp)
    }

    /// Negotiate ATT MTU size (Switch 2 prefers 247 bytes).
    pub fn exchange_mtu(&self, client_mtu: u16) -> std::io::Result<u16> {
        let mut req = [0u8; 3];
        req[0] = ATT_EXCHANGE_MTU_REQ;
        req[1..3].copy_from_slice(&client_mtu.to_le_bytes());

        let resp = self.request(&req, Duration::from_secs(3))?;
        if resp.len() >= 3 && resp[0] == ATT_EXCHANGE_MTU_RSP {
            let server_mtu = u16::from_le_bytes([resp[1], resp[2]]);
            Ok(client_mtu.min(server_mtu))
        } else {
            Ok(23)
        }
    }

    /// Write command without response (lowest latency, for rumble & controller commands).
    pub fn write_command(&self, handle: u16, value: &[u8]) -> std::io::Result<()> {
        let mut buf = Vec::with_capacity(3 + value.len());
        buf.push(ATT_WRITE_CMD);
        buf.extend_from_slice(&handle.to_le_bytes());
        buf.extend_from_slice(value);
        self.socket.send(&buf)?;
        Ok(())
    }

    /// Write request with response confirmation.
    pub fn write_request(
        &self,
        handle: u16,
        value: &[u8],
        timeout: Duration,
    ) -> std::io::Result<()> {
        let mut buf = Vec::with_capacity(3 + value.len());
        buf.push(ATT_WRITE_REQ);
        buf.extend_from_slice(&handle.to_le_bytes());
        buf.extend_from_slice(value);
        let resp = self.request(&buf, timeout)?;
        if resp.is_empty() || resp[0] != ATT_WRITE_RSP {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Unexpected ATT write response",
            ));
        }
        Ok(())
    }

    /// Subscribe to notifications via characteristic CCCD handle.
    pub fn subscribe(&self, cccd_handle: u16, enable: bool) -> std::io::Result<()> {
        let val: [u8; 2] = if enable { [0x01, 0x00] } else { [0x00, 0x00] };
        self.write_request(cccd_handle, &val, Duration::from_secs(2))
    }

    /// Discover all GATT primary services and their characteristics.
    pub fn discover_all(&self) -> std::io::Result<Vec<Service>> {
        let mut services: Vec<Service> = Vec::new();
        let mut start: u16 = 0x0001;

        // 1. Discover Primary Services
        loop {
            let mut req = [0u8; 7];
            req[0] = ATT_READ_BY_GROUP_REQ;
            req[1..3].copy_from_slice(&start.to_le_bytes());
            req[3..5].copy_from_slice(&0xFFFFu16.to_le_bytes());
            req[5..7].copy_from_slice(&UUID_PRIMARY_SERVICE.to_le_bytes());

            let resp = match self.request(&req, Duration::from_secs(2)) {
                Ok(r) => r,
                Err(e) => {
                    // Attribute not found (0x0A) ends discovery
                    debug!("Service discovery finished or returned error: {}", e);
                    break;
                }
            };

            if resp.len() < 2 || resp[0] != ATT_READ_BY_GROUP_RSP {
                break;
            }

            let item_len = resp[1] as usize;
            if item_len < 4 {
                break;
            }

            let mut last_end = 0u16;
            let data = &resp[2..];
            for chunk in data.chunks_exact(item_len) {
                let s_handle = u16::from_le_bytes([chunk[0], chunk[1]]);
                let e_handle = u16::from_le_bytes([chunk[2], chunk[3]]);
                let uuid = uuid_to_str(&chunk[4..]);
                services.push(Service {
                    start_handle: s_handle,
                    end_handle: e_handle,
                    uuid,
                    characteristics: Vec::new(),
                });
                last_end = e_handle;
            }

            if last_end >= 0xFFFF || last_end == 0 {
                break;
            }
            start = match last_end.checked_add(1) {
                Some(next) => next,
                None => break,
            };
        }

        for svc in &services {
            debug!(
                "GATT primary service 0x{:04X}..=0x{:04X} uuid={}",
                svc.start_handle, svc.end_handle, svc.uuid
            );
        }

        // 2. Discover Characteristics for each service
        for svc in &mut services {
            let mut c_start = svc.start_handle;
            while c_start <= svc.end_handle {
                let mut req = [0u8; 7];
                req[0] = ATT_READ_BY_TYPE_REQ;
                req[1..3].copy_from_slice(&c_start.to_le_bytes());
                req[3..5].copy_from_slice(&svc.end_handle.to_le_bytes());
                req[5..7].copy_from_slice(&UUID_CHARACTERISTIC.to_le_bytes());

                let resp = match self.request(&req, Duration::from_secs(2)) {
                    Ok(r) => r,
                    Err(_) => break,
                };

                if resp.len() < 2 || resp[0] != ATT_READ_BY_TYPE_RSP {
                    break;
                }

                let item_len = resp[1] as usize;
                if item_len < 5 {
                    break;
                }

                let mut last_handle = 0u16;
                let data = &resp[2..];
                for chunk in data.chunks_exact(item_len) {
                    let decl_handle = u16::from_le_bytes([chunk[0], chunk[1]]);
                    let properties = chunk[2];
                    let value_handle = u16::from_le_bytes([chunk[3], chunk[4]]);
                    let uuid = uuid_to_str(&chunk[5..]);

                    svc.characteristics.push(Characteristic {
                        decl_handle,
                        properties,
                        value_handle,
                        uuid,
                        cccd_handle: None,
                    });
                    last_handle = decl_handle;
                }

                if last_handle >= svc.end_handle || last_handle == 0 {
                    break;
                }
                c_start = last_handle.saturating_add(1);
            }

            for ch in &svc.characteristics {
                debug!(
                    "GATT characteristic decl=0x{:04X} value=0x{:04X} props=0x{:02X} uuid={}",
                    ch.decl_handle, ch.value_handle, ch.properties, ch.uuid
                );
            }
            debug!(
                "GATT service 0x{:04X}..=0x{:04X} uuid={}: {} characteristic(s)",
                svc.start_handle,
                svc.end_handle,
                svc.uuid,
                svc.characteristics.len()
            );

            // 3. Discover each characteristic's CCCD via ATT_FIND_INFO over its
            // descriptor range. No derivation: an absent CCCD stays None and is
            // only replaced by the legacy safety-net defaults in resolve_handles.
            let plan: Vec<(usize, u16, u16)> = (0..svc.characteristics.len())
                .filter_map(|idx| {
                    let ch = &svc.characteristics[idx];
                    let next_decl = svc.characteristics.get(idx + 1).map(|n| n.decl_handle);
                    descriptor_range(ch.value_handle, next_decl, svc.end_handle)
                        .map(|(start, end)| (idx, start, end))
                })
                .collect();

            for (idx, start, end) in plan {
                let uuid = svc.characteristics[idx].uuid.clone();
                debug!(
                    "CCCD scan characteristic uuid={} value=0x{:04X} descriptors=0x{:04X}..=0x{:04X}",
                    uuid, svc.characteristics[idx].value_handle, start, end
                );
                let entries = self.find_info_scan(start, end);
                let cccd = select_cccd(&entries, start, end);
                debug!(
                    "CCCD scan characteristic uuid={} -> {:?}",
                    uuid, cccd
                );
                svc.characteristics[idx].cccd_handle = cccd;
            }
        }

        Ok(services)
    }

    /// Page through `ATT_FIND_INFO_RSP` replies over `start..=end` and return
    /// `(handle, 16-bit uuid)` entries.
    ///
    /// Stops on ATT errors, non-FIND_INFO replies, malformed/truncated bodies,
    /// or after [`MAX_FIND_INFO_PAGES`] round-trips. 128-bit UUID entries only
    /// advance the page cursor (they can never be a CCCD). Every stop is logged
    /// at debug, including the page-cap case, so a silent truncation can be told
    /// apart from a genuine end of range.
    fn find_info_scan(&self, start: u16, end: u16) -> Vec<(u16, u16)> {
        let mut found = Vec::new();
        let mut handle = start;
        let mut pages = 0usize;

        loop {
            if handle > end {
                debug!(
                    "FIND_INFO 0x{:04X}..=0x{:04X}: range exhausted after {} page(s)",
                    start, end, pages
                );
                break;
            }
            if pages >= MAX_FIND_INFO_PAGES {
                debug!(
                    "FIND_INFO 0x{:04X}..=0x{:04X}: PAGE CAP {} reached with next handle \
                     0x{:04X}; stopping (possible missing descriptors)",
                    start, end, MAX_FIND_INFO_PAGES, handle
                );
                break;
            }
            pages += 1;

            let mut req = [0u8; 5];
            req[0] = ATT_FIND_INFO_REQ;
            req[1..3].copy_from_slice(&handle.to_le_bytes());
            req[3..5].copy_from_slice(&end.to_le_bytes());

            debug!(
                "FIND_INFO page {}: request 0x{:04X}..=0x{:04X}",
                pages, handle, end
            );

            let resp = match self.request(&req, Duration::from_secs(2)) {
                Ok(resp) => resp,
                Err(err) => {
                    match att_error_code(&err) {
                        Some(ATT_ERR_ATTRIBUTE_NOT_FOUND) => debug!(
                            "FIND_INFO page {}: 0x{:04X}..=0x{:04X} returned \
                             ATTRIBUTE_NOT_FOUND (normal end of range)",
                            pages, handle, end
                        ),
                        Some(code) => debug!(
                            "FIND_INFO page {}: 0x{:04X}..=0x{:04X} stopped on ATT \
                             error 0x{:02X}: {}",
                            pages, handle, end, code, err
                        ),
                        None => debug!(
                            "FIND_INFO page {}: 0x{:04X}..=0x{:04X} request failed: {}",
                            pages, handle, end, err
                        ),
                    }
                    break;
                }
            };

            let entries = match parse_find_info_rsp(&resp) {
                Some(entries) => entries,
                None => {
                    debug!(
                        "FIND_INFO page {}: unexpected payload (len={}): {:02X?}",
                        pages,
                        resp.len(),
                        resp
                    );
                    break;
                }
            };

            debug!(
                "FIND_INFO page {}: {} bytes, {} entries",
                pages,
                resp.len(),
                entries.len()
            );

            let mut last = 0u16;
            for (entry_handle, raw_uuid) in entries {
                let uuid = uuid_to_str(&raw_uuid);
                if raw_uuid.len() == 2 {
                    let uuid16 = u16::from_le_bytes([raw_uuid[0], raw_uuid[1]]);
                    if uuid16 == UUID_CCCD {
                        debug!(
                            "FIND_INFO page {}: 0x2902 CCCD candidate handle=0x{:04X} \
                             (range 0x{:04X}..=0x{:04X})",
                            pages, entry_handle, start, end
                        );
                    }
                    found.push((entry_handle, uuid16));
                }
                debug!(
                    "FIND_INFO page {}: entry handle=0x{:04X} uuid={}",
                    pages, entry_handle, uuid
                );
                last = last.max(entry_handle);
            }

            if last == 0 || last >= end {
                debug!(
                    "FIND_INFO 0x{:04X}..=0x{:04X}: last handle 0x{:04X} ends the range",
                    start, end, last
                );
                break;
            }
            handle = last + 1;
        }

        debug!(
            "FIND_INFO 0x{:04X}..=0x{:04X}: {} 16-bit entries, {} CCCD candidate(s)",
            start,
            end,
            found.len(),
            found.iter().filter(|(_, uuid)| *uuid == UUID_CCCD).count()
        );
        found
    }

    /// Resolve standard Switch 2 handles from discovery, reporting which values
    /// were discovered and which fell back to the hard-coded NSO GameCube
    /// defaults (legacy `_resolve_handles`).
    pub fn resolve_handles_ex(
        services: &[Service],
        pid: u16,
    ) -> (ResolvedHandles, HandleProvenance) {
        use crate::protocol::constants::*;

        let mut map: HashMap<String, (u16, Option<u16>)> = HashMap::new();
        for svc in services {
            for ch in &svc.characteristics {
                map.insert(ch.uuid.clone(), (ch.value_handle, ch.cccd_handle));
            }
        }

        let vib_uuid = vibration_uuid_for(pid);
        debug!(
            "GATT resolve pid=0x{:04X}: looking for vibration uuid {} among {} characteristic(s)",
            pid,
            vib_uuid,
            map.len()
        );
        if !GATT_UUID_LIST_LOGGED.swap(true, Ordering::Relaxed) {
            let uuids: Vec<&str> = services
                .iter()
                .flat_map(|svc| svc.characteristics.iter())
                .map(|ch| ch.uuid.as_str())
                .collect();
            info!("GATT characteristic UUIDs: [{}]", uuids.join(", "));
        }

        let value_handle = |uuid: &str, fallback: u16| -> (u16, bool) {
            match map.get(uuid) {
                Some((value, _)) => (*value, true),
                None => (fallback, false),
            }
        };
        let cccd_handle = |uuid: &str, fallback: u16| -> (u16, bool) {
            match map.get(uuid).and_then(|(_, cccd)| *cccd) {
                Some(handle) => (handle, true),
                None => (fallback, false),
            }
        };

        let mut handles = ResolvedHandles::default();
        let mut provenance = HandleProvenance::default();

        (handles.input_report, provenance.input_report) = value_handle(INPUT_REPORT_UUID, 0x000A);
        (handles.input_report_cccd, provenance.input_report_cccd) =
            cccd_handle(INPUT_REPORT_UUID, 0x000B);
        (handles.command_write, provenance.command_write) =
            value_handle(COMMAND_WRITE_UUID, 0x0014);
        (handles.command_response, provenance.command_response) =
            value_handle(COMMAND_RESPONSE_UUID, 0x001A);
        (
            handles.command_response_cccd,
            provenance.command_response_cccd,
        ) = cccd_handle(COMMAND_RESPONSE_UUID, 0x001B);
        (handles.vibration, provenance.vibration) = value_handle(vib_uuid, 0x0012);

        let mut fallbacks: Vec<String> = Vec::new();
        if !provenance.input_report {
            fallbacks.push(format!("input_report=0x{:04X}", handles.input_report));
        }
        if !provenance.input_report_cccd {
            fallbacks.push(format!(
                "input_report_cccd=0x{:04X}",
                handles.input_report_cccd
            ));
        }
        if !provenance.command_write {
            fallbacks.push(format!("command_write=0x{:04X}", handles.command_write));
        }
        if !provenance.command_response {
            fallbacks.push(format!(
                "command_response=0x{:04X}",
                handles.command_response
            ));
        }
        if !provenance.command_response_cccd {
            fallbacks.push(format!(
                "command_response_cccd=0x{:04X}",
                handles.command_response_cccd
            ));
        }
        if !provenance.vibration {
            fallbacks.push(format!("vibration=0x{:04X}", handles.vibration));
        }

        let cccd_reason = |uuid: &str| -> &'static str {
            match map.get(uuid) {
                Some((_, Some(_))) => "CCCD present",
                Some((_, None)) => "characteristic present but CCCD not found by FIND_INFO",
                None => "characteristic UUID absent",
            }
        };
        let mut reasons: Vec<String> = Vec::new();
        if !provenance.input_report {
            reasons.push(format!("input_report: {} absent", INPUT_REPORT_UUID));
        }
        if !provenance.input_report_cccd {
            reasons.push(format!(
                "input_report_cccd: {}",
                cccd_reason(INPUT_REPORT_UUID)
            ));
        }
        if !provenance.command_write {
            reasons.push(format!("command_write: {} absent", COMMAND_WRITE_UUID));
        }
        if !provenance.command_response {
            reasons.push(format!(
                "command_response: {} absent",
                COMMAND_RESPONSE_UUID
            ));
        }
        if !provenance.command_response_cccd {
            reasons.push(format!(
                "command_response_cccd: {}",
                cccd_reason(COMMAND_RESPONSE_UUID)
            ));
        }
        if !provenance.vibration {
            reasons.push(format!("vibration: {} absent", vib_uuid));
        }
        if !reasons.is_empty() {
            debug!("GATT fallback reasons: {}", reasons.join("; "));
        }
        debug!("GATT handle provenance: {:?}", provenance);

        if fallbacks.is_empty() {
            debug!(
                "GATT handles discovered: input=0x{:04X} input_cccd=0x{:04X} \
                 cmd_write=0x{:04X} cmd_rsp=0x{:04X} cmd_rsp_cccd=0x{:04X} vibration=0x{:04X}",
                handles.input_report,
                handles.input_report_cccd,
                handles.command_write,
                handles.command_response,
                handles.command_response_cccd,
                handles.vibration
            );
        } else {
            // The caller decides whether this pass is provisional or final;
            // only a final fallback deserves WARN (controller.rs).
            debug!(
                "GATT fallback handles: {}",
                fallbacks.join(", ")
            );
        }

        (handles, provenance)
    }

    /// Resolve standard Switch 2 handles from discovery.
    ///
    /// Applies the legacy NSO GameCube defaults for anything discovery did not
    /// find; use [`AttClient::resolve_handles_ex`] to learn which values are
    /// fallbacks.
    pub fn resolve_handles(services: &[Service], pid: u16) -> ResolvedHandles {
        Self::resolve_handles_ex(services, pid).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::constants::{
        COMMAND_RESPONSE_UUID, COMMAND_WRITE_UUID, INPUT_REPORT_UUID, PRO_CONTROLLER2_PID,
    };

    // --- FIND_INFO decoding -------------------------------------------------

    #[test]
    fn find_info_rsp_16bit_uuids() {
        let mut pdu = vec![ATT_FIND_INFO_RSP, 0x01];
        for (handle, uuid) in [(0x0010u16, UUID_CCCD), (0x0020, UUID_CHARACTERISTIC)] {
            pdu.extend_from_slice(&handle.to_le_bytes());
            pdu.extend_from_slice(&uuid.to_le_bytes());
        }

        let entries = parse_find_info_rsp(&pdu).expect("valid format 1 response");
        assert_eq!(
            entries,
            vec![
                (0x0010, UUID_CCCD.to_le_bytes().to_vec()),
                (0x0020, UUID_CHARACTERISTIC.to_le_bytes().to_vec()),
            ]
        );
    }

    #[test]
    fn find_info_rsp_128bit_uuids() {
        let uuid_a = [
            0xd2, 0x7f, 0xdf, 0x09, 0x8f, 0x11, 0x8f, 0x82, 0xad, 0x49, 0xfe, 0x89, 0xbe, 0xe9,
            0x7d, 0xab,
        ];
        let uuid_b = [0x11u8; 16];

        let mut pdu = vec![ATT_FIND_INFO_RSP, 0x02];
        pdu.extend_from_slice(&0x0030u16.to_le_bytes());
        pdu.extend_from_slice(&uuid_a);
        pdu.extend_from_slice(&0x0032u16.to_le_bytes());
        pdu.extend_from_slice(&uuid_b);

        let entries = parse_find_info_rsp(&pdu).expect("valid format 2 response");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], (0x0030, uuid_a.to_vec()));
        assert_eq!(entries[1], (0x0032, uuid_b.to_vec()));
    }

    #[test]
    fn find_info_rsp_rejects_bad_format_and_truncation() {
        assert_eq!(parse_find_info_rsp(&[]), None);
        assert_eq!(parse_find_info_rsp(&[ATT_FIND_INFO_RSP]), None);
        // Wrong opcode (request instead of response).
        assert_eq!(
            parse_find_info_rsp(&[ATT_FIND_INFO_REQ, 0x01, 0x10, 0x00, 0x02, 0x29]),
            None
        );
        // Unknown format.
        assert_eq!(
            parse_find_info_rsp(&[ATT_FIND_INFO_RSP, 0x03, 0x10, 0x00, 0x02, 0x29]),
            None
        );
        // Empty body.
        assert_eq!(parse_find_info_rsp(&[ATT_FIND_INFO_RSP, 0x01]), None);
        // 16-bit format, body not a multiple of 4.
        assert_eq!(
            parse_find_info_rsp(&[ATT_FIND_INFO_RSP, 0x01, 0x10, 0x00, 0x02]),
            None
        );
        // 128-bit format, body not a multiple of 18.
        let mut truncated = vec![ATT_FIND_INFO_RSP, 0x02];
        truncated.extend_from_slice(&[0xAA; 17]);
        assert_eq!(parse_find_info_rsp(&truncated), None);
    }

    #[test]
    fn select_cccd_last_wins() {
        let entries = [
            (0x0010, UUID_CHARACTERISTIC),
            (0x0011, UUID_CCCD),
            (0x0012, 0x2901),
            (0x0015, UUID_CCCD),
            (0x0019, UUID_CCCD),
        ];
        assert_eq!(select_cccd(&entries, 0x0010, 0x0020), Some(0x0019));
        // Narrowing the range to a single match still picks that one.
        assert_eq!(select_cccd(&entries, 0x0010, 0x0015), Some(0x0015));
    }

    #[test]
    fn select_cccd_out_of_range_returns_none() {
        let entries = [(0x000F, UUID_CCCD), (0x0021, UUID_CCCD)];
        assert_eq!(select_cccd(&entries, 0x0010, 0x0020), None);
        // Inverted range contains nothing.
        assert_eq!(select_cccd(&[(0x0015, UUID_CCCD)], 0x0020, 0x0010), None);
        assert_eq!(select_cccd(&[], 0x0010, 0x0020), None);
    }

    #[test]
    fn select_cccd_ignores_non_cccd() {
        let entries = [
            (0x0011, 0x2901), // user description
            (0x0012, 0x2904), // presentation format
            (0x0013, UUID_CHARACTERISTIC),
        ];
        assert_eq!(select_cccd(&entries, 0x0010, 0x0020), None);
    }

    #[test]
    fn descriptor_range_uses_next_decl() {
        // value_handle 0x0010, next characteristic declared at 0x0020.
        assert_eq!(
            descriptor_range(0x0010, Some(0x0020), 0x00FF),
            Some((0x0011, 0x001F))
        );
    }

    #[test]
    fn descriptor_range_last_char_uses_service_end() {
        assert_eq!(
            descriptor_range(0x0010, None, 0x0020),
            Some((0x0011, 0x0020))
        );
        // A last characteristic whose value_handle is the service end has no
        // descriptor space at all.
        assert_eq!(descriptor_range(0x0020, None, 0x0020), None);
    }

    #[test]
    fn descriptor_range_inverted_is_none() {
        // No descriptor gap: next characteristic is immediately after value.
        assert_eq!(descriptor_range(0x0010, Some(0x0011), 0x0020), None);
        // value_handle == service_end.
        assert_eq!(descriptor_range(0x0020, None, 0x0020), None);
    }

    #[test]
    fn descriptor_range_overflow_safe() {
        // value_handle + 1 overflows u16.
        assert_eq!(descriptor_range(u16::MAX, None, u16::MAX), None);
        assert_eq!(descriptor_range(u16::MAX, Some(0x0002), u16::MAX), None);
        // next_decl_handle - 1 underflows u16.
        assert_eq!(descriptor_range(0x0010, Some(0x0000), 0x00FF), None);
    }

    // --- handle resolution --------------------------------------------------

    fn service(chars: &[(&str, u16, Option<u16>)]) -> Service {
        Service {
            start_handle: 0x0001,
            end_handle: 0xFFFF,
            uuid: "service".to_string(),
            characteristics: chars
                .iter()
                .map(|(uuid, value_handle, cccd_handle)| Characteristic {
                    decl_handle: value_handle.saturating_sub(1),
                    properties: 0x10,
                    value_handle: *value_handle,
                    uuid: (*uuid).to_string(),
                    cccd_handle: *cccd_handle,
                })
                .collect(),
        }
    }

    const GC_DEFAULTS: ResolvedHandles = ResolvedHandles {
        input_report: 0x000A,
        input_report_cccd: 0x000B,
        command_write: 0x0014,
        command_response: 0x001A,
        command_response_cccd: 0x001B,
        vibration: 0x0012,
    };

    #[test]
    fn resolve_handles_defaults_when_empty() {
        let (handles, provenance) = AttClient::resolve_handles_ex(&[], PRO_CONTROLLER2_PID);
        assert_eq!(handles, GC_DEFAULTS);
        assert_eq!(provenance, HandleProvenance::default());
        assert!(!provenance.input_report);
        assert!(!provenance.input_report_cccd);
        assert!(!provenance.command_write);
        assert!(!provenance.command_response);
        assert!(!provenance.command_response_cccd);
        assert!(!provenance.vibration);
        // The legacy wrapper keeps the same defaults.
        assert_eq!(
            AttClient::resolve_handles(&[], PRO_CONTROLLER2_PID),
            GC_DEFAULTS
        );
    }

    #[test]
    fn resolve_handles_uses_discovered_handles() {
        let vibration_uuid = crate::protocol::constants::vibration_uuid_for(PRO_CONTROLLER2_PID);
        let services = [service(&[
            (INPUT_REPORT_UUID, 0x0030, Some(0x0031)),
            (COMMAND_WRITE_UUID, 0x0050, None),
            (COMMAND_RESPONSE_UUID, 0x0052, Some(0x0053)),
            (vibration_uuid, 0x0070, None),
        ])];

        let (handles, provenance) = AttClient::resolve_handles_ex(&services, PRO_CONTROLLER2_PID);
        assert_eq!(
            handles,
            ResolvedHandles {
                input_report: 0x0030,
                input_report_cccd: 0x0031,
                command_write: 0x0050,
                command_response: 0x0052,
                command_response_cccd: 0x0053,
                vibration: 0x0070,
            }
        );
        assert_eq!(
            provenance,
            HandleProvenance {
                input_report: true,
                input_report_cccd: true,
                command_write: true,
                command_response: true,
                command_response_cccd: true,
                vibration: true,
            }
        );
        assert_eq!(
            AttClient::resolve_handles(&services, PRO_CONTROLLER2_PID),
            handles
        );
    }

    #[test]
    fn resolve_handles_mixed_keeps_defaults() {
        let vibration_uuid = crate::protocol::constants::vibration_uuid_for(PRO_CONTROLLER2_PID);
        // Input report value is discovered but its CCCD is not; vibration is
        // discovered; the command characteristics are entirely missing.
        let services = [service(&[
            (INPUT_REPORT_UUID, 0x0030, None),
            (vibration_uuid, 0x0070, None),
        ])];

        let (handles, provenance) = AttClient::resolve_handles_ex(&services, PRO_CONTROLLER2_PID);
        assert_eq!(
            handles,
            ResolvedHandles {
                input_report: 0x0030,
                input_report_cccd: 0x000B,
                command_write: 0x0014,
                command_response: 0x001A,
                command_response_cccd: 0x001B,
                vibration: 0x0070,
            }
        );
        assert_eq!(
            provenance,
            HandleProvenance {
                input_report: true,
                input_report_cccd: false,
                command_write: false,
                command_response: false,
                command_response_cccd: false,
                vibration: true,
            }
        );
    }

    // --- request correlation ------------------------------------------------

    #[test]
    fn response_opcode_correlation() {
        assert_eq!(
            expected_response(ATT_EXCHANGE_MTU_REQ),
            Some(ATT_EXCHANGE_MTU_RSP)
        );
        assert_eq!(
            expected_response(ATT_FIND_INFO_REQ),
            Some(ATT_FIND_INFO_RSP)
        );
        assert_eq!(
            expected_response(ATT_READ_BY_TYPE_REQ),
            Some(ATT_READ_BY_TYPE_RSP)
        );
        assert_eq!(expected_response(ATT_READ_REQ), Some(ATT_READ_RSP));
        assert_eq!(
            expected_response(ATT_READ_BY_GROUP_REQ),
            Some(ATT_READ_BY_GROUP_RSP)
        );
        assert_eq!(expected_response(ATT_WRITE_REQ), Some(ATT_WRITE_RSP));
        assert_eq!(expected_response(ATT_WRITE_CMD), None);
        assert_eq!(expected_response(ATT_HANDLE_VALUE_NTF), None);

        // Matching opcode and errors correlate; everything else is stale.
        assert!(is_expected_response(
            Some(ATT_FIND_INFO_RSP),
            &[ATT_FIND_INFO_RSP, 0x01]
        ));
        assert!(is_expected_response(
            Some(ATT_FIND_INFO_RSP),
            &[ATT_ERROR_RSP, 0x04, 0x10, 0x00, 0x0A]
        ));
        assert!(!is_expected_response(
            Some(ATT_FIND_INFO_RSP),
            &[ATT_READ_RSP, 0x00]
        ));
        assert!(!is_expected_response(
            Some(ATT_FIND_INFO_RSP),
            &[ATT_HANDLE_VALUE_NTF, 0x00, 0x00, 0x00]
        ));
        assert!(!is_expected_response(Some(ATT_FIND_INFO_RSP), &[]));
        // Unmapped request: any non-error reply is accepted (legacy leniency).
        assert!(is_expected_response(None, &[ATT_READ_RSP, 0x00]));
    }

    #[test]
    fn att_error_code_distinguishes_not_found() {
        let not_found = att_error(ATT_ERR_ATTRIBUTE_NOT_FOUND);
        assert_eq!(
            att_error_code(&not_found),
            Some(ATT_ERR_ATTRIBUTE_NOT_FOUND)
        );
        assert_eq!(not_found.kind(), io::ErrorKind::Other);
        assert!(not_found.to_string().contains("0x0A"));
        assert_eq!(att_error_code(&att_error(0x0E)), Some(0x0E));
        // Transport/parse errors carry no ATT code.
        assert_eq!(att_error_code(&io::Error::other("boom")), None);
    }
}
