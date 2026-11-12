//! BlueZ advertisement source backed by `busctl GetManagedObjects` (ADR-6).
//!
//! Discovery is still driven through [`ScanControl`](super::scan::ScanControl)
//! (`btmgmt find`); this module only *reads* the object table BlueZ builds. One
//! `busctl call org.bluez / org.freedesktop.DBus.ObjectManager
//! GetManagedObjects --json=short` returns both the adapter address (so
//! `bluetoothctl list` scraping is gone) and every `Device1` object with
//! `Address`, `Connected`, `RSSI` and `ManufacturerData`.
//!
//! BlueZ properties are cached, so `RSSI` is the only freshness signal; see
//! [`DeviceAdvert::is_live`](super::scan::DeviceAdvert::is_live).
//!
//! Everything below the source is pure: [`parse_managed_objects`] and
//! [`parse_adapter_mac`] take the raw JSON string, which keeps the whole parser
//! testable without D-Bus, processes or hardware.

use std::io;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

use super::scan::{run_command, AdvertSource, DeviceAdvert};
use crate::protocol::advert::AdvertInfo;
use crate::protocol::constants::NINTENDO_COMPANY_ID;

const ORG_BLUEZ_ADAPTER1: &str = "org.bluez.Adapter1";
const ORG_BLUEZ_DEVICE1: &str = "org.bluez.Device1";
/// `busctl` is resolved from `PATH`, with the distro location as a fallback.
const BUSCTL_FALLBACK: &str = "/usr/bin/busctl";

/// [`AdvertSource`] that snapshots BlueZ's managed objects.
#[derive(Debug, Clone)]
pub struct BluezAdvertSource {
    command: PathBuf,
    adapter_mac: Option<String>,
}

impl Default for BluezAdvertSource {
    fn default() -> Self {
        Self::new()
    }
}

impl BluezAdvertSource {
    /// Use the first `busctl` on `PATH`, else `/usr/bin/busctl`.
    pub fn new() -> Self {
        Self {
            command: resolve_busctl(),
            adapter_mac: None,
        }
    }

    /// Use an explicit `busctl` path or name.
    pub fn with_command(command: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            adapter_mac: None,
        }
    }

    /// Address of `hci{hci_index}`, cached after the first success.
    pub fn adapter_mac(&mut self, hci_index: u8) -> io::Result<String> {
        if let Some(mac) = &self.adapter_mac {
            return Ok(mac.clone());
        }

        let json = self.managed_objects_json()?;
        let mac = parse_adapter_mac(&json, hci_index).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("BlueZ did not report an address for hci{hci_index}"),
            )
        })?;

        self.adapter_mac = Some(mac.clone());
        Ok(mac)
    }

    fn managed_objects_json(&self) -> io::Result<String> {
        let output = run_command(Command::new(&self.command).args([
            "call",
            "org.bluez",
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            "--json=short",
        ]))?;

        String::from_utf8(output).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("busctl output is not valid UTF-8: {err}"),
            )
        })
    }
}

impl BluezAdvertSource {
    /// Snapshot including the raw fields used by `switch2d advert`.
    pub fn snapshot(&mut self) -> io::Result<Vec<AdvertSnapshot>> {
        let json = self.managed_objects_json()?;
        parse_advert_snapshots(&json)
    }
}

impl AdvertSource for BluezAdvertSource {
    fn devices(&mut self) -> io::Result<Vec<DeviceAdvert>> {
        let json = self.managed_objects_json()?;
        parse_managed_objects(&json)
    }
}

/// One BlueZ `Device1` entry with the raw fields the `switch2d advert`
/// diagnostic needs (name, address type, manufacturer block).
///
/// Deliberately separate from [`DeviceAdvert`]: that type is the daemon's
/// transport-agnostic view and must not grow fields the hub does not use.
#[derive(Debug, Clone, PartialEq)]
pub struct AdvertSnapshot {
    pub address: String,
    /// BlueZ `AddressType` (`"public"` / `"random"`), when exposed.
    pub address_type: Option<String>,
    /// `Alias` when present, else `Name`.
    pub name: Option<String>,
    pub connected: bool,
    pub rssi: Option<i16>,
    /// Raw Nintendo manufacturer block, exactly as BlueZ returned it.
    pub manufacturer_data: Option<Vec<u8>>,
    pub advert: Option<AdvertInfo>,
}

