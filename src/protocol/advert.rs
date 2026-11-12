//! BLE advertisement parsing for Switch 2 controllers.
//!
//! BlueZ (and therefore `bleak` / `bluer`) keys the manufacturer-specific AD
//! block by 16-bit company id and strips those two bytes, so the offsets below
//! are relative to the payload for [`NINTENDO_COMPANY_ID`] (`0x0553`):
//!
//! | Range   | Meaning                                            |
//! |---------|----------------------------------------------------|
//! | `[0:3]` | unused prefix                                      |
//! | `[3:5]` | Nintendo VID, `u16` LE — must be `0x057E`          |
//! | `[5:7]` | product id, `u16` LE                               |
//! | `[7:10]`| unused (flags/status)                              |
//! | `[10:16]`| bonded-host MAC, 6 bytes **little-endian**; all   |
//! |         | zero bytes mean the pad is in pairing mode        |
//!
//! # MAC byte-order asymmetry
//!
//! The display form `AA:BB:CC:DD:EE:FF` is the big-endian integer
//! `0xAABBCCDDEEFF`, i.e. bytes `AA BB CC DD EE FF` in order
//! ([`mac_to_int`]). The advertisement field stores the reverse order —
//! `FF EE DD CC BB AA` — which is exactly what
//! [`mac_to_le`](super::command::mac_to_le) /
//! [`host_field_from_adapter`] return. Comparisons are therefore done on the
//! little-endian wire bytes, never on the display string.
//!
//! [`NINTENDO_COMPANY_ID`]: super::constants::NINTENDO_COMPANY_ID

use std::time::Duration;

use super::command::{mac_to_le, parse_mac};
use super::constants::{
    JOYCON2_LEFT_PID, JOYCON2_RIGHT_PID, NINTENDO_VENDOR_ID, NSO_GAMECUBE_PID, PRO_CONTROLLER2_PID,
};

/// Shortest AD block that still carries the VID and PID fields.
const MIN_ADVERT_LEN: usize = 13;
/// Offset of the bonded-host field inside the manufacturer block.
const HOST_FIELD_START: usize = 10;
/// End offset (exclusive) of the bonded-host field.
const HOST_FIELD_END: usize = 16;

/// How long a wake (button-press) advertisement stays "live".
pub const WAKE_TTL: Duration = Duration::from_secs(4);
/// How long a pairing (Sync held) advertisement stays "live".
pub const PAIRING_TTL: Duration = Duration::from_secs(45);

/// What the bonded-host field of an advertisement tells us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostBinding {
    /// The block is too short to contain the host field (`13 <= len < 16`).
    /// The legacy bridge treated these pads as compatible and still woke them.
    Unknown,
    /// All six host bytes are zero: the pad is in pairing mode (Sync held).
    Pairing,
    /// The pad is bonded to a concrete host, in BLE wire (little-endian) order.
    Host([u8; 6]),
}

impl HostBinding {
    /// Whether this binding names the given adapter.
    ///
    /// `adapter_display_mac` is the human-readable adapter MAC
    /// (`AA:BB:CC:DD:EE:FF`); it is converted to wire order before comparing.
    /// Only [`HostBinding::Host`] can match; `Unknown`/`Pairing` never do.
    pub fn matches_adapter(&self, adapter_display_mac: &str) -> bool {
        match (self, host_field_from_adapter(adapter_display_mac)) {
            (HostBinding::Host(wire), Some(adapter_wire)) => *wire == adapter_wire,
            _ => false,
        }
    }
}

/// A decoded Nintendo BLE advertisement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertInfo {
    /// Controller MAC, normalised to uppercase (legacy compares
    /// `device.address.upper()`).
    pub address: String,
    /// Nintendo vendor id, always `0x057E` after a successful parse.
    pub vid: u16,
    /// Product id (see [`AdvertInfo::is_supported`]).
    pub pid: u16,
    /// Bonded-host state decoded from `[10:16]`.
    pub host: HostBinding,
}

impl AdvertInfo {
    /// Decode the manufacturer-specific data for a device.
    ///
    /// Returns `None` when there is no manufacturer block, when it is shorter
    /// than 13 bytes (no VID/PID), or when the vendor id is not Nintendo's.
    /// A block of 13..16 bytes parses successfully with
    /// [`HostBinding::Unknown`].
    pub fn parse(address: &str, manufacturer_data: Option<&[u8]>) -> Option<Self> {
        let data = manufacturer_data?;
        if data.len() < MIN_ADVERT_LEN {
            return None;
        }

        let vid = u16::from_le_bytes([data[3], data[4]]);
        if vid != NINTENDO_VENDOR_ID {
            return None;
        }
        let pid = u16::from_le_bytes([data[5], data[6]]);

        let host = if data.len() < HOST_FIELD_END {
            HostBinding::Unknown
        } else {
            let mut wire = [0u8; 6];
            wire.copy_from_slice(&data[HOST_FIELD_START..HOST_FIELD_END]);
            if wire == [0u8; 6] {
                HostBinding::Pairing
            } else {
                HostBinding::Host(wire)
            }
        };

        Some(Self {
            address: address.to_ascii_uppercase(),
            vid,
            pid,
            host,
        })
    }

