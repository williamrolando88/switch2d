//! Raw `/dev/uinput` layer: ioctl ABI definitions and event writes.
//!
//! `evdev` 0.12 has no `physical_path()` on its uinput builder, but the
//! `phys="ngc/<MAC>"` contract is required by `70-ngc-imu-uaccess.rules` and by
//! `ngc_evdev_uniq_shim.c` (which falls back to `phys` when `EVIOCGUNIQ` is
//! absent). This module therefore talks to the kernel directly.
//!
//! **All `unsafe` in the uinput layer lives here.** Every block carries a
//! `// SAFETY:` comment naming the invariant it relies on.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;

use evdev::{AbsoluteAxisType, EventType, Key};

// ---------------------------------------------------------------------------
// ioctl encoding (asm-generic/ioctl.h)
// ---------------------------------------------------------------------------

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_WRITE: u32 = 1;

/// `_IOW`-style request number. `UINPUT_IOCTL_BASE` is `0x55`.
const fn iow(nr: u32, size: usize) -> libc::c_ulong {
    ((IOC_WRITE << IOC_DIRSHIFT)
        | ((size as u32) << IOC_SIZESHIFT)
        | ((0x55u32) << IOC_TYPESHIFT)
        | (nr << IOC_NRSHIFT)) as libc::c_ulong
}

/// `_IO`-style request number (no direction, no size).
const fn ioc_none(nr: u32) -> libc::c_ulong {
    ((0x55u32) << IOC_TYPESHIFT) as libc::c_ulong | nr as libc::c_ulong
}

pub const UI_DEV_CREATE: libc::c_ulong = ioc_none(1);
pub const UI_DEV_DESTROY: libc::c_ulong = iow(2, size_of::<libc::c_int>());
pub const UI_DEV_SETUP: libc::c_ulong = iow(3, size_of::<UinputSetup>());
pub const UI_ABS_SETUP: libc::c_ulong = iow(4, size_of::<UinputAbsSetup>());
pub const UI_SET_EVBIT: libc::c_ulong = iow(100, size_of::<libc::c_int>());
pub const UI_SET_KEYBIT: libc::c_ulong = iow(101, size_of::<libc::c_int>());
pub const UI_SET_ABSBIT: libc::c_ulong = iow(103, size_of::<libc::c_int>());
pub const UI_SET_FFBIT: libc::c_ulong = iow(107, size_of::<libc::c_int>());
pub const UI_SET_PHYS: libc::c_ulong = iow(108, size_of::<*const u8>());
pub const UI_SET_PROPBIT: libc::c_ulong = iow(110, size_of::<libc::c_int>());

// ---------------------------------------------------------------------------
// Kernel ABI structs
// ---------------------------------------------------------------------------

/// `struct input_event` from `<linux/input.h>`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RawInputEvent {
    pub time: libc::timeval,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl RawInputEvent {
    pub fn new(kind: u16, code: u16, value: i32) -> Self {
        Self {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            kind,
            code,
            value,
        }
    }
}

/// `struct input_id`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

/// `struct input_absinfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

impl AbsInfo {
    /// Same six-field shape as `evdev::AbsInfo::new`, for drop-in call sites.
    pub const fn new(
        value: i32,
        minimum: i32,
        maximum: i32,
        fuzz: i32,
        flat: i32,
        resolution: i32,
    ) -> Self {
        Self {
            value,
            minimum,
            maximum,
            fuzz,
            flat,
            resolution,
        }
    }
}

/// `struct uinput_setup` (`UINPUT_MAX_NAME_SIZE` = 80).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UinputSetup {
    pub id: InputId,
    pub name: [u8; 80],
    pub ff_effects_max: u32,
}

impl Default for UinputSetup {
    fn default() -> Self {
        Self {
            id: InputId::default(),
            name: [0u8; 80],
            ff_effects_max: 0,
        }
    }
}

