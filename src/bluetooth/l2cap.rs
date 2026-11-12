//! Low-level Linux raw L2CAP Bluetooth socket handling (bypassing BlueZ GATT).

use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;
use tracing::debug;

pub const AF_BLUETOOTH: libc::c_int = 31;
pub const BTPROTO_L2CAP: libc::c_int = 0;
pub const SOL_BLUETOOTH: libc::c_int = 274;
pub const BT_SECURITY: libc::c_int = 4;
pub const BT_SECURITY_LOW: u8 = 1;
pub const ATT_CID: u16 = 4;

pub const BDADDR_LE_PUBLIC: u8 = 1;
pub const BDADDR_LE_RANDOM: u8 = 2;

/// Address types in legacy attempt order: public first, then random
/// (`bridge.py:436-474`).
pub const ADDRESS_TYPES: [u8; 2] = [BDADDR_LE_PUBLIC, BDADDR_LE_RANDOM];

/// The ordered address types a connect attempt walks.
pub fn address_type_order() -> [u8; 2] {
    ADDRESS_TYPES
}

/// Human-readable address type for diagnostics.
pub fn address_type_name(addr_type: u8) -> &'static str {
    match addr_type {
        BDADDR_LE_PUBLIC => "LE_PUBLIC",
        BDADDR_LE_RANDOM => "LE_RANDOM",
        _ => "unknown",
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SockaddrL2 {
    pub l2_family: u16,
    pub l2_psm: u16,
    pub l2_bdaddr: [u8; 6],
    pub l2_cid: u16,
    pub l2_bdaddr_type: u8,
    pub _pad: u8,
}

impl SockaddrL2 {
    pub fn new(bdaddr: [u8; 6], cid: u16, bdaddr_type: u8) -> Self {
        Self {
            l2_family: AF_BLUETOOTH as u16,
            l2_psm: 0,
            l2_bdaddr: bdaddr,
            l2_cid: cid,
            l2_bdaddr_type: bdaddr_type,
            _pad: 0,
        }
    }
}

/// `L2CAP connect failed (LE_PUBLIC: <err>; LE_RANDOM: <err>)`.
///
/// Kept as a pure function so the diagnostic contract is unit-testable without
/// a Bluetooth adapter.
fn format_connect_failure(failures: &[(u8, String)]) -> String {
    let detail = failures
        .iter()
        .map(|(addr_type, err)| format!("{}: {}", address_type_name(*addr_type), err))
        .collect::<Vec<_>>()
        .join("; ");
    format!("L2CAP connect failed ({detail})")
}

pub struct L2capSocket {
    fd: OwnedFd,
}

impl L2capSocket {
    /// Create a raw L2CAP SEQPACKET socket with BT_SECURITY_LOW.
    pub fn create() -> std::io::Result<Self> {
        let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_SEQPACKET, BTPROTO_L2CAP) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };

        // Set BT_SECURITY_LOW (level = 1, key_size = 0)
        let sec: [u8; 2] = [BT_SECURITY_LOW, 0];
        let ret = unsafe {
            libc::setsockopt(
                owned_fd.as_raw_fd(),
                SOL_BLUETOOTH,
                BT_SECURITY,
                sec.as_ptr() as *const libc::c_void,
                sec.len() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self { fd: owned_fd })
    }

    /// Bind to a local Bluetooth adapter MAC.
    pub fn bind(&self, adapter_mac_le: [u8; 6]) -> std::io::Result<()> {
        let addr = SockaddrL2::new(adapter_mac_le, ATT_CID, BDADDR_LE_PUBLIC);
        let ret = unsafe {
            libc::bind(
                self.fd.as_raw_fd(),
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<SockaddrL2>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Non-blocking connect with timeout to remote device MAC.
    ///
    /// Private on purpose: a failed `connect(2)` poisons the socket (later
    /// attempts fail with `EINVAL`), so the only public entry point is
    /// [`L2capSocket::connect_new`], which builds a fresh socket per attempt.
    fn connect_timeout(
        &self,
        dest_mac_le: [u8; 6],
        dst_type: u8,
        timeout: Duration,
    ) -> std::io::Result<()> {
        let raw_fd = self.fd.as_raw_fd();

        // Put in non-blocking mode
        let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFL, 0) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let addr = SockaddrL2::new(dest_mac_le, ATT_CID, dst_type);
        let ret = unsafe {
            libc::connect(
                raw_fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<SockaddrL2>() as libc::socklen_t,
            )
        };

        if ret != 0 {
            let err = std::io::Error::last_os_error();
            let raw_err = err.raw_os_error().unwrap_or(0);
            if raw_err != libc::EINPROGRESS && raw_err != libc::EALREADY {
                return Err(err);
            }

            // Wait for socket to become writable via poll
            let mut pfd = libc::pollfd {
                fd: raw_fd,
                events: libc::POLLOUT,
                revents: 0,
            };

            let timeout_ms = timeout.as_millis().clamp(100, i32::MAX as u128) as libc::c_int;
            let poll_ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };

            if poll_ret <= 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "L2CAP connection timed out",
                ));
            }

            // Check SO_ERROR
            let mut soerr: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let getsockopt_ret = unsafe {
                libc::getsockopt(
                    raw_fd,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut soerr as *mut _ as *mut libc::c_void,
                    &mut len,
                )
            };
            if getsockopt_ret < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if soerr != 0 {
                return Err(std::io::Error::from_raw_os_error(soerr));
            }
        }

        // Restore blocking mode
        if unsafe { libc::fcntl(raw_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(())
    }

    /// Connect to `dest_mac_le`, trying LE_PUBLIC then LE_RANDOM, with a
    /// **fresh socket (and bind) for every address type**.
    ///
    /// Legacy rebuilt the whole `SwitchController` per attempt
    /// (`bridge.py:436-474`). A socket whose `connect(2)` failed cannot be
    /// reused: Linux returns `EINVAL` immediately on any later connect, so a
    /// retry on the same fd both never reaches the controller and masks the
    /// real error of the first attempt.
    ///
    /// Returns the connected socket plus the address type that worked. On total
    /// failure the error names every attempt and its OS error.
    pub fn connect_new(
        adapter_mac_le: [u8; 6],
        dest_mac_le: [u8; 6],
        timeout: Duration,
    ) -> std::io::Result<(Self, u8)> {
        let per_try = timeout / ADDRESS_TYPES.len() as u32;
        let mut failures: Vec<(u8, String)> = Vec::with_capacity(ADDRESS_TYPES.len());

        for addr_type in ADDRESS_TYPES {
            // Fresh socket per attempt, dropped (closed) on failure: the
            // poisoned fd is never touched again.
            let socket = Self::create()?;
            socket.bind(adapter_mac_le)?;

            debug!(
                "Attempting L2CAP connect (type {}, {:02X?})...",
                address_type_name(addr_type),
                dest_mac_le
            );
            match socket.connect_timeout(dest_mac_le, addr_type, per_try) {
                Ok(()) => return Ok((socket, addr_type)),
                Err(err) => {
                    debug!(
                        "L2CAP connect with {} failed: {}",
                        address_type_name(addr_type),
                        err
                    );
                    failures.push((addr_type, err.to_string()));
                }
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format_connect_failure(&failures),
        ))
    }

    pub fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        let ret = unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
                0,
            )
        };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    pub fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let ret = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    pub fn shutdown(&self) {
        unsafe {
            libc::shutdown(self.fd.as_raw_fd(), libc::SHUT_RDWR);
        }
    }
}