    /// Whether this advertisement is acceptable under `policy` for
    /// `adapter_display_mac`.
    ///
    /// * `Unknown` host — accepted under both policies (legacy compatibility).
    /// * `Pairing` — only accepted when a pairing scan was requested.
    /// * `Host(m)` — only accepted when `m` is this adapter.
    pub fn accepted(&self, adapter_display_mac: &str, policy: AcceptPolicy) -> bool {
        match self.host {
            HostBinding::Unknown => true,
            HostBinding::Pairing => policy == AcceptPolicy::Pairing,
            HostBinding::Host(_) => self.host.matches_adapter(adapter_display_mac),
        }
    }

    /// Whether the product id belongs to a controller this daemon supports.
    pub fn is_supported(&self) -> bool {
        matches!(
            self.pid,
            JOYCON2_RIGHT_PID | JOYCON2_LEFT_PID | PRO_CONTROLLER2_PID | NSO_GAMECUBE_PID
        )
    }
}

/// Which kinds of advertisement a scan is willing to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptPolicy {
    /// Normal bridge wake: accept pads bonded to this adapter (or unknown).
    Wake,
    /// Explicit pairing (`switch2d pair`): additionally accept pairing-mode pads.
    Pairing,
}

/// Parse a display MAC (`AA:BB:CC:DD:EE:FF`) into its big-endian integer form
/// (`0xAABBCCDDEEFF`).
pub fn mac_to_int(mac: &str) -> Option<u64> {
    parse_mac(mac)?
        .iter()
        .try_fold(0u64, |acc, &byte| Some((acc << 8) | u64::from(byte)))
}

/// Convert this adapter's display MAC into the advertisement's host field
/// (6 bytes, little-endian, i.e. the reverse of the display order).
pub fn host_field_from_adapter(adapter_display_mac: &str) -> Option<[u8; 6]> {
    mac_to_le(adapter_display_mac)
}