impl AdvertSnapshot {
    /// Live advertisement rather than a cached BlueZ property set.
    pub fn is_live(&self) -> bool {
        matches!(self.rssi, Some(rssi) if rssi > -127)
    }
}

/// Find `busctl` on `PATH`, falling back to the distro path.
fn resolve_busctl() -> PathBuf {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();

    dirs.into_iter()
        .map(|dir| dir.join("busctl"))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(BUSCTL_FALLBACK))
}

/// Parse a `busctl --json=short` `GetManagedObjects` reply into devices.
///
/// Objects without an `org.bluez.Device1` member are ignored. A device entry
/// that is missing its `Address` is skipped rather than failing the snapshot.
/// Errors are reserved for malformed JSON or a `data` array that is not the
/// expected single object.
pub fn parse_managed_objects(json: &str) -> io::Result<Vec<DeviceAdvert>> {
    let objects = parse_object_map(json)?;

    let mut devices = Vec::new();
    for object in objects.values() {
        let Some(device) = object.get(ORG_BLUEZ_DEVICE1) else {
            continue;
        };
        if let Some(advert) = parse_device(device) {
            devices.push(advert);
        }
    }

    Ok(devices)
}

/// Parse a `GetManagedObjects` reply into [`AdvertSnapshot`]s (raw view).
pub fn parse_advert_snapshots(json: &str) -> io::Result<Vec<AdvertSnapshot>> {
    let objects = parse_object_map(json)?;
    let mut devices = Vec::new();
    for object in objects.values() {
        if let Some(device) = object.get(ORG_BLUEZ_DEVICE1) {
            if let Some(snapshot) = parse_snapshot_device(device) {
                devices.push(snapshot);
            }
        }
    }
    Ok(devices)
}

/// Extract `org.bluez.Adapter1.Address` for `/org/bluez/hci{hci_index}`.
pub fn parse_adapter_mac(json: &str, hci_index: u8) -> Option<String> {
    let objects = parse_object_map(json).ok()?;
    let adapter_path = format!("/org/bluez/hci{hci_index}");

    property_string(
        objects.get(&adapter_path)?.get(ORG_BLUEZ_ADAPTER1)?,
        "Address",
    )
}

/// `data[0]` of the `a{oa{sa{sv}}}` reply, i.e. the object path map.
fn parse_object_map(json: &str) -> io::Result<serde_json::Map<String, Value>> {
    let root: Value = serde_json::from_str(json).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid BlueZ managed-object JSON: {err}"),
        )
    })?;

    root.get("data")
        .and_then(Value::as_array)
        .and_then(|data| data.first())
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "BlueZ payload has no object map at data[0]",
            )
        })
}

fn parse_device(device: &Value) -> Option<DeviceAdvert> {
    let snapshot = parse_snapshot_device(device)?;
    Some(DeviceAdvert {
        address: snapshot.address,
        connected: snapshot.connected,
        rssi: snapshot.rssi,
        advert: snapshot.advert,
    })
}

fn parse_snapshot_device(device: &Value) -> Option<AdvertSnapshot> {
    let address = property_string(device, "Address")?;
    let address_type = property_string(device, "AddressType").map(|kind| kind.to_lowercase());
    let name = property_string(device, "Alias").or_else(|| property_string(device, "Name"));
    let connected = property_bool(device, "Connected").unwrap_or(false);
    let rssi = property_i16(device, "RSSI");
    let manufacturer_data = manufacturer_data(device);
    let advert = manufacturer_data
        .as_ref()
        .and_then(|bytes| AdvertInfo::parse(&address, Some(bytes)));

    Some(AdvertSnapshot {
        address,
        address_type,
        name,
        connected,
        rssi,
        manufacturer_data,
        advert,
    })
}

/// Raw `data` field of a BlueZ property variant (`{"type":..,"data":..}`).
fn property<'a>(object: &'a Value, name: &str) -> Option<&'a Value> {
    object.get(name)?.get("data")
}

fn property_string(object: &Value, name: &str) -> Option<String> {
    property(object, name)?.as_str().map(str::to_string)
}

fn property_bool(object: &Value, name: &str) -> Option<bool> {
    property(object, name)?.as_bool()
}

