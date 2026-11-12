# Legacy Analysis — Discovery, Connection Lifecycle, and Player-Slot Assignment

**Source of truth:** `.old/` (Python prototype `ngc/*`, plus `system/` and `tools/`)
**Audience:** Rust re-implementation (`src/`) — Lead Software Architect
**Scope:** controller discovery (BLE + the absence of USB/HID discovery), connection
lifecycle/handshake/event loops, and controller position assignment (player slots,
Joy-Con L/R pairing, downstream emulator ordering).

All file references are relative to `.old/` unless noted. Line numbers are from the
snapshot in `.old/` at the time of this analysis.

---

## 0. Executive summary

1. **Discovery is BLE-advertisement-only.** Controllers are identified from a single
   manufacturer-specific AD block (company id `0x0553`) that carries a Nintendo VID, a
   product id, and — critically — the **host MAC the pad is bonded to**. That host MAC
   field is the entire "is this pad awake for *me*, or in pairing mode?" state machine.
2. **Connections are raw L2CAP/ATT sockets with `BT_SECURITY_LOW`.** BlueZ's GATT stack
   is bypassed on purpose; SMP pairing attempts kill the link. This forces an
   **exclusive-ownership discipline** over the adapter: LE scanning and
   create-connection cannot be interleaved, so the legacy code stops LE scanning at the
   HCI level (`btmgmt stop-find -l`), removes the BlueZ `Device` object, then dials.
3. **The lifecycle is a handshake of ~10 ordered ATT command/response exchanges**
   (GATT discovery → notification subscribe → identity read → calibration →
   player LEDs → features → input subscribe → optional bond), all over one
   single-outstanding-request ATT mutex. Input arrives as notifications on the reader
   thread.
4. **Player assignment is config-first, upstream-decided.** The bridge owns a persisted
   `player` (1–8) per bonded MAC, pushes it into the controller's player LEDs, and maps
   `player-1` to a DSU motion slot. The *authoritative* player order for emulators is
   computed **outside** the bridge by `system/bazzite-controller-detect.py` /
   `bazzite-set-player-leds.py`, which write `led-players.json` back into the bridge.
5. **Joy-Con L/R pairing does not exist in the legacy code.** Left/Right are separate
   product ids with separate vibration UUIDs, and Joy-Cons are explicitly excluded from
   "has second stick". The Rust README promises *combined* and *split* Joy-Con modes —
   that is **net-new design**, not a port.

---

## 1. Component map (legacy)

| File | Responsibility |
| --- | --- |
| `ngc/protocol.py` | Pure data: VIDs/PIDs, GATT UUIDs, commands, memory addresses, button bitmask, report/calibration/vibration codecs, `build_command`, advertisement reconnect-MAC decode |
| `ngc/att.py` | Minimal ATT client on a raw L2CAP seqpacket socket; MTU exchange; GATT service/characteristic/CCCD discovery; reader thread |
| `ngc/device.py` | `SwitchController`: connect, GATT handle resolution, handshake (`initialize`), commands, bonding, LED, rumble, idle sleep, input decode |
| `ngc/scanner.py` | BLE advertisement parsing + the three scan helpers used by the CLI/tools |
| `ngc/bridge.py` | `_ConnectHub` (single scanner + connect scheduling), `_Worker` (per-pad session), `Bridge` (state, DSU, reorder hooks) |
| `ngc/config.py` | Persistent config (`~/.config/nso-gc/config.json`), adapter detection, player-slot bookkeeping |
| `ngc/gamepad.py` | uinput virtual pad + force-feedback decode |
| `ngc/motion_evdev.py` | Companion `INPUT_PROP_ACCELEROMETER` uinput node sharing `phys="ngc/MAC"` |
| `ngc/dsu.py` | DSU/cemuhook UDP server, 4 slots, `slot = player-1` |
| `ngc/status.py` | Atomic `state.json` for GUI/Decky/watchdog |
| `ngc/controller.py` | **Legacy** bleak-based GATT implementation (pre-raw-L2CAP). Not used by the bridge; only some tools still import it |
| `system/bazzite-controller-detect.py` | Detects *all* pads via evdev/SDL, classifies kind, orders them, writes emulator configs |
| `system/bazzite-set-player-leds.py` | Applies the ordering to LEDs; writes `led-players.json` consumed by the bridge |
| `tools/*` | Bring-up diagnostics (`scan_list`, `adv_watch`, `att_discover`, `l2cap_test`, `pair_verify`, `verify_device`, `diag`) |

---

## 2. Discovery mechanisms

### 2.1 BLE (the only real discovery path for Switch 2 pads)

`ngc/scanner.py` uses **bleak** (`BleakScanner`, BlueZ/D-Bus backend) with a detection
callback. Discovery is **passive parsing of advertisement payloads**; no GATT connection
is needed to identify a pad.

**Advertisement layout as decoded by the legacy code** (`scanner.py:37-48`,
`protocol.py:reconnect_mac_from_advertisement`):

| Offset in `manufacturer_data[0x0553]` | Size | Meaning |
| --- | --- | --- |
| `[0:3]` | 3 | Unused/unknown prefix (not inspected) |
| `[3:5]` | 2 | Nintendo **VID**, LE u16 — must equal `0x057E` |
| `[5:7]` | 2 | **PID**, LE u16 — see table below |
| `[7:10]` | 3 | Not inspected (flags/status) |
| `[10:16]` | 6 | **Bonded-host MAC**, little-endian 48-bit (`0` ⇒ pairing mode) |

Notes / invariants:

* bleak keys `manufacturer_data` by the 16-bit **company id** and strips those two
  bytes, hence "offset" above is *after* the company id. `NINTENDO_COMPANY_ID = 0x0553`
  is **not** the Nintendo VID (`0x057E`); both appear and both are checked.
* `_parse_manufacturer` requires `len(manu) >= 13`; the reconnect MAC requires
  `len(manu) >= 16`. A pad with a short AD block is accepted as "unknown host", not
  rejected — see §2.2.
* Encoding asymmetry worth pinning down in Rust (and adding a unit test for):
  `marketing MAC "AA:BB:CC:DD:EE:FF"` → `mac_to_int()` = **big-endian** integer of the
  six bytes `AA BB CC DD EE FF`, while the advertisement field is decoded with
  `decodeu()` = **little-endian** over bytes `manu[10:16]`. So the wire form is the
  reverse byte order of the display string, and the comparison is done as an integer.
  Any Rust port must test both directions explicitly.
* Matching against saved pads is done on `device.address.upper()` (MAC string).

**Supported product ids** (`protocol.py`):

| PID | Name | HD rumble | Analog triggers | Second stick |
| --- | --- | --- | --- | --- |
| `0x2066` | Joy-Con 2 (Right) | yes | no | no |
| `0x2067` | Joy-Con 2 (Left) | yes | no | no |
| `0x2069` | Pro Controller 2 | yes | no | yes |
| `0x2073` | NSO GameCube Controller | **no** (presets only) | **yes** | yes |

### 2.2 Advertisement "modes" — the whole wake/pairing state machine

`reconnect_mac_from_advertisement(adv)` returns:

| Value | Meaning | Legacy handling |
| --- | --- | --- |
| `None` | No manufacturer block / too short | Treated as *compatible* — accepted for wake (`_accept`), accepted in `wait_for_addresses` even with `bonded_wake_only` |
| `0` | **Pairing mode** (Sync held) | Only accepted when `require_pairing=True` (CLI `pair`/`rebond`); bridge uses TTL 45 s and treats it as "pairing" mode |
| `== host_mac` | **Bonded wake** (button press) | Normal bridge wake path, TTL 4 s |
| anything else | Bonded to **another host** | **Rejected** (`_ConnectHub._accept`, `wait_for_addresses`) — this is why the adapter MAC must be persisted and correct |

TTLs (`bridge.py:52-60`): wake `4.0 s`, pairing `45.0 s`. Pairing adverts are brief,
wake adverts repeat — the longer TTL for pairing is deliberate.

### 2.3 The three legacy scan helpers (`scanner.py`)

| Helper | Used by | Behaviour |
| --- | --- | --- |
| `scan(timeout, only_pids, require_pairing)` | `tools/scan_list.py`, setup UI listing | Collect **all** matching adverts for `timeout`, de-dup by address, return `Discovered(device, vid, pid)` |
| `find_first(timeout, only_pids, require_pairing)` | `python -m ngc pair` / `rebond` | Return the **first** match immediately; stop the scan in `finally` |
| `wait_for_addresses(addresses, timeout, host_mac, bonded_wake_only)` | **no longer used at runtime** (superseded by `_ConnectHub`) | Wait for one of a saved set; last arg filters to real button-wakes |

The bridge **does not** use these; it owns the scanner itself (see §3.6) because it must
stop scanning *before every connect attempt* rather than after a fixed window.

### 2.4 USB / HID

* **There is no USB transport for Switch 2 controllers in the legacy code** — no
  `hidraw`/`hidapi`/USB path anywhere (`grep` over `.old/` finds none). The protocol
  layer is transport-agnostic, but only an L2CAP/ATT backend is implemented. Rust
  should treat USB as *future work*, not a port target.
* **HID/evdev appears only downstream**, in two roles:
  1. `system/ngc_evdev_uniq_shim.c` — `LD_PRELOAD` `ioctl` shim: `uinput` cannot set
     `uniq` (`EVIOCGUNIQ` → `ENOENT`), so it returns `phys` for devices whose `phys`
     starts with `ngc/`. This lets SDL3 pair the gamepad node with the IMU node.
  2. `system/bazzite-set-player-leds.py` — walks `/sys/class/input/*/uniq` and
     `/sys/bus/hid/devices/*/uniq|uevent (HID_UNIQ=)` to find **player LEDs** for
     *non*-Switch-2 pads (DualSense/EXLENE/N64). Switch-2 bridge pads never expose
     sysfs player LEDs, so they are driven over BLE instead (§4.2).