impl AsRawFd for L2capSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_type_order_is_public_then_random() {
        assert_eq!(address_type_order(), [BDADDR_LE_PUBLIC, BDADDR_LE_RANDOM]);
        assert_eq!(address_type_name(BDADDR_LE_PUBLIC), "LE_PUBLIC");
        assert_eq!(address_type_name(BDADDR_LE_RANDOM), "LE_RANDOM");
        assert_eq!(address_type_name(0xFF), "unknown");
    }

    #[test]
    fn connect_failure_error_names_every_attempt() {
        // The exact shape of the hardware failure that blocked the test round:
        // ENOSYS from the kernel's bt_to_errno for a refused LE connect, then
        // EINVAL from the poisoned-fd reuse.
        let text = format_connect_failure(&[
            (
                BDADDR_LE_PUBLIC,
                "Function not implemented (os error 38)".to_string(),
            ),
            (
                BDADDR_LE_RANDOM,
                "Invalid argument (os error 22)".to_string(),
            ),
        ]);

        assert_eq!(
            text,
            concat!(
                "L2CAP connect failed (LE_PUBLIC: Function not implemented (os error 38); ",
                "LE_RANDOM: Invalid argument (os error 22))"
            )
        );
        assert!(text.contains("LE_PUBLIC"));
        assert!(text.contains("LE_RANDOM"));
        assert!(text.contains("os error 38"));
        assert!(text.contains("os error 22"));
    }

    #[test]
    fn only_connect_path_returns_a_fresh_connected_socket() {
        // `connect_new` is the only public connect API: it creates, binds and
        // connects a new socket per address type and hands back the connected
        // one. There is no `&self` connect method left, so a socket whose
        // connect(2) failed can never be retried (Linux would answer EINVAL on
        // every later attempt). This function-pointer assertion pins the
        // signature at compile time.
        let connect: fn([u8; 6], [u8; 6], Duration) -> std::io::Result<(L2capSocket, u8)> =
            L2capSocket::connect_new;
        let _ = connect;
    }
}