fn property_i16(object: &Value, name: &str) -> Option<i16> {
    i16::try_from(property(object, name)?.as_i64()?).ok()
}

/// Nintendo company id (`0x0553`) as `ManufacturerData` bytes.
///
/// BlueZ emits `a{qv}` as `[{"<company-id>": {"type":"ay","data":[..]}}]`, but
/// the company id is accepted as a decimal/hex-decimal string key or as a JSON
/// number in the two-element `[company-id, variant]` shape.
fn manufacturer_data(device: &Value) -> Option<Vec<u8>> {
    let entries = property(device, "ManufacturerData")?.as_array()?;

    for entry in entries {
        if let Some(map) = entry.as_object() {
            for (company_id, value) in map {
                if company_id_is_nintendo(company_id) {
                    if let Some(bytes) = ay_bytes(value) {
                        return Some(bytes);
                    }
                }
            }
        } else if let Some(pair) = entry.as_array() {
            if pair.len() == 2 && company_id_value_is_nintendo(&pair[0]) {
                if let Some(bytes) = ay_bytes(&pair[1]) {
                    return Some(bytes);
                }
            }
        }
    }

    None
}

fn company_id_is_nintendo(key: &str) -> bool {
    let key = key.trim();
    let parsed = if let Some(hex) = key.strip_prefix("0x").or_else(|| key.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        key.parse::<u32>().ok()
    };

    parsed == Some(u32::from(NINTENDO_COMPANY_ID))
}

fn company_id_value_is_nintendo(value: &Value) -> bool {
    match value {
        Value::String(key) => company_id_is_nintendo(key),
        Value::Number(number) => number.as_u64() == Some(u64::from(NINTENDO_COMPANY_ID)),
        _ => false,
    }
}