* `udev/70-ngc-imu-uaccess.rules` grants `uaccess` to `PHYS=="ngc/*"` accelerometer
  nodes; `71-controller-player-leds-uaccess.rules` chmods `*:player-*` LED brightness
  for the other pads.

### 2.5 Adapter discovery

`config.py:detect_adapter()`:
1. `/sys/class/bluetooth/hciN/address` (may be missing on newer kernels),
2. else `busctl get-property org.bluez /org/bluez/hciN org.bluez.Adapter1 Address`
   (no root),
3. override with `NGC_HCI` for the *index* used by `btmgmt` (default `0`, `bridge.py:66`).

---

## 3. Connection lifecycle

### 3.1 L2CAP socket setup (`att.py:_connect_once`)

```
socket(AF_BLUETOOTH=31, SOCK_SEQPACKET, BTPROTO_L2CAP=0)
setsockopt(SOL_BLUETOOTH=274, BT_SECURITY=4, struct{level=1 (LOW), key_size=0})
bind(sockaddr_l2{psm=0, bdaddr=<adapter MAC LE>, cid=4 (ATT_CID), atype=1 (LE_PUBLIC)})
setblocking(False)
connect(sockaddr_l2{psm=0, bdaddr=<pad MAC LE>, cid=4, atype=<LE_PUBLIC|LE_RANDOM>})
  errno EINPROGRESS(115) / EALREADY(114) are expected
select([], [fd], [], timeout)            -> timeout ⇒ "adapter may still be scanning"
getsockopt(SO_ERROR)                     -> non-zero ⇒ failure
setblocking(True)                        -> start reader thread -> ATT Exchange MTU 247
```

* `sockaddr_l2` is packed as `"<HH6sHB"` + 1 pad byte (14 bytes); MACs are passed in
  **reverse** byte order (`baddr()`).
* `BT_SECURITY_LOW` is mandatory. **SMP pairing attempts cause the controller to drop
  the link** — this is the core reason BlueZ GATT is bypassed.
* MTU: request 247, `mtu = min(247, server_mtu)`; failure is non-fatal (MTU stays 23).
  Input reports are 63 bytes, so a 23-byte MTU yield truncated reports — the legacy code
  does not guard against that (it would just mis-parse). Worth an explicit check in Rust.
* Address type: **public first, then random**, with the last known-good type tried first
  on subsequent attempts (`_Worker.last_dst_type`, `bridge.py:436-470`), alternating on
  failure. Bonded pads vary by firmware.

### 3.2 The adapter-ownership problem (the single biggest portability constraint)

Raw create-connection **cannot run while the adapter is discovering** — including
Steam's permanent background LE scan. Legacy mitigations, all in `bridge.py`:

| Helper | Effect |
| --- | --- |
| `prepare_bluez_global()` | `pkill -f decky-bluetooth-wake-control`, `bluetoothctl scan off`, D-Bus `Adapter1.StopDiscovery`, then `_force_le_scan_off(force=True)` |
| `_force_le_scan_off()` | `sudo -n btmgmt -i <idx> stop-find -l` then `stop-find`; rate-limited to 0.35 s; hard 1.5 s timeouts with `kill()` so a hung btmgmt can never block the hub |
| `_bluez_remove_device(mac)` | `bluetoothctl remove` + D-Bus `Adapter1.RemoveDevice` to drop the BlueZ `Device` ghost that would otherwise race/steal the ACL. **Must not be called while we hold a live session for that MAC** |
| `_CONNECT_LOCK` (module-level `threading.Lock`) | Most adapters allow only **one outstanding LE create-connection**; all dials serialize behind it |

Rust implication: these privileges (`sudo -n btmgmt`, `bluetoothctl`) are part of the
contract of the current product. Decide explicitly whether to keep them (shell-outs) or
replace with `mgmt` sockets — but **do not** silently drop the HCI-level scan stop; the
README claims zero-sudo userspace, yet the reliable path in practice required sudo.

### 3.3 Handshake order (`device.py:SwitchController.initialize`, lines 165-189)

```
1. _resolve_handles()                     ATT service/char/CCCD discovery; map UUID -> handle
2. enable_commands()                      subscribe WRITE-request to cmd-response CCCD
3. read_controller_info()                 memory read 0x40 @ 0x00013000
4. _resolve_vibration_handle()            pick per-PID vibration char UUID from _by_uuid
5. _read_calibration()                    left/right sticks (+ GC analog trigger neutral)
6. set_player_leds(player)                cmd 0x09 / sub 0x07
7. play_vibration_preset(SOFT=0x03)       cmd 0x0A / sub 0x02  (a "vibration test")
8. enable_features(0x03 | FEATURE_MOTION) cmd 0x0C / sub INIT 0x02 then ENABLE 0x04
9. [if has_hd_rumble] start HD rumble worker thread
10. subscribe input CCCD                  input notifications start flowing here
11. last_input_at = last_button_at = now; _last_buttons = 0
```

Each step is wrapped by `_retry(label, fn, attempts=3, delay=0.1)`; failure raises
`RuntimeError("<label> failed after 3 tries: …")` and the worker tears the partial
session down. `initialize()` is called **after** the ATT link is up, on the worker thread.

**Handle resolution** (`_resolve_handles`) is dynamic and tolerant: it discovers the full
GATT table and maps `INPUT_REPORT_UUID`, `COMMAND_WRITE_UUID`, `COMMAND_RESPONSE_UUID`
(+ their CCCDs) and the per-PID vibration UUID; if a characteristic is missing it **keeps
the GameCube default handle** so partial discovery never regresses. Defaults
(`device.py:26-31`): input `0x000A`/CCCD `0x000B`, vibration `0x0012`, command write
`0x0014`, command response `0x001A`/CCCD `0x001B`. In practice discovery succeeds and
the real hardware table is used; treat the defaults as an NSO-GC-only safety net.

**Identity** (`ControllerInfo.from_bytes`, 0x40 bytes): `serial = data[2:16]` ASCII,
`vid = u16le @18`, `pid = u16le @20`, colors in 3-byte triples at `25/28/31/34`.
`product_id` drives every behavioural branch (`has_hd_rumble`, `has_analog_triggers`,
`has_second_stick`, vibration UUID, button map, gamepad product id).

**Calibration**: each stick reads 0x0B bytes at the *user* address
(`0x001FC042` L / `0x001FC062` R); if `u24le(data[0:3]) == 0xFFFFFF` (unwritten) it
re-reads the *factory* address (`0x000130A8` / `0x000130E8`). Layout is three packed
12-bit XY pairs: center, max, min (`get_stick_xy` = `v&0xFFF`, `v>>12`).
GC analog triggers read 2 bytes at `0x00013140` (left/right neutral); values `0x00`/`0xFF`
are rejected and the default neutral `30` is kept.

### 3.4 Command plumbing

Framing (`protocol.build_command`):

```
[cmd][0x91][0x01][subcmd][0x00][len][0x00][0x00][data…]
```

* Written with **ATT Write Command (0x52)** — unacknowledged. The reply arrives later as
  a notification on the command-response characteristic.
* `write_command()` takes a mutex (`_cmd_lock`), clears a single `Event`, sends, waits up
  to 2 s (0.5–1 s for the sleep command), then validates
  `resp[0] == cmd && resp[1] == 0x01`, returning `resp[8:]`.
* `read_memory(length ≤ 0x4F, address)` = `CMD_MEMORY 0x02 / SUB_MEMORY_READ 0x04` with
  payload `len, 0x7E, 0x00, 0x00, addr_u32le`, validates echoed length and address,
  returns `data[8:]`.
* Correlated commands used: memory read, LEDs, vibration preset, feature init/enable,
  pairing (4 writes), host HCI sleep.

### 3.5 Bonding

`SwitchController.bond()` (`device.py:278`) runs four sequential commands:

```
PAIR 0x15 / SET_MAC 0x01 : 00 02 <host MAC LE> <host MAC LE>
PAIR 0x15 / LTK1    0x04 : PAIR_LTK1 (fixed 16-byte key, 0x00-prefixed)
PAIR 0x15 / LTK2    0x02 : PAIR_LTK2 (fixed 16-byte key, 0x00-prefixed)
PAIR 0x15 / FINISH  0x03 : 00
```

The controller stores host address + LTK, so afterwards it advertises the host MAC in
`manu[10:16]` and wakes on button press without re-entering pairing mode. The bridge
bonds automatically on first successful `activate()` when `entry.bonded == false`, then
persists `bonded: true` (`bridge.py:571-601`). `python -m ngc rebond` re-runs it for a
saved MAC.

> Rust note: bonding is the **only** writer of the state that discovery reads. If
> `adapter_mac` is wrong (e.g. adapter replaced), every bonded pad will advertise
> "other host" and be filtered out — the failure mode looks like "pad never wakes".

### 3.6 Event loops and threading (legacy topology)

