//! ATT (Attribute Protocol) client over raw L2CAP socket for Switch 2 controllers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use tracing::debug;

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
pub const ATT_WRITE_CMD: u8 = 0x52;

pub const UUID_PRIMARY_SERVICE: u16 = 0x2800;
pub const UUID_CHARACTERISTIC: u16 = 0x2803;
pub const UUID_CCCD: u16 = 0x2902;

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

#[derive(Debug, Clone, Default)]
pub struct ResolvedHandles {
    pub input_report: u16,
    pub input_report_cccd: u16,
    pub command_write: u16,
    pub command_response: u16,
    pub command_response_cccd: u16,
    pub vibration: u16,
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
                            let opcode = pdu[0];
                            if opcode == ATT_HANDLE_VALUE_NTF && n >= 3 {
                                let handle = u16::from_le_bytes([pdu[1], pdu[2]]);
                                let data = &pdu[3..];
                                if let Some(cb) = notification_cb.lock().unwrap().as_ref() {
                                    cb(handle, data);
                                }
                            } else {
                                // Request response
                                let (lock, cvar) = &*resp_state;
                                let mut resp = lock.lock().unwrap();
                                *resp = Some(pdu.to_vec());
                                cvar.notify_one();
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

        // Wait for response with timeout
        let mut resp_guard = lock.lock().unwrap();
        while resp_guard.is_none() {
            let (new_guard, wait_res) = cvar.wait_timeout(resp_guard, timeout).unwrap();
            resp_guard = new_guard;
            if wait_res.timed_out() && resp_guard.is_none() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "ATT request timed out",
                ));
            }
        }

        let resp = resp_guard.take().unwrap();
        if !resp.is_empty() && resp[0] == ATT_ERROR_RSP {
            let code = if resp.len() >= 5 { resp[4] } else { 0 };
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("ATT Error response code: 0x{:02X}", code),
            ));
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
    pub fn write_request(&self, handle: u16, value: &[u8], timeout: Duration) -> std::io::Result<()> {
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

            // Fill CCCD handles (typically value_handle + 1)
            for ch in &mut svc.characteristics {
                if (ch.properties & 0x10) != 0 || (ch.properties & 0x20) != 0 {
                    // Characteristic supports notifications or indications
                    ch.cccd_handle = Some(ch.value_handle + 1);
                }
            }
        }

        Ok(services)
    }

    /// Resolve standard Switch 2 handles from discovery.
    pub fn resolve_handles(services: &[Service], pid: u16) -> ResolvedHandles {
        use crate::protocol::constants::*;

        let mut map: HashMap<String, (u16, Option<u16>)> = HashMap::new();
        for svc in services {
            for ch in &svc.characteristics {
                map.insert(ch.uuid.clone(), (ch.value_handle, ch.cccd_handle));
            }
        }

        let vib_uuid = vibration_uuid_for(pid);

        ResolvedHandles {
            input_report: map.get(INPUT_REPORT_UUID).map(|x| x.0).unwrap_or(0x000A),
            input_report_cccd: map.get(INPUT_REPORT_UUID).and_then(|x| x.1).unwrap_or(0x000B),
            command_write: map.get(COMMAND_WRITE_UUID).map(|x| x.0).unwrap_or(0x0014),
            command_response: map.get(COMMAND_RESPONSE_UUID).map(|x| x.0).unwrap_or(0x001A),
            command_response_cccd: map.get(COMMAND_RESPONSE_UUID).and_then(|x| x.1).unwrap_or(0x001B),
            vibration: map.get(vib_uuid).map(|x| x.0).unwrap_or(0x0012),
        }
    }
}