/// Byte array (`ay`) from a variant `{"type":"ay","data":[..]}`.
fn ay_bytes(value: &Value) -> Option<Vec<u8>> {
    value
        .get("data")?
        .as_array()?
        .iter()
        .map(|byte| u8::try_from(byte.as_u64()?).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::advert::HostBinding;
    use crate::protocol::constants::PRO_CONTROLLER2_PID;

    /// Synthetic object table shaped like a `busctl` capture; also carries
    /// the adapter object.
    const MANAGED_OBJECTS: &str = r#"{
        "type": "a{oa{sa{sv}}}",
        "data": [
            {
                "/org/bluez/hci0": {
                    "org.bluez.Adapter1": {
                        "Address": {"type": "s", "data": "0A:0B:0C:0D:0E:0F"}
                    }
                },
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01": {
                    "org.bluez.Device1": {
                        "Address": {"type": "s", "data": "AA:BB:CC:DD:EE:01"},
                        "Connected": {"type": "b", "data": false},
                        "RSSI": {"type": "n", "data": -53},
                        "ManufacturerData": {
                            "type": "a{qv}",
                            "data": [
                                {
                                    "1363": {
                                        "type": "ay",
                                        "data": [0, 0, 0, 126, 5, 105, 32, 0, 0, 0, 1, 238, 221, 204, 187, 170]
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        ]
    }"#;

    fn device_object(properties: &str) -> String {
        format!(r#"{{"org.bluez.Device1": {{{properties}}}}}"#)
    }

    fn managed_objects_with(devices: &[(&str, String)]) -> String {
        let entries: Vec<String> = devices
            .iter()
            .map(|(path, object)| format!(r#""{path}": {object}"#))
            .collect();
        format!(
            r#"{{"type":"a{{oa{{sa{{sv}}}}}}","data":[{{{}}}]}}"#,
            entries.join(",")
        )
    }

    #[test]
    fn parses_device_with_manufacturer_data() {
        let devices = parse_managed_objects(MANAGED_OBJECTS).expect("fixture parses");
        assert_eq!(devices.len(), 1);

        let device = &devices[0];
        assert_eq!(device.address, "AA:BB:CC:DD:EE:01");
        assert!(!device.connected);
        assert_eq!(device.rssi, Some(-53));
        assert!(device.is_live());

        let advert = device.advert.as_ref().expect("manufacturer block decoded");
        assert_eq!(advert.address, "AA:BB:CC:DD:EE:01");
        assert_eq!(advert.vid, 0x057E);
        assert_eq!(advert.pid, PRO_CONTROLLER2_PID);
        assert_eq!(
            advert.host,
            HostBinding::Host([0x01, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA])
        );
        assert!(advert.host.matches_adapter("AA:BB:CC:DD:EE:01"));
    }

    #[test]
    fn parses_device_without_manufacturer_data() {
        let json = managed_objects_with(&[(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
            device_object(
                r#""Address": {"type":"s","data":"AA:BB:CC:DD:EE:FF"},
                   "Connected": {"type":"b","data":true},
                   "RSSI": {"type":"n","data":-40}"#,
            ),
        )]);

        let devices = parse_managed_objects(&json).expect("fixture parses");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].address, "AA:BB:CC:DD:EE:FF");
        assert!(devices[0].connected);
        assert_eq!(devices[0].rssi, Some(-40));
        assert_eq!(devices[0].advert, None);
    }

    #[test]
    fn parses_device_without_rssi() {
        let json = managed_objects_with(&[(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
            device_object(
                r#""Address": {"type":"s","data":"AA:BB:CC:DD:EE:FF"},
                   "Connected": {"type":"b","data":false}"#,
            ),
        )]);

        let devices = parse_managed_objects(&json).expect("fixture parses");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].rssi, None);
        assert!(!devices[0].is_live());
    }

    #[test]
    fn ignores_non_device_objects() {
        // Adapter-only and unknown-interface objects must not become devices.
        let json = r#"{
            "type": "a{oa{sa{sv}}}",
            "data": [{
                "/org/bluez/hci0": {
                    "org.bluez.Adapter1": {
                        "Address": {"type": "s", "data": "0A:0B:0C:0D:0E:0F"}
                    }
                },
                "/org/bluez/hci0/dev_AA": {
                    "org.example.Other1": {
                        "Address": {"type": "s", "data": "AA:BB:CC:DD:EE:FF"}
                    }
                }
            }]
        }"#;

        assert!(parse_managed_objects(json)
            .expect("fixture parses")
            .is_empty());
    }

    #[test]
    fn extracts_adapter_address() {
        assert_eq!(
            parse_adapter_mac(MANAGED_OBJECTS, 0),
            Some("0A:0B:0C:0D:0E:0F".to_string())
        );
        // Unknown index maps to an object path that does not exist.
        assert_eq!(parse_adapter_mac(MANAGED_OBJECTS, 1), None);
        assert_eq!(parse_adapter_mac("not json", 0), None);
    }

    #[test]
    fn snapshot_exposes_name_address_type_and_raw_block() {
        let bytes = [0, 0, 0, 126, 5, 105, 32, 0, 0, 0, 1, 2, 3, 4, 5, 6];
        let json = managed_objects_with(&[(
            "/org/bluez/hci0/dev_AA",
            device_object(&format!(
                r#""Address": {{"type":"s","data":"AA:BB:CC:DD:EE:FF"}},
                   "AddressType": {{"type":"s","data":"public"}},
                   "Alias": {{"type":"s","data":"Joy-Con 2 (Left)"}},
                   "RSSI": {{"type":"n","data":-52}},
                   "ManufacturerData": {{"type":"a{{qv}}","data":[{{"1363":{{"type":"ay","data":{bytes:?}}}}}]}}"#
            )),
        )]);

        let snapshots = parse_advert_snapshots(&json).expect("snapshot parses");
        assert_eq!(snapshots.len(), 1);
        let snapshot = &snapshots[0];
        assert_eq!(snapshot.address, "AA:BB:CC:DD:EE:FF");
        assert_eq!(snapshot.address_type.as_deref(), Some("public"));
        assert_eq!(snapshot.name.as_deref(), Some("Joy-Con 2 (Left)"));
        assert_eq!(snapshot.rssi, Some(-52));
        assert_eq!(snapshot.manufacturer_data.as_deref(), Some(&bytes[..]));
        assert!(snapshot.advert.is_some());
        assert!(snapshot.is_live());

        // The hub's transport-agnostic view shares the same parser.
        let devices = parse_managed_objects(&json).expect("devices parse");
        assert_eq!(devices[0].address, "AA:BB:CC:DD:EE:FF");
        assert!(devices[0].is_live());
    }

    #[test]
    fn company_id_key_as_number_and_string() {
        let bytes = [0, 0, 0, 126, 5, 105, 32, 0, 0, 0, 1, 2, 3, 4, 5, 6];

        // Company id as a decimal string key.
        let as_string = managed_objects_with(&[(
            "/org/bluez/hci0/dev_AA",
            device_object(&format!(
                r#""Address": {{"type":"s","data":"AA:BB:CC:DD:EE:FF"}},
                   "ManufacturerData": {{"type":"a{{qv}}","data":[{{"1363":{{"type":"ay","data":{bytes:?}}}}}]}}"#
            )),
        )]);
        let devices = parse_managed_objects(&as_string).expect("string key parses");
        assert_eq!(devices.len(), 1);
        assert!(devices[0].advert.is_some());

        // Company id as a JSON number in the pair shape.
        let as_number = managed_objects_with(&[(
            "/org/bluez/hci0/dev_AA",
            device_object(&format!(
                r#""Address": {{"type":"s","data":"AA:BB:CC:DD:EE:FF"}},
                   "ManufacturerData": {{"type":"a{{qv}}","data":[[1363,{{"type":"ay","data":{bytes:?}}}]]}}"#
            )),
        )]);
        let devices = parse_managed_objects(&as_number).expect("numeric company id parses");
        assert_eq!(devices.len(), 1);
        assert!(devices[0].advert.is_some());

        // Hex spelling of the same id, and a non-Nintendo id that is ignored.
        let hex_key = managed_objects_with(&[(
            "/org/bluez/hci0/dev_AA",
            device_object(&format!(
                r#""Address": {{"type":"s","data":"AA:BB:CC:DD:EE:FF"}},
                   "ManufacturerData": {{"type":"a{{qv}}","data":[{{"0x553":{{"type":"ay","data":{bytes:?}}}}}]}}"#
            )),
        )]);
        assert!(parse_managed_objects(&hex_key).expect("hex key parses")[0]
            .advert
            .is_some());

        let foreign_key = managed_objects_with(&[(
            "/org/bluez/hci0/dev_AA",
            device_object(&format!(
                r#""Address": {{"type":"s","data":"AA:BB:CC:DD:EE:FF"}},
                   "ManufacturerData": {{"type":"a{{qv}}","data":[{{"1358":{{"type":"ay","data":{bytes:?}}}}}]}}"#
            )),
        )]);
        assert!(
            parse_managed_objects(&foreign_key).expect("foreign key parses")[0]
                .advert
                .is_none()
        );
    }

    #[test]
    fn malformed_json_is_error_but_bad_device_entry_is_skipped() {
        assert_eq!(
            parse_managed_objects("{not json")
                .expect_err("malformed JSON")
                .kind(),
            io::ErrorKind::InvalidData
        );

        // `data` is not an object.
        assert_eq!(
            parse_managed_objects(r#"{"type":"a{oa{sa{sv}}}","data":[[]]}"#)
                .expect_err("data[0] is an array")
                .kind(),
            io::ErrorKind::InvalidData
        );

        // One good device, one entry whose Device1 has no Address: skip the bad
        // entry, keep the good one.
        let json = managed_objects_with(&[
            (
                "/org/bluez/hci0/dev_AA",
                device_object(r#""Address": {"type":"s","data":"AA:BB:CC:DD:EE:FF"}"#),
            ),
            (
                "/org/bluez/hci0/dev_BB",
                device_object(r#""Connected": {"type":"b","data":true}"#),
            ),
        ]);

        let devices = parse_managed_objects(&json).expect("bad entry is skipped");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].address, "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn is_live_rejects_missing_and_sentinel_rssi() {
        let base = DeviceAdvert {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            connected: false,
            rssi: None,
            advert: None,
        };

        assert!(!base.is_live());
        assert!(!DeviceAdvert {
            rssi: Some(-127),
            ..base.clone()
        }
        .is_live());
        assert!(!DeviceAdvert {
            rssi: Some(-128),
            ..base.clone()
        }
        .is_live());
        assert!(DeviceAdvert {
            rssi: Some(-126),
            ..base.clone()
        }
        .is_live());
        assert!(DeviceAdvert {
            rssi: Some(0),
            ..base
        }
        .is_live());
    }
}