```
main thread        Bridge.run(): creates DSUServer, one _Worker + thread per entry,
                   starts _ConnectHub thread, starts state loop thread, waits on stop
ngc-hub thread     one asyncio loop for the hub's whole lifetime (bleak's BlueZ
                   scanner manager is a process-wide singleton bound to the first loop;
                   asyncio.run() per attempt pinned it to a dead loop — see bridge.py:267)
                   loop: 0.5 s scan window -> scan.stop() -> 0.10 s settle
                         -> collect recently-seen disconnected MACs (TTL by mode)
                         -> prepare_bluez_global() + _bluez_remove_device()
                         -> async with connect lock: run_in_executor(_connect_sync)
                         -> sleep 0.025/0.05 s -> repeat
ngc-connect pool   ThreadPoolExecutor(max_workers=2) for the blocking dial
                   (_CONNECT_ATTEMPTS=16 × _CONNECT_ATTEMPT_S=0.80 s per dst type)
ctrl-N thread      _Worker.run(): wait _ready(1 s poll) -> monitor disconnect/idle ->
                   teardown; on budget exceeded sends sleep + closes link
att reader thread  blocking recv() loop; dispatches 0x1B Handle-Value-Notifications to
                   SwitchController._on_notification -> worker input callback
ff thread          gamepad._ff_loop(): blocking uinput read_loop() for EV_FF / upload/erase
hd rumble thread   SwitchController._hd_loop(): ~60 Hz motor packet resend while active
dsu thread         DSUServer._recv_loop(): UDP request/response + broadcast
ngc-state thread   every 1.5 s: apply led-players.json overrides, write state.json
```

Important consequences to preserve (or deliberately change) in Rust:

* **Input is callback-driven from the ATT reader thread.** Any blocking work in the
  callback stalls not only input but the notification channel used by command responses.
* **Scanner restart on idle**: after `_HUB_IDLE_RESTART_CYCLES = 80` (~20 s) with
  disconnected pads and no pending adverts, the hub recreates `BleakScanner` and clears
  seen/logged state (stuck-scanner mitigation).
* **Error backoff** (`_run_async`): exponential 1 s → 30 s; on
  `BleakDBusError("InProgress")` it re-runs `prepare_bluez_global()` + scan-off before
  retrying.
* **Supervision is external**: `systemd` unit (`Restart=always`) plus
  `bazzite-nso-gc-watchdog.timer` → `.sh` which restarts the service when
  `state.json` says *0 connected for > 600 s*, *stale state > 90 s*, *hub_error set*, or
  *"saw …" adverts in the journal but no "connected"*. Critically it **never restarts
  while any pad is connected** (that would tear down live sessions).
  `nso-gc-after-gamescope.sh` does a `try-restart` 22 s after Game Mode starts, only if
  0 pads are connected — an earlier unconditional restart killed live sessions.

### 3.7 Disconnect, teardown, idle sleep, reconnect

* Disconnect is detected by `recv()` returning empty / `OSError` in the reader thread →
  `disconnect_cb` → `_Worker._on_disconnect` sets `_disconnected`.
* `_teardown_session(full=False)` on link loss: detach rumble callback,
  `gamepad.release_all()` (**keeps the uinput node alive** so Steam/SDL do not see the
  pad vanish and re-enumerate), clear DSU slot, close controller, fire topology-change
  hook. `full=True` (only at shutdown) also closes the gamepad/IMU nodes.
* Reconnect is **not** attempted by the worker; the worker loops back to waiting on
  `_ready` and the hub eventually sees a new advert. No blind retry loop.
* **Idle sleep**: if `now - last_button_at >= config.idle_sleep_s` (default 300 s, env
  `NGC_IDLE_SLEEP_S`, `NGC_IDLE_SLEEP=0|false` disables), the worker sends
  `HOST 0x01 / HCI_STATE 0x06` with `[0x00]` (1 s timeout) and closes the link, then
  `_disconnected.set()`. Note the timer uses *button* activity only, so stick-only use
  counts as idle.
* **Status plane**: `state.json` (atomic write via `state.json.tmp` + `os.replace`) holds
  `hub_alive`, `hub_scanning`, `hub_error`, `service`, `headline`, `detail`, and per-pad
  `{mac, player, name, bonded, connected, battery_pct, battery_mv}`. GUI/Decky/watchdog
  consume this instead of scraping logs — a good pattern to keep (as JSON or D-Bus).
* **Topology-change hooks**: `on_topology_change` schedules (2 s debounce) the three
  reorder scripts (`bazzite-reorder-on-nso-connect.sh`, `bazzite-dolphin-apply-gcpad1.sh`,
  `bazzite-eden-reset-controllers.py`), disabled with `NGC_AUTO_REORDER=0`.
* `SIGUSR1` pulses a mapped-button hotkey (`C`+`R` on a GameCube pad, 0.12 s) for
  "Dolphin save state" — niche, but shows the bridge injects synthetic input.

### 3.8 Input report decode (for completeness)

63-byte report: `timestamp u32le @0`, `buttons u32le @4`, left stick 3 packed bytes
`@10:13`, right stick `@13:16`, `battery_mv u16le @0x1F`, accel 3×`i16le @0x30..0x36`,
gyro 3×`i16le @0x36..0x3C`, `left_trigger u8 @0x3C`, `right_trigger u8 @0x3D`.
`calibrated_input()` applies deadzone-free calibration, maps analog triggers via
`normalize_trigger()` (neutral-relative, deadzone 35, click threshold 209), and
**forces ±255 when the digital L/R click bits are set** so the axis agrees with the
shoulder button. For non-GC pads ZL/ZR digital bits drive the trigger axes 0/255.

---

## 4. Controller position / slot assignment

### 4.1 Persisted player assignment (bridge-owned)

`config.json` (`$XDG_CONFIG_HOME/nso-gc/config.json`, backup `.bak`, atomic replace):

```json
{
  "controllers": [{ "mac": "AA:..", "player": 1, "name": "…", "bonded": true }],
  "adapter_mac": "…", "button_map": {}, "enable_rumble": true,
  "gc_impact_threshold": 0.52, "idle_sleep_s": 300.0
}
```

Rules (`config.py`):

* `player` is **1-based, 1..8**. `add_controller()` with no explicit player takes the
  lowest free slot in 1..8; error past 8 (`"maximum 8 controllers already saved"`).
* Explicit `--player N` is validated against slots used by *other* MACs (→ `ValueError`
  "player N already in use"). `swap_players(a,b)` exchanges two saved entries.
* Legacy single-pad configs (`controller_mac` + `player`) are auto-migrated into
  `controllers[0]` on load; `save()` drops the legacy keys.
* Config load is defensive: unreadable → fall back to `.bak` → fall back to defaults;
  unknown keys are filtered against dataclass fields.
* `bonded` is a persisted bool, set after a successful `bond()`.

### 4.2 LEDs and the external source of truth

* Player LEDs are **not** sysfs for these pads; they are BLE command
  `LEDS 0x09 / SET_PLAYER 0x07` with a 4-byte payload whose first byte is the bit
  pattern (`LED_PATTERN`: P1 `0x01`, P2 `0x03`, P3 `0x07`, P4 `0x0F`, P5 `0x09`,
  P6 `0x05`, P7 `0x0D`, P8 `0x06`), clamped 1..8. `controller.py` additionally had a
  `reversed_` bit-reverse option (unused).
* The bridge applies LEDs at connect (`initialize(player=effective_player())`) and then
  re-applies them live every 1.5 s from `~/.config/nso-gc/led-players.json`
  (`Bridge._apply_led_overrides`, `_led_player` remembers the last pushed value).
* `led-players.json` is written by `system/bazzite-set-player-leds.py`; it is the
  **authoritative mapping from emulator player order → LED number**:

  ```json
  { "AA:BB:..": 1, "CC:DD:..": 2 }
  ```

  That script picks a mode (Eden / Dolphin) by sniffing the running emulator process
  (`/bin/eden`, `eden-game.sh`, Dolphin, Ryujinx → eden-style) or a remembered mode file,
  then orders pads by *kind priority*:
  * **Dolphin/global**: `gamecube_nso` → `exlene` → `switch2_pro`/`joycon2*` → `dualsense`
  * **Eden/Ryujinx (Switch)**: `switch2_pro`/`joycon2*` → `gamecube_nso` → `dualsense` →
    `n64_nso`
  Non-bridge pads get sysfs player LEDs (and DualSense lightbar colours) instead.

### 4.3 DSU motion slot

`_Worker.slot = clamp(entry.player - 1, 0, 3)` — computed **once at construction**
(`bridge.py:495`). `DSUServer` has exactly 4 slots; `set_slot(slot, connected, mac,
battery_mv)` on activate/deactivate, `update(slot, report, sticks, triggers)` per input
frame (sticks → 0..255 with 128 neutral; triggers lt/rt).

Known limitations to fix in Rust:
* Players 5–8 all collapse onto DSU slot 3 (silent collision).
* `slot` uses `entry.player`, while LEDs use `effective_player()` (the
  `led-players.json` override). So an override changes LEDs but **not** the DSU slot,
  gamepad name (`"… (P{entry.player})"`), or emulator ordering → the three can disagree.

### 4.4 Downstream (emulator) ordering — `bazzite-controller-detect.py`

Even though the bridge only knows "player", the *real* player order for Dolphin /
RetroArch / Eden is computed by the detector script. Key constants (lines 28-45):

```
FIXED_KIND_SLOTS = {
  gamecube_nso: 0, exlene: 1, switch2_pro: 2, dualsense: 3, mcon: 4, n64_nso: 5 }
```

i.e. **fixed, non-compacting slots** — a missing pad leaves its slot empty rather than
shifting everyone up. `enrich_pad()` assigns `steam_slot = FIXED_KIND_SLOTS[kind]`,
falling back to Steam's `player_slot_led` from `preferences_*.vdf`
(`read_steam_slot`) or per-MAC `FIXED_STEAM_SLOTS`; `order_pads()` also supports
`BAZZITE_PAD_PRIORITY` (e.g. `n64_nso` wins P1 for N64 cores), Gemma-profile overrides,
and Remote-Play Xbox pads taking P1 (`want_remote_xbox_p1`). Then:

* Dolphin: `_pads_for_dolphin_slots()` → `GCPad1..4` + Wiimote ports by fixed slot,
  `Device = SDL/<n>/<name>` or `evdev/<idx>/<name>` where `<n>` is the **Nth occurrence
  of that device name** (not the global index) — see `finalize_device_indices`.
* RetroArch: always `input_joypad_driver = "sdl2"`, `input_playerN_joypad_index = <sdl_idx>`
  because udev ordering is syspath-dependent and unstable.
* Eden: per-player blocks + per-GUID SDL binding strings; Eden's `port` is the Nth
  joystick **with that GUID** (≈0), and GUID bytes 2-3 (name CRC) are zeroed to match
  Eden's `SDLDriver::GetGUID`. Also `DSU_GUID = "0000000000000000000000007f000001"`
  pointing at `127.0.0.1:26760` for motion.
* New pads are recognised **by uinput name string** (`classify()`):
  `"gamecube"` → `gamecube_nso` (checked before `"pro controller"`),
  `"pro controller 2"`/`"switch 2 pro"` → `switch2_pro`,
  `"joy-con 2"` (+`left`/`right`) → `joycon2_left|right|joycon2`.
  So the Rust daemon's virtual-pad **names are part of the integration contract**.

### 4.5 Joy-Con L/R pairing logic — what actually exists

**Legacy reality (do not over-read it):**

1. Left/Right have distinct PIDs (`0x2067` / `0x2066`) and distinct vibration write UUIDs
   (`vibration_uuid_for`: Joy-Con L `289326cb-…`, Joy-Con R `fa19b0fb-…`, else the "pro"
   `cc483f51-…`).
2. Drift-free motor payload sizing: Pro Controller gets **two** duplicated motor blocks,
   everything else (incl. Joy-Con) gets **one** (`device.py:_write_motor`).
3. `has_second_stick` is `PID in {PRO, NSO_GC}` → Joy-Cons never read right-stick
   calibration in `controller.py` (the legacy bleak class), while `device.py` reads both
   unconditionally → Joy-Con R/L calibration semantics were never validated.