/// `struct uinput_abs_setup`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct UinputAbsSetup {
    pub code: u16,
    pub _pad: u16,
    pub absinfo: AbsInfo,
}

// ---------------------------------------------------------------------------
// ioctl wrappers
// ---------------------------------------------------------------------------

fn ioctl_int(fd: &OwnedFd, request: libc::c_ulong, value: libc::c_int) -> io::Result<()> {
    // SAFETY: `fd` is an open uinput file descriptor and `UI_SET_*` take a
    // by-value `int` exactly as encoded in `request` (`_IOW(..., int)`).
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), request, value) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn ioctl_ptr<T>(fd: &OwnedFd, request: libc::c_ulong, value: *const T) -> io::Result<()> {
    // SAFETY: the caller passes a pointer to a live `repr(C)` value whose size
    // matches the size encoded in `request`; the kernel only reads it for the
    // duration of the call.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), request, value) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn ioctl_none(fd: &OwnedFd, request: libc::c_ulong) -> io::Result<()> {
    // SAFETY: `fd` is an open uinput descriptor; `_IO` requests take no
    // argument and the kernel ignores whatever vararg slot is unused.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), request) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// Owned `/dev/uinput` handle.
///
/// Dropping it destroys the virtual device (best effort).
#[derive(Debug)]
pub struct RawUinput {
    fd: OwnedFd,
}

impl RawUinput {
    /// Open `/dev/uinput` (falling back to `/dev/input/uinput`).
    pub fn open() -> io::Result<Self> {
        let mut last_error = None;
        for path in ["/dev/uinput", "/dev/input/uinput"] {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(path)
            {
                Ok(file) => {
                    // `File -> OwnedFd` keeps the fd unique for the lifetime of
                    // the handle, so the ioctl/write paths can never use a
                    // stale descriptor.
                    return Ok(Self { fd: file.into() });
                }
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no uinput device available")
        }))
    }

    pub fn set_evbit(&self, event_type: EventType) -> io::Result<()> {
        ioctl_int(&self.fd, UI_SET_EVBIT, libc::c_int::from(event_type.0))
    }

    pub fn set_keybit(&self, key: Key) -> io::Result<()> {
        ioctl_int(&self.fd, UI_SET_KEYBIT, libc::c_int::from(key.0))
    }

    pub fn set_absbit(&self, axis: AbsoluteAxisType) -> io::Result<()> {
        ioctl_int(&self.fd, UI_SET_ABSBIT, libc::c_int::from(axis.0))
    }

    pub fn set_ffbit(&self, code: u16) -> io::Result<()> {
        ioctl_int(&self.fd, UI_SET_FFBIT, libc::c_int::from(code))
    }

    pub fn set_propbit(&self, code: u16) -> io::Result<()> {
        ioctl_int(&self.fd, UI_SET_PROPBIT, libc::c_int::from(code))
    }

    /// Configure one absolute axis (kernel ≥ 4.5 `UI_ABS_SETUP`).
    pub fn abs_setup(&self, axis: AbsoluteAxisType, absinfo: AbsInfo) -> io::Result<()> {
        let setup = UinputAbsSetup {
            code: axis.0,
            _pad: 0,
            absinfo,
        };
        ioctl_ptr(&self.fd, UI_ABS_SETUP, &setup)
    }

    /// `UI_DEV_SETUP`: bus/vendor/product/version, name and FF slots.
    pub fn device_setup(&self, id: &InputId, name: &str, ff_effects_max: u32) -> io::Result<()> {
        if name.as_bytes().contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "uinput device name contains an interior NUL",
            ));
        }

        let mut setup = UinputSetup {
            id: *id,
            ff_effects_max,
            ..UinputSetup::default()
        };
        // Truncate to 79 bytes, leaving room for the NUL terminator.
        let bytes = name.as_bytes();
        let take = bytes.len().min(setup.name.len() - 1);
        setup.name[..take].copy_from_slice(&bytes[..take]);

        ioctl_ptr(&self.fd, UI_DEV_SETUP, &setup)
    }

    /// `UI_SET_PHYS`: the `phys` string udev and `EVIOCGPHYS` readers see.
    pub fn set_phys(&self, phys: &str) -> io::Result<()> {
        let c_string = CString::new(phys).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "phys string contains an interior NUL",
            )
        })?;

        ioctl_ptr(&self.fd, UI_SET_PHYS, c_string.as_ptr())
    }

    /// Register the configured device with the input subsystem.
    pub fn create(&self) -> io::Result<()> {
        ioctl_none(&self.fd, UI_DEV_CREATE)
    }

    /// Write a batch of input events with one `write(2)` per remaining chunk.
    ///
    /// Short writes and `EINTR` retry the remainder; an empty slice is a no-op.
    pub fn emit(&self, events: &[RawInputEvent]) -> io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let total = size_of::<RawInputEvent>() * events.len();
        // SAFETY: `RawInputEvent` is `repr(C)` and plain-old-data (timeval of
        // integers plus integer fields), so viewing initialized events as bytes
        // for a write(2) is valid; the slice keeps them alive for the call.
        let bytes = unsafe { std::slice::from_raw_parts(events.as_ptr().cast::<u8>(), total) };

        let mut written = 0usize;
        while written < bytes.len() {
            let chunk = &bytes[written..];
            // SAFETY: `self.fd` is an open uinput descriptor and `chunk` points
            // to `chunk.len()` readable bytes; write(2) only reads from it.
            let rc = unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    chunk.as_ptr().cast::<libc::c_void>(),
                    chunk.len(),
                )
            };

            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if rc == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            written += rc as usize;
        }

        Ok(())
    }

    /// Tear the device down. Safe to call more than once.
    pub fn destroy(&self) -> io::Result<()> {
        ioctl_none(&self.fd, UI_DEV_DESTROY)
    }
}