/// Acceptance for a *known* bonded MAC.
///
/// `None` means the advertisement carried no usable Nintendo block at all;
/// legacy treated that as wake-compatible (`bridge.py:258-265`), so it is
/// accepted. Never call this for an unknown address: without a block there is
/// no PID and no host field to vet.
pub fn accepts(
    advert: Option<&AdvertInfo>,
    adapter_display_mac: &str,
    policy: AcceptPolicy,
) -> bool {
    advert.map_or(true, |a| a.accepted(adapter_display_mac, policy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::command::{format_mac, mac_to_le, parse_mac};

    const PAD_ADDR: &str = "AA:BB:CC:DD:EE:00";
    const ADAPTER: &str = "AA:BB:CC:DD:EE:FF";
    const OTHER_ADAPTER: &str = "11:22:33:44:55:66";
    /// `AA:BB:CC:DD:EE:FF` in wire (little-endian) order.
    const ADAPTER_WIRE: [u8; 6] = [0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA];

    /// Build a manufacturer block; `host == None` yields a 13-byte (host-less)
    /// block, `Some([0; 6])` a pairing advert.
    fn advert_block(vid: u16, pid: u16, host: Option<[u8; 6]>) -> Vec<u8> {
        let len = if host.is_some() {
            HOST_FIELD_END
        } else {
            MIN_ADVERT_LEN
        };
        let mut data = vec![0u8; len];
        data[3..5].copy_from_slice(&vid.to_le_bytes());
        data[5..7].copy_from_slice(&pid.to_le_bytes());
        if let Some(host) = host {
            data[HOST_FIELD_START..HOST_FIELD_END].copy_from_slice(&host);
        }
        data
    }

    #[test]
    fn parses_bonded_wake_advert() {
        let block = advert_block(
            NINTENDO_VENDOR_ID,
            PRO_CONTROLLER2_PID,
            host_field_from_adapter(ADAPTER),
        );
        let info = AdvertInfo::parse(PAD_ADDR, Some(&block)).expect("well-formed advert");

        assert_eq!(info.address, PAD_ADDR);
        assert_eq!(info.vid, 0x057E);
        assert_eq!(info.pid, 0x2069);
        assert_eq!(info.host, HostBinding::Host(ADAPTER_WIRE));
        assert!(info.is_supported());

        // Bonded wake: accepted by the owning adapter under either policy,
        // rejected by a different adapter.
        assert!(info.accepted(ADAPTER, AcceptPolicy::Wake));
        assert!(info.accepted(ADAPTER, AcceptPolicy::Pairing));
        assert!(!info.accepted(OTHER_ADAPTER, AcceptPolicy::Wake));
        assert!(!info.accepted(OTHER_ADAPTER, AcceptPolicy::Pairing));
        assert!(info.host.matches_adapter(ADAPTER));
        assert!(!info.host.matches_adapter(OTHER_ADAPTER));
    }

    #[test]
    fn parses_pairing_advert() {
        let block = advert_block(NINTENDO_VENDOR_ID, JOYCON2_LEFT_PID, Some([0u8; 6]));
        let info = AdvertInfo::parse(PAD_ADDR, Some(&block)).expect("well-formed advert");

        assert_eq!(info.host, HostBinding::Pairing);
        assert_eq!(info.pid, 0x2067);

        // Pairing adverts never satisfy the wake path, only an explicit pair.
        assert!(!info.accepted(ADAPTER, AcceptPolicy::Wake));
        assert!(info.accepted(ADAPTER, AcceptPolicy::Pairing));
        assert!(!info.host.matches_adapter(ADAPTER));

        // Legacy TTLs: brief pairing window (45 s) vs. short wake window (4 s).
        assert_eq!(PAIRING_TTL, Duration::from_secs(45));
        assert_eq!(WAKE_TTL, Duration::from_secs(4));
    }

    #[test]
    fn short_block_is_unknown() {
        let block = advert_block(NINTENDO_VENDOR_ID, JOYCON2_RIGHT_PID, None);
        assert_eq!(block.len(), 13);

        let info = AdvertInfo::parse(PAD_ADDR, Some(&block)).expect("13-byte advert still parses");
        assert_eq!(info.host, HostBinding::Unknown);
        assert!(info.is_supported());

        // Unknown hosts were always considered compatible by the legacy bridge.
        assert!(info.accepted(ADAPTER, AcceptPolicy::Wake));
        assert!(info.accepted(OTHER_ADAPTER, AcceptPolicy::Wake));
        assert!(info.accepted(ADAPTER, AcceptPolicy::Pairing));
        assert!(info.accepted(OTHER_ADAPTER, AcceptPolicy::Pairing));
        assert!(!info.host.matches_adapter(ADAPTER));
    }

    #[test]
    fn missing_manufacturer_data_is_unknown() {
        assert_eq!(AdvertInfo::parse(PAD_ADDR, None), None);
        // Empty block is unusable too.
        assert_eq!(AdvertInfo::parse(PAD_ADDR, Some(&[])), None);
    }

    #[test]
    fn rejects_foreign_vendor_id() {
        let foreign = advert_block(0x054C, PRO_CONTROLLER2_PID, Some([0u8; 6]));
        assert_eq!(AdvertInfo::parse(PAD_ADDR, Some(&foreign)), None);

        // The company id (0x0553) is not the VID (0x057E); mistaking one for
        // the other must not parse.
        let company_as_vid = advert_block(
            crate::protocol::constants::NINTENDO_COMPANY_ID,
            PRO_CONTROLLER2_PID,
            Some([0u8; 6]),
        );
        assert_eq!(AdvertInfo::parse(PAD_ADDR, Some(&company_as_vid)), None);

        // Too short to contain the VID/PID at all.
        let truncated = &advert_block(NINTENDO_VENDOR_ID, PRO_CONTROLLER2_PID, None)[..12];
        assert_eq!(AdvertInfo::parse(PAD_ADDR, Some(truncated)), None);
    }

    #[test]
    fn mac_encoding_round_trip() {
        // Display string -> big-endian integer.
        assert_eq!(mac_to_int(ADAPTER), Some(0xAABB_CCDD_EEFF));
        assert_eq!(
            parse_mac(ADAPTER),
            Some([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])
        );

        // Display string -> little-endian wire field (reversed).
        assert_eq!(host_field_from_adapter(ADAPTER), Some(ADAPTER_WIRE));
        assert_eq!(host_field_from_adapter(ADAPTER), mac_to_le(ADAPTER));

        // Wire field -> display string (the reverse direction the daemon uses).
        let mut reclaimed = ADAPTER_WIRE;
        reclaimed.reverse();
        assert_eq!(format_mac(&reclaimed), ADAPTER);
        assert_eq!(mac_to_int(&format_mac(&reclaimed)), mac_to_int(ADAPTER));

        // Wire bytes must never be interpreted as the display order.
        assert_ne!(ADAPTER_WIRE, parse_mac(ADAPTER).expect("valid MAC"));

        // Malformed input yields None instead of panicking.
        assert_eq!(mac_to_int("AA:BB:CC:DD:EE"), None);
        assert_eq!(mac_to_int("AA:BB:CC:DD:EE:ZZ"), None);
        assert_eq!(host_field_from_adapter("nope"), None);
    }

    #[test]
    fn accept_policy_matrix() {
        // (binding, wake@matching, pairing@matching, wake@other, pairing@other)
        let cases = [
            (HostBinding::Unknown, true, true, true, true),
            (HostBinding::Pairing, false, true, false, true),
            (HostBinding::Host(ADAPTER_WIRE), true, true, false, false),
        ];

        for (host, wake_matching, pairing_matching, wake_other, pairing_other) in cases {
            let info = AdvertInfo {
                address: PAD_ADDR.to_string(),
                vid: NINTENDO_VENDOR_ID,
                pid: PRO_CONTROLLER2_PID,
                host,
            };

            assert_eq!(
                info.accepted(ADAPTER, AcceptPolicy::Wake),
                wake_matching,
                "wake policy vs. matching adapter for {host:?}"
            );
            assert_eq!(
                info.accepted(ADAPTER, AcceptPolicy::Pairing),
                pairing_matching,
                "pairing policy vs. matching adapter for {host:?}"
            );
            assert_eq!(
                info.accepted(OTHER_ADAPTER, AcceptPolicy::Wake),
                wake_other,
                "wake policy vs. non-matching adapter for {host:?}"
            );
            assert_eq!(
                info.accepted(OTHER_ADAPTER, AcceptPolicy::Pairing),
                pairing_other,
                "pairing policy vs. non-matching adapter for {host:?}"
            );
        }
    }

    #[test]
    fn supported_pids() {
        let parse_pid = |pid: u16| {
            AdvertInfo::parse(
                PAD_ADDR,
                Some(&advert_block(NINTENDO_VENDOR_ID, pid, Some([0u8; 6]))),
            )
            .expect("valid advert")
        };

        assert!(parse_pid(JOYCON2_RIGHT_PID).is_supported());
        assert!(parse_pid(JOYCON2_LEFT_PID).is_supported());
        assert!(parse_pid(PRO_CONTROLLER2_PID).is_supported());
        assert!(parse_pid(NSO_GAMECUBE_PID).is_supported());

        assert!(!parse_pid(0x0000).is_supported());
        assert!(!parse_pid(0x2006).is_supported());
        assert!(!parse_pid(0x2068).is_supported());
        assert!(!parse_pid(0xFFFF).is_supported());
    }

    #[test]
    fn absent_advert_is_wake_compatible() {
        // bridge.py:258-265 -- a missing/short manufacturer block is accepted
        // for a known MAC, under either policy.
        assert!(accepts(None, ADAPTER, AcceptPolicy::Wake));
        assert!(accepts(None, ADAPTER, AcceptPolicy::Pairing));
        // Adapter identity is irrelevant without a host field to compare.
        assert!(accepts(None, OTHER_ADAPTER, AcceptPolicy::Wake));
        assert!(accepts(None, OTHER_ADAPTER, AcceptPolicy::Pairing));
    }

    #[test]
    fn known_mac_advert_matrix() {
        // Known MAC, but the pad is bonded to a different host: rejected under
        // both policies. Being a known address does not bypass host vetting.
        let foreign = AdvertInfo::parse(
            PAD_ADDR,
            Some(&advert_block(
                NINTENDO_VENDOR_ID,
                PRO_CONTROLLER2_PID,
                host_field_from_adapter(OTHER_ADAPTER),
            )),
        )
        .expect("well-formed advert");
        assert!(!accepts(Some(&foreign), ADAPTER, AcceptPolicy::Wake));
        assert!(!accepts(Some(&foreign), ADAPTER, AcceptPolicy::Pairing));

        // Conversely, the owning adapter (and only it) is accepted.
        let own = AdvertInfo::parse(
            PAD_ADDR,
            Some(&advert_block(
                NINTENDO_VENDOR_ID,
                PRO_CONTROLLER2_PID,
                host_field_from_adapter(ADAPTER),
            )),
        )
        .expect("well-formed advert");
        assert!(accepts(Some(&own), ADAPTER, AcceptPolicy::Wake));
        assert!(!accepts(Some(&own), OTHER_ADAPTER, AcceptPolicy::Wake));
    }
}