4. **No pairing, no combining, no splitting, no sideways orientation, no per-side button
   or stick remapping.** `gamepad.py` always builds the full dual-stick layout with
   `button_map_for_product()` returning only two maps (`GAMECUBE_BUTTON_MAP`,
   `PRO_BUTTON_MAP`); a single Joy-Con therefore presents a full-controller map. The
   README says so explicitly ("Joy-Con 2 is untested… currently reuses the
   full-controller layout").
5. Each Joy-Con is an independent config entry with its own `player` and its own uinput
   pad; nothing correlates an L with an R.

**Therefore the Rust `Combined` / `Split` modes are new work.** Design inputs the legacy
code does provide:

* Identity per side is available from the advertisement PID before connecting, so a
  "pair" can be formed by (a) explicit config (left MAC + right MAC share a `pair_id`),
  or (b) opportunistic autodetect of the complementary PID. Explicit is far more
  predictable; note the legacy ordering tools key off *uinput names* and per-MAC LED
  entries, so combined mode must publish a name they classify (or accept being treated
  as `generic`).
* Slot semantics to respect: Joy-Cons should occupy **one** player slot when combined
  (one LED pattern, one DSU slot, one gamepad) and **two** in split mode. Legacy's
  `player` field is per-MAC, so a combined pair needs an explicit slot owner and the
  partner marked non-owning (otherwise both fight over the same LED pattern and DSU slot).
* Per-side quirks to carry over: single motor block, per-side vibration UUID, digital
  ZL/ZR, no right stick on L / no left stick on R, sideways IMU axis remap (not
  implemented anywhere in `.old/` — needs hardware validation).
* The combined pad's IMU node must keep `phys="ngc/<MAC>"` (or a pair-derived MAC) and
  consistently set `vendor=0x057E`, `bustype=BUS_BLUETOOTH`, `product=<pid>` if you want
  SDL/Eden to bind it like a real pad (see `gamepad.py:126-136`, `motion_evdev.py`).

### 4.6 Button-map / evdev mapping contract (`gamepad.py`)

`PRO_BUTTON_MAP`: A→`BTN_SOUTH`, B→`BTN_EAST`, X→`BTN_WEST`, Y→`BTN_NORTH`,
L/R→`BTN_TL`/`BTN_TR`, ZL/ZR→`BTN_TL2`/`BTN_TR2`, PLUS→`BTN_START`,
MINUS→`BTN_SELECT`, HOME→`BTN_MODE`, CAPTURE→`BTN_Z`, C→`BTN_C`,
sticks→`BTN_THUMBL`/`BTN_THUMBR`.

`GAMECUBE_BUTTON_MAP` deviates deliberately: X→`BTN_NORTH`, Y→`BTN_WEST` (physical GC
diamond), **ZL and L both → `BTN_TL`**, **ZR and R both → `BTN_TR`** (so Z and the R
trigger full-click share a bumper), PLUS→`BTN_START` (GC Start is the PLUS bit),
C→`BTN_SELECT`, CAPTURE→`BTN_Z`.

Axes: `ABS_X/Y/RX/RY` in ±32768 with **Y inverted** (`-scale`), `ABS_Z`/`ABS_RZ`
0..255, `ABS_HAT0X/Y` from the D-pad bits. Events are emitted **only when changed** and
followed by `SYN_REPORT`. Force-feedback: upload/erase tracked in
`_effects[id] = (strong, weak)`, `EV_FF` value≠0 → look up magnitudes /65535 →
`rumble_cb`; value 0 → `(0,0)`.

Rumble semantics (`device.py:set_rumble`):
* HD-capable pads: store `(strong, weak)` and let a ~60 Hz worker resend
  `VibrationData(lf_freq=0x0E1, lf_amp=int(min(1, strong+0.5*weak)*0x3FF))` three
  sub-samples per block (fills the ~1/3 duty cycle); stop packet when idle.
* GC pad: presets only — `<0.06` ignored, `≥0.52` → preset `0x02` (strong), else `0x03`
  (soft), retriggered every 0.22 s while held; preset `0x01` (medium) deliberately unused
  ("reads too harsh").

Battery: percent from `mV` with `BATTERY_EMPTY_MV=2950`, `BATTERY_FULL_MV=4200`;
DSU battery enum: ≤0→0, ≥4000→5, ≥3800→4, ≥3600→3, ≥3400→2, else 1.

---

## 5. Invariants and pitfalls the Rust port must not lose

1. **`BT_SECURITY_LOW` always.** Any SMP/pairing upgrade kills the link. Do not "fix" it.
2. **Scanning and connecting are mutually exclusive.** Stop LE discovery at the HCI level
   before dialing; serialize all connects (one outstanding create-connection). Treat
   `BleakDBusError:InProgress`-equivalent (`EALREADY`/`EBUSY`/`EAGAIN` on
   `connect`, or mgmt "busy") as "scan still held" and clear it, don't spin.
3. **Drop the BlueZ `Device` object before a raw connect** (else BlueZ races/steals the
   ACL), but **never while you own a live session for that MAC**.
4. **Only the advertisement's host-MAC field decides wake-vs-pairing.** `0` = pairing
   (accept only when the user asked to pair), host match = wake, other = ignore,
   absent = accept. Persist `adapter_mac`; wrong adapter ⇒ "pad never wakes".
5. **Single outstanding ATT request.** The legacy code has one response slot guarded by a
   lock; a Rust implementation should use request/response correlation by opcode+handle
   (or a serialized command queue) and keep notifications on a separate path.
6. **Notifications arrive on the reader thread.** Input decode, uinput writes, DSU
   broadcast must not block. Prefer: reader task → bounded channel → consumer.
7. **Command writes are unacknowledged**, correlated only by the response notification
   (`resp[0] == cmd`, `resp[1] == 0x01`). Validate and time out (2 s) rather than
   assuming success; the handshake has 3-try retries per step.
8. **Dynamic GATT discovery with model fallbacks.** Never hard-code handles for new
   models; keep GC defaults as a safety net and log which handles were used (all the
   `tools/*` diagnostics print this).
9. **Keep the uinput node alive across disconnects** (`release_all()`, not `close()`), or
   Steam/emulators re-enumerate and player order shuffles. Full close only at shutdown.
10. **Player slot is 1..8, DSU slot is `player-1` capped at 3.** Decide explicitly whether
    to cap configured players at 4 (all motion slots usable) or keep 8 at the cost of
    DSU collisions. Keep LED pattern table for 1..8.
11. **`led-players.json` is an external override** written every time the detector runs.
    Decide whether to keep it (recommended for compatibility) and ensure the override is
    applied consistently to LEDs, DSU slot, gamepad name, and emulator ordering — legacy
    applied it only to LEDs.
12. **Virtual pad names are a public contract** (`gamecube`, `pro controller 2`,
    `joy-con 2 [left|right]`, `(P<n>)` suffix) for `classify()` and for user-facing
    Steam/emulator configs. Also keep `vendor=0x057E`, `bustype=BUS_BLUETOOTH`,
    `phys="ngc/<MAC>"`, and the `INPUT_PROP_ACCELEROMETER` IMU node.
13. **Timing constants that matter** (measured on hardware): scan window 0.50 s, settle
    0.10 s, per-attempt connect wait 0.80 s, 16 attempts, connect-lock rate limiting,
    scan-off min interval 0.35 s, wake TTL 4 s / pairing TTL 45 s, hub idle scanner
    restart ~20 s, status publish 1.5 s, HD rumble 16 ms, GC preset retrigger 0.22 s,
    idle sleep 300 s.
14. **Supervision must never restart while pads are connected.** The watchdog's
    "0 connected for >600 s / stale state >90 s / hub_error" logic is the safe version;
    keep that shape if you move to a Rust supervisor.
15. **Adapters vary on address type.** Public-then-random with last-good memory; don't
    assume public.
16. **MTU**: request 247; a 23-byte MTU produces truncated input reports — check the
    negotiated MTU (or report length) and warn/fail rather than mis-parse silently.

---

## 6. Suggested Rust module mapping (informational)

| Legacy | Existing Rust | Gap / recommendation |
| --- | --- | --- |
| `protocol.py` | `protocol/{constants,command,report,calibration}.rs` | Add advert parsing (`manufacturer_data` → vid/pid/reconnect MAC) with LE/BE unit tests; keep button bitmask + LED table together |
| `att.py` | `bluetooth/{l2cap,att}.rs` | Verify `sockaddr_l2` layout, `BT_SECURITY_LOW`, public/random fallback, MTU validation, request correlation |
| `scanner.py` + hub advert filtering | *missing* (daemon only retries connects) | **Highest-value missing piece**: a scan/connect scheduler with host-MAC filtering, TTLs, and HCI scan-stop. Without it, wake-connect behaves like the old blind retry loop |
| `device.py` | `controller.rs` | Confirm handshake ordering, 3-retry steps, dynamic handles, bond sequence, idle sleep, GC preset vs HD rumble mapping |
| `bridge.py` hub/workers | `daemon.rs` | Current version is `while running { connect; sleep }` with a connect mutex — no advertisement gating, no TTLs, no BlueZ hygiene, no state plane, no idle sleep (config has `idle_sleep_seconds` but it is unused) |
| `gamepad.py` / `motion_evdev.py` | `uinput/{gamepad,combined}.rs` | Keep names/phys/product; combined-mode contract is new |
| `dsu.py` | *missing* | Motion backend for Dolphin/Eden; slot policy decision (§5.10) |
| `config.py` | `config.rs` | Has `player`, `bonded`, `mode`; lacks adapter fallback detection, legacy migration, `led-players.json`, swap/remove CLI parity |
| `status.py` | *missing* | Needed by GUI/Decky/watchdog and by any integration tests |
| Joy-Con pairing | *new* | Define `pair_id`/combined-slot ownership + sideways mapping; needs hardware validation plan |

## 7. Open questions for the Architect

1. Do we keep the **sudo `btmgmt`/`bluetoothctl` shell-outs** (reliable in practice) or
   replace them with `mgmt`-socket/BlueZ work (no sudo, more code)? The README's
   "zero-sudo" claim does not currently match the wake path.
2. Is **DSU/cemuhook** in scope for v1? It is required for gyro in Dolphin/Eden/Ryujinx
   and its slot rule interacts with the player-slot decision.
3. Do we keep **`led-players.json`** as the cross-tool contract, or replace it with a
   bridge-owned IPC/CLI (`switch2d set-player --mac … --player …`) and re-order emulators
   ourselves?
4. Combined Joy-Con: explicit config pairing (`left_mac`/`right_mac` + one slot) or
   autodetect complementary PIDs? And which uinput name do the Bazzite detectors get —
   e.g. `Nintendo Switch 2 Combined Joy-Cons` currently classifies as **`generic`**, not
   `joycon2`, so the Eden/Dolphin profile choice would be wrong.
5. Player slots: cap at 4 (clean DSU mapping) or support 8 (LED patterns exist)?
6. Should the daemon own **USB** (future) behind the same transport-agnostic protocol
   layer, or is BLE-only acceptable for v1?

---

## 8. Follow-up clarifications (requested by t13)

### 8.1 `att.py:discover_cccds` (lines 330-355) — exact behaviour

```python
for idx, ch in enumerate(chars):
    scan_start = ch.value_handle + 1
    scan_end   = chars[idx+1].decl_handle - 1 if idx+1 < len(chars) else service.end
    if scan_start > scan_end: continue
    handle = scan_start
    while handle <= scan_end:
        req = struct.pack("<BHH", ATT_FIND_INFO_REQ(0x04), handle, scan_end)
        try: resp = self._request(req)
        except ATTError: break
        if resp[0] != ATT_FIND_INFO_RSP(0x05): break
        entry_len = 4 if resp[1] == 1 else 18          # fmt 1 = 16-bit UUIDs, else 128-bit
        for each entry in resp[2:]:
            h    = u16le @ entry[0]
            uuid = uuid_to_str(entry[2:entry_len])      # 16-bit -> "2902"
            if uuid == "2902": ch.cccd_handle = h       # NOTE: last match wins, no break
        if last == 0 or last >= scan_end: break
        handle = last + 1
```

Confirmed facts:

* Yes — **`ATT_FIND_INFO_REQ` (0x04)**, range `[value_handle + 1, next_characteristic.decl_handle - 1]`,
  or `[value_handle + 1, service.end]` for the last characteristic in the service. The end
  handle is **never** `value_handle + 1`.
* It assigns **every** `0x2902` it finds in the range; there is no `break` after a match, so
  the effective value is the **last/highest-handle** CCCD in that range (identical to "the
  first" in practice: one CCCD per characteristic).
* **No CCCD ⇒ no fallback.** `Characteristic.cccd_handle` stays `None`. The only fallback
  happens one layer up in `device.py:_resolve_handles`:
  `cccd(uuid, default) = ch.cccd_handle if ch and ch.cccd_handle else default`, with the
  **hard-coded GameCube defaults** `h_input_cccd = 0x000B`, `h_cmd_resp_cccd = 0x001B`.
  There is **no** `read(value_handle + 1)` probe or any other derivation. `discover_all()`
  failure also leaves all defaults in place (`device.py:117-125`, returns early).
* Failure modes that silently leave `None`: `ATTError` (incl. `0x0A` attribute-not-found when
  the descriptor range is empty), non-`FIND_INFO` response, or `scan_start > scan_end`
  (adjacent characteristics with no descriptor gap).
* `_by_uuid` is a flat `dict` keyed by UUID string (built after full discovery, last service
  wins on duplicate UUIDs); the vibration handle is resolved from it separately
  (`_resolve_vibration_handle`), and `_by_uuid` does not exist at all if discovery threw.

### 8.2 Vibration characteristic payload (exact)

Handle: `h_vibration`, resolved from `vibration_uuid_for(pid)` via `_by_uuid`
(L `289326cb-…`, R `fa19b0fb-…`, Pro/GC `cc483f51-…`); falls back to `0x0012`.

**5-byte sample** (`VibrationData.to_bytes`, one `u40le`):
bit 0-8 `lf_freq` (mask 0x1FF) · bit 9 `lf_en_tone` · bit 10-19 `lf_amp` (0x3FF) ·
bit 20-28 `hf_freq` (0x1FF) · bit 29 `hf_en_tone` · bit 30-39 `hf_amp` (0x3FF).
Defaults: `lf_freq=0x0E1`, `hf_freq=0x1E1`, all amps 0, tones false.

**Block** = `header(1 byte) + sample×3` = 16 bytes, where
`header = 0x50 + (packet_id & 0x0F)` (cycles `0x50..0x5F`, incremented after every write).

**(a) Pro Controller 2** — 33-byte payload: `00 | block | block`

```
00 5X e1 00 10 1e 00 | e1 00 10 1e 00 | e1 00 10 1e 00   <- block 1 (3 sub-samples)
   5X e1 00 10 1e 00 | e1 00 10 1e 00 | e1 00 10 1e 00   <- block 2, byte-identical
```
(both blocks carry the **same** header byte; left and right motors get the same waveform)

**(b) Joy-Con 2 (and every non-Pro PID, incl. NSO GC)** — 17-byte payload: `00 | block`

```
00 5X e1 00 10 1e 00 e1 00 10 1e 00 e1 00 10 1e 00
```

Sample byte encodings (verified against the Python implementation):

| case | sample bytes |
| --- | --- |
| stop / zero amp | `e1 00 10 1e 00` |
| lf_amp 0x0FF (0.25) | `e1 fc 13 1e 00` |
| lf_amp 0x1FF (0.5) | `e1 fc 17 1e 00` |
| lf_amp 0x3FF (1.0) | `e1 fc 1f 1e 00` |
| both bands full | `e1 fc 1f de ff` |
| dual band + tone bits | `e1 82 1c 3e 70` |

**Stop packet** = `VibrationData()` (defaults ⇒ zero amplitudes but **frequencies remain
`0x0E1`/`0x1E1`**), i.e. Pro: `00 5X e1 00 10 1e 00 ×3 ×2`, Joy-Con:
`00 5X e1 00 10 1e 00 ×3`. Sent once when the target magnitudes drop to ≤0.001
(`_hd_loop`), then the worker waits on its event with a 1 s timeout; the sustain loop
re-sends the active waveform every **16 ms**.

**Transport:** `att.write_command(handle, payload)` ⇒ **ATT Write Command (opcode 0x52),
unacknowledged** — no ATT response, no error feedback, no CCCD subscription. Consequences:
fire-and-forget (a lost packet just means one dropped 16 ms frame), and PDU size matters —
33 bytes of payload needs `ATT_MTU ≥ 36`; at the 23-byte default MTU this write is
impossible, so a Rust port must check the negotiated MTU (legacy does not).

(For contrast, the obsolete `controller.py:set_vibration` used **one** sample per block, no
per-PID UUID, and always duplicated the block — do not port that variant.)

### 8.3 `gamepad.py` FF handling — exact fields and conversion

uinput FF capabilities declared: `FF_RUMBLE` (0x50), `FF_PERIODIC` (0x51),
`FF_CONSTANT` (0x52), `FF_GAIN` (0x60).

* `EV_UINPUT` / `UI_FF_UPLOAD` (0x01): `upload = ui.begin_upload(event.value)`;
  **only** `effect.type == FF_RUMBLE` is stored:
  `self._effects[effect.id] = (effect.u.ff_rumble_effect.strong_magnitude,
                               effect.u.ff_rumble_effect.weak_magnitude)`
  — Linux `struct ff_rumble_effect`, both `__u16` **0..65535** (heavy vs light motor);
  `effect.id` is a signed 16-bit id. `upload.retval = 0`; `ui.end_upload(upload)`.
  No `ff_replay.length/delay`, no `ff_trigger`, no `direction`, no gain are read.
* `EV_UINPUT` / `UI_FF_ERASE` (0x02): `begin_erase(event.value)` → `_effects.pop(erase.effect_id)`;
  `retval = 0`.
* `EV_FF` (0x15) with `code == effect id`, `value == play count`:
  * `value == 0` → `rumble_cb(0.0, 0.0)` (stored magnitudes are **not** cleared);
  * otherwise `strong, weak = _effects.get(code, (0, 0))` →
    `rumble_cb(strong / 65535.0, weak / 65535.0)` (plain u16 → 0..1 float, no gain
    multiplication, no envelope, no clamping beyond the u16 range).
* `rumble_cb` is `_Worker._on_rumble` → `SwitchController.set_rumble(strong, weak)`, invoked
  on the FF reader thread. `SwitchGamepad.close()` calls `rumble_cb(0.0, 0.0)` once;
  `release_all()` does not touch rumble.

Deliberate/accidental gaps to decide on for the Rust port:

* Non-rumble effect types are **silently ignored**: a periodic/constant effect uploads but
  never enters `_effects`, so playback resolves to `(0,0)` ⇒ silent.
* `FF_GAIN`/`FF_AUTOCENTER` are declared as capabilities but not handled; they arrive as
  `EV_FF` with `code = 0x60` / `0x61`, fall into the `_effects.get(code, (0,0))` path and so
  **turn rumble off** (and would alias a real effect whose id is 96/97).
* `ff_replay.length` is ignored, so a finite-length effect keeps rumbling until an explicit
  `EV_FF` stop/erase arrives.
* Binding quirk (python-evdev 2.0.0): `begin_upload/begin_erase` set `upload.effect_id`
  (a stray attribute) and never populate `request_id`, so the kernel ioctl always sees
  `request_id = 0`. Correct Rust behaviour is
  `request_id = event.value`, `retval = 0`, then `UI_END_FF_UPLOAD` — and for erase
  `request_id = event.value` with `effect_id` cleared.

---

## 9. Hub/worker internals (requested by t13, for tasks 04 and 08)

### 9.1 `_ConnectHub._scan_loop` — exact per-iteration sequence

Outer wrapper `_run_async()` keeps **one** asyncio loop for the hub's life; any exception
escaping `_scan_loop` sets `hub_error`, sleeps with backoff 1 s → 2 → 4 … capped 30 s,
and on `BleakDBusError:"InProgress"` runs `prepare_bluez_global()` + `_force_le_scan_off()`
before the sleep. A clean return resets `backoff=1.0, fails=0`.

One iteration (`bridge.py:299-433`):

| # | Step | Detail / duration |
| --- | --- | --- |
| 1 | Snapshot | `workers = list(workers_by_mac.values())`; `disconnected = [w for w in workers if not w.is_connected()]` |
| 2 | Early out | If **no** disconnected worker: `_scanning=False`, `sleep(1.5)`, `continue` — **no scanning at all while every configured pad is connected**, and the idle counter is *not* touched in this branch |
| 3 | Bookkeeping | `connected_count = len(workers) - len(disconnected)` (only used for the final sleep) |
| 4 | Scan start | `_scanning = True`; `await scanner.start()` with up to **3** attempts; each `BleakDBusError` containing `"InProgress"` ⇒ `_scanning=False`, `prepare_bluez_global()`, `_force_le_scan_off()`, `sleep(0.5 * attempt)` (0.5/1.0/1.5 s), retry; any other exception re-raises; after 3 failures raises `BleakDBusError("…still held by another client after 3 clears")` (⇒ outer backoff restart) |
| 5 | Scan window | `sleep(0.50)` (`scan_on_s`) — **adverts are only collected here**, via the `on_adv` callback |
| 6 | Scan stop | `finally: await scanner.stop(); _scanning = False` |
| 7 | Settle | `sleep(_SCAN_SETTLE_S = 0.10)` |
| 8 | Select pending | `now = time.monotonic()`; pending = MACs with a worker, `not worker.is_connected()`, `_last_seen[mac]` present, and `now - seen_ts <= _seen_ttl(mode)`; **sorted by seen timestamp descending** (freshest first) |
| 9 | Prep if pending | `prepare_bluez_global()` (pkill `decky-bluetooth-wake-control`, `bluetoothctl scan off`, D-Bus `Adapter1.StopDiscovery`, `_force_le_scan_off(force=True)`); then for **every** pending MAC still disconnected `_bluez_remove_device(mac)` (`bluetoothctl remove` + `busctl … RemoveDevice`); then `sleep(0.08)` |
| 10 | Connect | `async with _connect_lock:` for each pending MAC in order (see below) |
| 11 | TTL sweep | For every `_last_seen` entry with `now - seen_at > _seen_ttl(mode)`: pop from `_last_seen`, discard from `_logged`. Uses the `now` captured **before** the connect phase, so a long connect cannot expire entries mid-cycle |
| 12 | Idle counter | If `disconnected and not pending`: `_idle_scan_cycles += 1`; on `>= _HUB_IDLE_RESTART_CYCLES (80)`: reset to **0**, clear `_last_seen`/`_logged`, `prepare_bluez_global()`, stop scanner (errors swallowed), recreate `BleakScanner(detection_callback=on_adv)`. **Else: `_idle_scan_cycles = 0`** |
| 13 | Cadence | `sleep(0.05 if connected_count else 0.025)` |
| 14 | Loop | back to 1 |

### 9.1.1 `recently_seen` (`_last_seen`) recording and TTL

Recorded only in the detection callback `on_adv` (installed per scanner instance):

```python
addr = device.address.upper()                     # key: UPPERCASE MAC string
if hub.stop.is_set() or not hub._accept(addr, adv): return
reconnect = P.reconnect_mac_from_advertisement(adv)
mode = "pairing" if reconnect == 0 else "wake"     # None (no manu block) => "wake"
hub._last_seen[addr] = (time.monotonic(), mode)    # value: (monotonic ts, mode)
if addr not in hub._logged: hub._logged.add(addr); log "saw <mac> (<mode>)"
```

* Timestamp = `time.monotonic()` **at advert reception** (not at connect time), overwritten
  by every subsequent advert for that MAC.
* `_accept()` is the filter: a worker must exist for that MAC, it must not be connected,
  and (`reconnect is None` or `host_mac is None` or `reconnect in (0, host_mac)`). Pads
  not in config are never recorded.
* TTL is a pure function of the last mode: `_seen_ttl("pairing") = 45.0 s`,
  `_seen_ttl("wake") = 4.0 s` — applied in step 8 (selection) and step 11 (sweep).
  Consequence: a pad that advertises once in wake mode must be dialed within ~4 s or the
  next scan cycle re-discovers it; pairing mode survives ~45 s / ~70 cycles.
* `_logged` is a log-dedupe set only; entries are discarded on successful connect and on
  TTL expiry (and wholesale on idle restart). A failed connect does **not** clear
  `_last_seen` (so the pad is retried next cycle while inside its TTL).

### 9.1.2 Two (or more) pads due in the same cycle

* Both appear in `pending` (sorted freshest-first) and are dialed **strictly serially**
  inside a single `_connect_lock` acquisition: `await run_in_executor(_connect_sync, mac)`
  per MAC. The lock is held across the whole loop body, and `_connect_sync` additionally
  takes the **module-level** `_CONNECT_LOCK`, so no two raw connects ever overlap.
* Per MAC, `_connect_sync` retries `_CONNECT_ATTEMPTS = 16` outer attempts × up to 2
  destination address types (last-good first) × `_CONNECT_ATTEMPT_S = 0.80 s` each — worst
  case ≈ **25.6 s per MAC**, all while the second pad waits. Practical effect: with two
  pads due at once, pad B's connect can begin ~5–25 s later; its `_last_seen` still counts
  because `pending` was frozen before dialing, but the *next* cycle's TTL check uses fresh
  `now`, so B may need another advert round.
* Success path per MAC: log, `_last_seen.pop(mac)`, `_logged.discard(mac)`; the
  `_ready.set()` that wakes the worker comes from inside `_connect_sync` →
  `worker.activate()`, **not** from the scan loop.
* Failure path per MAC: log, then `_force_le_scan_off(force=True)` (bypasses the 0.35 s
  rate limit) before the next MAC is attempted.
* The `_connect_lock` is an `asyncio.Lock` created per `_scan_loop` invocation, so it is
  recreated if the hub restarts.
* Note the executor has `max_workers=2` but that is irrelevant in practice: the asyncio
  lock serializes submissions.

### 9.2 `_publish_state` / `_apply_led_overrides` / `state.json`

`_state_loop` (thread `ngc-state`):

```python
while not self._stop.wait(_STATUS_INTERVAL_S):   # 1.5 s
    try: self._apply_led_overrides(); self._publish_state()
    except Exception: logger.debug("state publish failed: %s", exc)
```

So both run **every 1.5 s, LED first, state second**, and a failure in either is only a
debug log (the loop never dies). `_publish_state()` is *also* called event-driven from
`Bridge.run()` (once at startup), `_Worker.activate()` (successful session) and
`_teardown_session()` (link loss / shutdown). There is **no** publish on advert sightings,
so `hub_scanning` can lag reality by up to 1.5 s.

`state.json` (`$XDG_CONFIG_HOME/nso-gc/state.json`, `BridgeState` + `ControllerState`):

| Field | Source / rule |
| --- | --- |
| `updated_at` | `time.time()` set inside `write_state()` (wall clock, not monotonic) |
| `hub_alive` | `not stop.is_set() and not hub._hub_error` |
| `hub_scanning` | `hub._scanning` (set/cleared inside the scan loop) |
| `hub_error` | `hub._hub_error` (full string; only the *headline* truncates to 120 chars) |
| `service` | `stopping` (stop set) / `error` (hub_error) / `running` (otherwise) |
| `headline` | precedence: `Stopping` → `Needs attention` (hub_error) → `Set up` (no entries) → `"{n} connected"` → `Scanning` (if `hub_scanning`) → `Ready` |
| `detail` | `""` / `hub_error[:120]` / `"Add a controller once with Sync."` / `"P<player> <name>, … — ready in Steam and emulators"` / `"Press a button or hold Sync on a saved controller."` |
| `controllers[]` | one per **config entry**, in persisted order, matched to workers by uppercased MAC |

Per-controller fields: `mac` and `player` from the **config entry** (never
`effective_player()`), `name` = `entry.name or ctrl.name or ""`, `bonded` = entry flag,
`connected` = `worker.is_connected()`, `battery_mv` = live `ctrl.battery_mv` (or `None`),
`battery_pct` = `None` if `battery_mv` is falsy (0 or None) else
`clamp(int((mv - 2950) * 100 / 1300), 0, 100)` with `BATTERY_EMPTY_MV=2950`,
`BATTERY_FULL_MV=4200`.

Write mechanics: `Bridge._state_lock` serialises concurrent publishers; the write is
`state.json.tmp` + `os.replace` (atomic rename, no `fsync`). At shutdown `clear_state()`
**unlinks** `state.json`.

`led-players.json`:

* Path `CONFIG_DIR / "led-players.json"` = `$XDG_CONFIG_HOME/nso-gc/led-players.json`
  (default `~/.config/nso-gc/led-players.json`), produced by
  `system/bazzite-set-player-leds.py:write_bridge_led_map`.
* Shape: flat object `{"AA:BB:CC:DD:EE:FF": 1, "…": 2}` — MAC → 1-based player. Written
  with `indent=2` and a trailing newline; keys uppercased by the writer.
* Read on **every** use (`_read_led_players()`), no cache, no mtime check — two reads per
  1.5 s cycle when a session is connected (`effective_player()` at connect time, then
  `_apply_led_overrides`), one otherwise.
* Failure handling: `OSError` / `json.JSONDecodeError` ⇒ `{}` (silent); non-dict root ⇒
  `{}`; per entry, `str(mac).upper()` + `int(player)`, and `TypeError`/`ValueError` skips
  **that entry only**. There is no validation that player ≤ 8 or that two MACs do not map
  to the same player.
* `_led_override_for(mac)` = `min(max(player, 1), 8)` or `None` if absent.
  `effective_player()` = `_led_override_for(entry.mac)` if not `None` else `entry.player`.
  Note the asymmetry: `_apply_led_overrides()` uses the **raw** `_read_led_players()` value
  (no clamp) and relies on `set_player_leds()`'s internal `min(max(p,1),8)`.
* `_apply_led_overrides()` loop: for each worker whose controller is connected, look up its
  MAC; **skip if `player is None` or `worker._led_player == player`**; else
  `ctrl.set_player_leds(player)` (BLE command 0x09/0x07, synchronous ATT Write Request —
  runs on the state thread) and on success set `worker._led_player = player`.
* `worker._led_player` is the last **successfully pushed** value: initialised to
  `effective_player()` inside `activate()`, so a pad connecting while the file already
  agrees never gets a redundant write; on exception it is left unchanged ⇒ automatic retry
  on the next 1.5 s tick (no backoff, so a failing pad is retried 0.67 Hz forever).
* `_led_player` is reset only by process restart (it is per-`_Worker`, and workers are
  created once per configured entry in `Bridge.run()`).
* The override affects **only** LEDs: `slot` (`player-1` for DSU), `state.json.player`,
  the gamepad name `"{name} (P{entry.player})"` and the emulator ordering scripts all keep
  using `entry.player` (see §4.3).

### 9.3 `_on_input` — per-frame order and blocking

Two functions are involved; both run **synchronously on the ATT reader thread**.

`SwitchController._on_notification` (`device.py:205`, runs first, for every notification):

```python
if handle == self.h_input:
    report = P.InputReport.parse(data)      # 1. decode: ts, buttons, sticks, batt, IMU, triggers
    self.battery_mv = report.battery_mv     # 2. battery capture (every frame)
    now = time.monotonic()
    self.last_input_at = now                # 3. activity timestamp (every frame)
    if report.buttons != self._last_buttons:
        self._last_buttons = report.buttons
        self.last_button_at = now           # 4. only on button-MASK CHANGE
    if self.input_callback is not None:
        self.input_callback(self, report)   # 5. -> _Worker._on_input
```

Then `_Worker._on_input` (`bridge.py:513`):

```python
(lx,ly),(rx,ry),lt,rt = ctrl.calibrated_input(report)   # A. calibration + triggers
if self.gamepad is not None:
    self.gamepad.update(report.buttons, (lx,ly), (rx,ry), lt, rt)   # B. uinput
if self.motion is not None:
    self.motion.update(report)                                      # C. IMU uinput
if self.dsu is not None:
    sticks = (_stick_to_dsu(lx), _stick_to_dsu(ly), _stick_to_dsu(rx), _stick_to_dsu(ry))
    self.dsu.update(self.slot, report, sticks, (lt, rt))             # D. DSU
```

A. `calibrated_input` (all PIDs, same code path):

1. left stick: `StickCalibration.apply()` — `raw - center`, deadzone **0**, positive divided
   by `max`, negative by `min`, clamped to ±1.0;
2. right stick: identical with `right_calib`;
3. if `has_analog_triggers` (NSO GC only): `normalize_trigger(raw, neutral)` for both
   (subtract neutral, scale to 255, then `apply_trigger_deadzone`: `≤35` → 0,
   `≥209` → 255, linear in between), then force `lt = 255` if the digital `L` bit is set
   and `rt = 255` if `R` is set;
4. else (Pro / Joy-Con): `lt = 255 if ZL bit else 0`, `rt = 255 if ZR bit else 0`.

B. `gamepad.update()`: builds a per-code key state, OR-ing all switch names that share an
evdev code (GC L/ZL → `BTN_TL`), emits only changed `EV_KEY`/`EV_ABS`, D-pad as
`ABS_HAT0X/Y`, sticks scaled ±32768 with **Y negated**, then a single `SYN_REPORT` **iff
anything changed**. (uinput syscalls: one per changed axis/key + one syn.)

C. `motion.update()`: 6 `EV_ABS` writes (per-axis deduplicated) then an **unconditional**
`SYN_REPORT`.

D. `dsu.update()`: takes the slot mutex, stores buttons/sticks/triggers/IMU/timestamp,
recomputes the DSU battery enum, releases, then `_broadcast(slot)` builds one PadData
packet (zlib CRC32) and `sendto()`s it to every registered client; if there are no clients
it builds nothing.

**Joy-Con L/R differences in this path: none.** `_on_input` has no PID branch; Joy-Cons get
both stick calibrations read and both trigger bytes normalised (they simply report 0/garbage
on the missing side), `has_analog_triggers=False` so they take the digital ZL/ZR branch, and
`has_hd_rumble=True` (used only by `_write_motor`, §8.2). The full dual-stick uinput layout
is used for a lone Joy-Con — the "no per-side mapping" limitation from §4.5.

`last_button_at` semantics matter for task 04/08: the idle timer counts **time since the
button bitmask last changed**. Holding a button, or moving sticks / using gyro, does *not*
refresh it → a pad used only for aiming still falls asleep after `idle_sleep_s`.

**Blocking relationship:** B, C and D are strictly sequential on the same thread — they
cannot run concurrently, and none of them takes a lock the others take, so there is no
mutual blocking/deadlock between uinput emission and DSU update. The only cross-thread
coupling in the session is:

| Path | Thread | Contends with input path? |
| --- | --- | --- |
| `_write_motor` / `play_vibration_preset` / `set_player_leds` | FF thread, state thread | Only on the ATT socket: `sock.send()` for Write Command (single `send` per PDU, SEQPACKET ⇒ PDU-atomic) or `_resp_lock` for Write Request; the input path itself never sends |
| `_apply_led_overrides` | state thread (1.5 s) | Same as above; a 2 s command timeout can stall the state loop, not input |
| `_broadcast` | input thread (+ `_recv_loop` thread answering PadData polls) | The slot mutex is held only for the state copy, not during `sendto`; `_recv_loop` can answer a poll **while** the input thread broadcasts (two threads calling `sendto` on the same socket — safe for UDP) |

So the practical hazard is not lock contention but **head-of-line blocking inside the
callback**: every frame's uinput writes + IMU writes + N UDP sends + CRC happen before the
next `recv()`. A slow/blocked client socket or a busy disk-less `/dev/uinput` write delays
input and, because the same reader thread also carries the **command-response**
notifications, can also delay the handshake/rumble acknowledgements.

### 9.4 Teardown on link loss, and where the uinput node finally closes

`_Worker.run()` inner loop exits on `_disconnected` (set by `att` reader disconnect callback
or by `_sleep_for_idle()`), then calls `_teardown_session(full=False)`.

`_teardown_session(full=False)` — order and what is kept:

| Step | Effect | Kept deliberately |
| --- | --- | --- |
| 1 | `gamepad.rumble_cb = None` | — |
| 2 | `gamepad.release_all()` — emits key-up + axis-neutral for everything last set, one `SYN_REPORT` | **uinput node stays open**; `self.gamepad` and `_gamepad_product` retained |
| 3 | `motion` untouched (neither reset nor closed) | IMU node stays open, last IMU values linger |
| 4 | `dsu.set_slot(self.slot, False)` — marks disconnected, zeroes accel/gyro, clears MAC; **buttons/sticks/`motion_ts` are left stale** | slot index reserved (no reassignment) |
| 5 | `controller.close()` (socket shutdown+close; `_closing=True` so no second disconnect callback), `self.controller = None` | — |
| 6 | `on_topology_change()` → debounced (2 s) `run_emulator_reorder()` | — |
| 7 | `bridge._publish_state()` | — |

The node survives so Steam/SDL see a stable device across reconnects (no
re-enumeration, no player-order shuffle); reconnection reuses the same `SwitchGamepad`
unless the product id changes (`_ensure_gamepad` closes and recreates it only when
`_gamepad_product != ctrl.product_id`).

`_teardown_session(full=True)` — only from `_Worker.cleanup()`, called by `Bridge.run()`
during shutdown (after the stop event is set, per worker, before `dsu.stop()`):

* `gamepad.rumble_cb = None` then `gamepad.close()` → **`release_all()` is skipped** on this
  path; `close()` also calls `rumble_cb(0,0)` *only if set*, and it was just cleared, so no
  final stop packet is sent. The device node is destroyed, which is why the missing key-up
  events are harmless.
* `motion.close()` (uinput fd closed).
* Same `dsu.set_slot(..., False)`, `controller.close()`, topology hook, `_publish_state()` as
  above.

Then `Bridge.run()` finishes with `dsu.stop()`,
`hub._executor.shutdown(wait=False, cancel_futures=True)` and `clear_state()` (unlinks
`state.json`).

Other close points for completeness:

* `_teardown_partial(ctrl)` — used when `activate()` throws mid-handshake: closes the ATT
  link **and fully closes** `gamepad`/`motion` (note: it closes whatever gamepad is present,
  even one that was healthy from a previous session), leaving the pad unconnected until the
  hub retries.
* `_ensure_gamepad` — closes and recreates the pad/IMU pair when the PID changed.
* `_hd_worker` — stopped by `close()`/`_on_disconnect` via `_hd_run=False` + `_hd_dirty.set()`;
  no join call anywhere (daemon thread, may emit one last packet).

### 8.4 `play_vibration_preset` — exact bytes, and whether it is awaited

`device.py:311`: `write_command(COMMAND_VIBRATION=0x0A, SUBCOMMAND_VIBRATION_PLAY_PRESET=0x02,
preset_id.to_bytes().ljust(4, b"\x00"))`.

`preset_id.to_bytes()` with no arguments is `int.to_bytes(length=1, byteorder='big')`, then
right-padded with zeros to **4 bytes**. There are **no** leading pad/echo bytes inside the
data; the only length field is the protocol header's `len` byte.

| Preset | Data (after the 8-byte header) | Full PDU |
| --- | --- | --- |
| `0x03` soft | `03 00 00 00` | `0a 91 01 02 00 04 00 00 03 00 00 00` |
| `0x02` strong | `02 00 00 00` | `0a 91 01 02 00 04 00 00 02 00 00 00` |
| `0x01` medium (unused by the GC path) | `01 00 00 00` | `0a 91 01 02 00 04 00 00 01 00 00 00` |

Compare: player LED `09 91 01 07 00 04 00 00 01 00 00 00`; HCI sleep
`01 91 01 06 00 04 00 00 00 00 00 00`.

**Awaited, at the protocol level.** The write itself is an ATT Write Command (0x52,
unacknowledged at the ATT layer), but `SwitchController.write_command` then blocks on
`_cmd_event` for up to **2.0 s** while `_cmd_lock` is held, waiting for the notification on
the command-response characteristic (`h_cmd_resp`, GC default `0x001A`), and validates
`resp[0] == 0x0A && resp[1] == 0x01`, returning `resp[8:]`. So each preset costs one
round-trip and **serialises with every other command** (memory reads, LED writes, bond,
sleep) on that mutex.

Consequences worth speccing:

* `initialize()` always fires one *soft* preset at every connect
  (`_retry("vibration test", …GC_VIBRATION_PRESET_SOFT…)`) for **all** PIDs, independent of
  `config.enable_rumble`.
* The GC rumble path calls it **without** try/except; a timeout/ATT error propagates out of
  `set_rumble()` into `bridge._Worker._on_rumble`'s `try/except` (debug log) — i.e. that
  retrigger is silently dropped, and `_last_rumble_ts` has already been updated.
* A non-ACKing pad can therefore stall the FF thread for 2 s per retrigger (and the state
  thread for 2 s per LED write) because they share `_cmd_lock`.

### 8.5 Preset lifetime and GC retrigger semantics

* A preset is **not** sustained by the host and there is **no stop command**: the only
  vibration subcommand in the protocol module is `PLAY_PRESET 0x02`. The controller plays a
  finite built-in clip; "holding" rumble is emulated by re-issuing the same preset.
* **On release the GC path sends nothing.** `set_rumble()` GC branch:
  update `_last_rumble_mag`, and `if magnitude <= GC_LIGHT_THRESHOLD (0.06): return` — no
  stop packet, no zero command, no ATT traffic. The last-started clip simply finishes on
  the pad.
* **The 0.22 s retrigger is wall-clock gated but event-driven**, not input-frame driven:
  the interval is only evaluated when `set_rumble()` is entered, which happens only on an
  `EV_FF` play event from the uinput FF layer (`_on_rumble`). `set_rumble` has exactly one
  caller (`bridge.py:533`); there is no timer thread and no per-frame hook. So while a
  client holds rumble without re-issuing `EV_FF`, the pad goes quiet after the clip ends.
  `_last_rumble_ts` starts at `0.0`, and the rising-edge branch bypasses the interval check.
* A magnitude **change while held** does not take effect immediately: the non-rising branch
  only fires once `now - _last_rumble_ts >= 0.22`, so a harder hit mid-hold can be delayed up
  to 220 ms before the strong preset is selected.
* Thresholds: `< 0.06` ignored; `>= 0.52` → strong (`0x02`); otherwise soft (`0x03`).
  Preset `0x01` (medium) is deliberately never selected.

### 8.6 HD rumble worker: stopping, idling, and disconnect

`_hd_loop` (`device.py:359`):

```python
active = False
while self._hd_run:
    strong, weak = self._hd_target
    if strong <= 0.001 and weak <= 0.001:
        if active:
            try: self._write_motor(P.VibrationData())   # stop, ONCE
            except Exception: pass
            active = False
        self._hd_dirty.wait(timeout=1.0); self._hd_dirty.clear(); continue
    try: self._write_motor(self._hd_waveform(strong, weak))
    except Exception: pass
    active = True
    time.sleep(0.016)
```

* **Motor clear when FF goes to (0,0) with the pad still connected**: exactly **one** stop
  packet (`VibrationData()` ⇒ sample `e1 00 10 1e 00`, zero amplitudes, frequencies still
  `0x0E1/0x1E1`, next `header` byte, block duplicated for Pro — §8.2). It is sent on the
  first loop pass that observes the zeroed target, at most ~16 ms (+ one write) after the
  `EV_FF` stop. Failures are swallowed and **not retried** — if that single write is lost,
  nothing more is sent and the actuator is left to the firmware.
* **No repeated zero packets.** The zero-target branch is entered *before* the waveform
  write, so the loop never sends a second stop packet; `active` acts purely as a
  "did I send anything since the last stop" latch.
* **The sustain loop never exits on silence.** The thread lives until `_hd_run` is false; the
  zero branch parks on `_hd_dirty.wait(timeout=1.0)` and clears the flag. Restart latency
  after a silent period is normally immediate (the `EV_FF` sets the event) with a worst case
  of ~1 s if a new target lands between the wake and the `clear()` (the clear can eat the
  set, so the loop re-reads the target only after the next 1 s timeout). Sustain cadence is
  `time.sleep(0.016)` — nominal ~62 Hz, drift-accumulating, not an absolute schedule.
* **On disconnect there is no stop packet.** `_on_disconnect` (ATT reader thread) and
  `close()` both just set `_hd_run = False` + `_hd_dirty.set()`; the worker exits at the next
  loop head (≤16 ms, or immediately from the idle wait), possibly mid-`_write_motor` on a
  closing socket, which is swallowed. Nothing is joined. The motor state is left entirely to
  the firmware / the pad's own sleep path, exactly like a plain link drop.
* `SwitchGamepad.close()` does push `rumble_cb(0.0, 0.0)`, but every teardown path
  (`_teardown_session`, `_teardown_partial`, `_ensure_gamepad`) sets `gamepad.rumble_cb =
  None` **before** calling `close()`, so that push never reaches the controller in practice.
* `config.enable_rumble == false` ⇒ `rumble_cb` is never installed ⇒ `set_rumble` is never
  called, the worker (started unconditionally for HD PIDs in `initialize`) sits idle, and no
  stop packet is ever needed.