impl Drop for RawUinput {
    fn drop(&mut self) {
        // Best effort: a failed destroy must never abort teardown.
        let _ = self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_linux_abi() {
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
        assert_eq!(UI_DEV_DESTROY, 0x4004_5502);
        assert_eq!(UI_ABS_SETUP, 0x401c_5504);
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_ABSBIT, 0x4004_5567);
        assert_eq!(UI_SET_PHYS, 0x4008_556c);
        assert_eq!(UI_SET_PROPBIT, 0x4004_556e);
        // Not in the pinned list above, but same encoding rule.
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_SET_FFBIT, 0x4004_556b);
        assert_eq!(UI_DEV_CREATE, 0x5501);
    }

    #[test]
    fn raw_input_event_size_matches_kernel_abi() {
        // struct input_event = timeval + u16 + u16 + i32 (+ tail padding to
        // the timeval alignment), i.e. 24 bytes with a 16-byte timeval here and
        // 16 bytes where time_t is 32-bit.
        assert_eq!(
            size_of::<RawInputEvent>(),
            size_of::<libc::timeval>() + 8,
            "struct input_event is timeval + u16 + u16 + i32"
        );
        assert_eq!(std::mem::offset_of!(RawInputEvent, time), 0);
        assert_eq!(
            std::mem::offset_of!(RawInputEvent, kind),
            size_of::<libc::timeval>()
        );
        assert_eq!(
            std::mem::offset_of!(RawInputEvent, code),
            size_of::<libc::timeval>() + 2
        );
        assert_eq!(
            std::mem::offset_of!(RawInputEvent, value),
            size_of::<libc::timeval>() + 4
        );
        assert_eq!(size_of::<AbsInfo>(), 6 * size_of::<i32>());
        assert_eq!(size_of::<InputId>(), 4 * size_of::<u16>());
        assert_eq!(std::mem::offset_of!(UinputAbsSetup, absinfo), 4);
    }
}
