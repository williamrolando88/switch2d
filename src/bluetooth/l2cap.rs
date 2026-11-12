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
    pub fn connect_timeout(
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

    /// Try connecting with LE_PUBLIC, then fallback to LE_RANDOM.
    pub fn connect_le(
        &self,
        dest_mac_le: [u8; 6],
        timeout: Duration,
    ) -> std::io::Result<u8> {
        let per_try = timeout / 2;
        debug!("Attempting L2CAP connect with BDADDR_LE_PUBLIC...");
        match self.connect_timeout(dest_mac_le, BDADDR_LE_PUBLIC, per_try) {
            Ok(()) => Ok(BDADDR_LE_PUBLIC),
            Err(e_pub) => {
                debug!("LE_PUBLIC connect failed ({}), trying BDADDR_LE_RANDOM...", e_pub);
                match self.connect_timeout(dest_mac_le, BDADDR_LE_RANDOM, per_try) {
                    Ok(()) => Ok(BDADDR_LE_RANDOM),
                    Err(e_rnd) => Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        format!("L2CAP connect failed (pub: {}, rnd: {})", e_pub, e_rnd),
                    )),
                }
            }
        }
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
