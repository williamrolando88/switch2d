# switch2d — Rust Architecture Plan

**Author:** Lead Architect (t13) · **Input:** `docs/analysis_controllers.md` (t12)
**Baseline:** HEAD `4ec0515`, 2540 LOC, `cargo check` clean.

---

## 1. Findings from reviewing `src/` against the analysis

Four defects confirmed by reading code (not speculation):

| # | Location | Defect | Impact |
|---|---|---|---|
| D1 | `bluetooth/att.rs::discover_all` | CCCD handle is **guessed** as `value_handle + 1` | Wrong CCCD on any layout where a non-CCCD descriptor sits after the characteristic → subscribe silently fails. Legacy discovers CCCDs properly via `ATT_FIND_INFO` (`att.py:330-355`). |
| D2 | `controller.rs::connect` | `bond()` is called on **every** connect, unconditionally | Legacy bonds only when `!entry.bonded` (`bridge.py:578-585`). 4 extra command round-trips per wake + needless flash writes. |
| D3 | `controller.rs::connect` | Calibration is **never read**; `left_calib`/`right_calib`/`trigger_neutral` stay `Default` forever | Sticks use a fake 2048/1700 calibration; GC triggers never get their real neutral. Daemon passes `StickCalibration::default()` at the call site too (`daemon.rs`). |
| D4 | `uinput/gamepad.rs` | `phys` argument is **logged only**, never applied. `evdev 0.12.2` has no `physical_path()` on `VirtualDeviceBuilder` (verified in the vendored crate) | Breaks the `phys="ngc/<MAC>"` contract used by the IMU companion node, `70-ngc-imu-uaccess.rules`, and SDL pairing. Needs raw `UI_SET_PHYS` ioctl. |

Also absent relative to legacy: rumble/FF entirely (`EV_FF` never enabled), `status.json`, DSU server, advertisement gating, idle sleep (`idle_sleep_seconds` is dead config).

## 2. Target module map

```
src/
  lib.rs
  main.rs            CLI (status/run/mode/pair/add/remove) — shrink over time
  config.rs          persisted config + adapter detect + migration + led-players.json
  protocol/
    constants.rs     ids, UUIDs, LED table, feature bits           [exists]
    command.rs       framing, LTK, ControllerInfo                  [exists]
    report.rs        input report decode                           [exists]
    calibration.rs   stick/trigger math                            [exists]
    advert.rs        advertisement parse + accept policy   <-- NEW (task 01)
  bluetooth/
    l2cap.rs         raw socket, BT_SECURITY_LOW                   [exists]
    att.rs           ATT client: correlation by opcode, CCCD discovery  [fix D1]
    mgmt.rs          HCI scan on/off (btmgmt or mgmt socket)       <-- NEW (later)
  controller.rs      handshake, calibration, rumble, idle sleep    [fix D2, D3]
  scanner.rs         AdvertSource trait + hub scan/connect scheduler <-- NEW
  worker.rs          per-pad session state machine (was bridge.py:_Worker) <-- NEW
  daemon.rs          supervision + topology (was bridge.py:Bridge)  [rewrite]
  status.rs          atomic state.json publish                   <-- NEW
  dsu.rs             cemuhook UDP server                         <-- NEW
  uinput/
    gamepad.rs       virtual pad, phys ioctl, FF upload/erase     [fix D4]
    motion.rs        INPUT_PROP_ACCELEROMETER node                <-- NEW
    combined.rs      Joy-Con combine/split mapping                [exists]
```

### Concurrency model (tokio)

- One **supervisor task** owns config, `status.json`, and topology hooks.
- One **hub task** owns BLE scanning and is the *only* place a connect is initiated;
  it holds a `tokio::sync::Mutex<()>` connect permit so only one LE create-connection
  is outstanding (mirrors `_CONNECT_LOCK`).
- One **worker task per bonded MAC**: waits for the hub to hand it a connected
  `SwitchController`, then owns uinput + rumble + idle-sleep. Never dials on its own.
- **Reader thread** (blocking `recv`) → `tokio::sync::mpsc` bounded(64) → consumer.
  Input decode, uinput, DSU never run on the reader thread (analysis §5.6).
- Command responses correlate on `(opcode, handle)` through a `oneshot` registered in a
  pending-request map, not a single shared slot (analysis §5.5).

### Trait seam for testability (USB later, fake scanner now)

```rust
pub trait Transport: Send { fn write_cmd(&self, v: &[u8]) -> io::Result<()>; }
pub trait AdvertSource: Send { fn next_adverts(&mut self, window: Duration) -> io::Result<Vec<AdvertInfo>>; }
```
Hub and worker logic are written against these so the scheduler is unit-testable
without hardware, and USB becomes a second `Transport` impl later.

## 3. Task sequence (delegated in order, each independently verifiable)

| # | Task | Depends on | Hardware | Status |
|---|---|---|---|---|
| 01 | `protocol/advert.rs` + tests | — | no | **done** `514b25e` |
| 02 | `status.rs` atomic state plane (+02C service/XDG/battery) | — | no | **done** `514b25e` |
| 03 | `att.rs` CCCD discovery + response correlation | — | no | **done** `482b818` |
| 04A | `scan.rs` `SeenAdverts` + `ScanControl` seam | 01 | no | **done** `482b818` |
| 05 | `controller.rs` calibration, bond gating, MTU, retries | 03 | yes to validate | **done** `2805098` |
| 06A | `bluez.rs` `AdvertSource` over `busctl` GetManagedObjects | 01, 04A | no | next |
| 06B | `hub.rs` scan/connect scheduler with fake source + dialer | 04A, 06A | no | |
| 07 | `uinput` raw node with `UI_SET_PHYS` (ADR-1) | — | yes | |
| 08 | rumble: HD 60 Hz worker + GC presets + `EV_FF` | 05, 07 | yes | |
| 09 | daemon rewrite: hub + worker split, idle sleep, topology, `status` wiring | 06B, 08 | yes | |
| 10 | `dsu.rs` 4 slots + slot policy (ADR-3/4) | 09 | Dolphin | |
| 11 | `uinput/motion.rs` IMU node | 07 | yes | |
| 12 | combined-pair ownership + horizontal mapping validation | 05, 07 | yes | |

Commit policy: one commit per approved task, each verified to build on its own.

## 4. Decisions required before tasks 06-09

These change the public contract, so they are the human Project Lead's call:

1. **`phys` / uinput creation** — `evdev 0.12.2` cannot set `phys`. Options:
   (a) keep `evdev` for event types but create the node with raw `/dev/uinput`
   ioctls (`UI_DEV_SETUP`, `UI_SET_PHYS`, `UI_SET_EVBIT`, ...) — no new deps, ~200 LOC;
   (b) bump `evdev` to a version that exposes `physical_path` — unverified, may not exist;
   (c) drop the `phys` contract and update the udev rules + shim.
   **Recommendation: (a).**
2. **Privileged scan stop** — keep the `sudo -n btmgmt` / `bluetoothctl` shell-outs
   (proven on hardware) or implement HCI via `mgmt` sockets? Analysis §6 open Q1.
   **Recommendation: keep shell-outs behind `bluetooth/mgmt.rs` so they can be replaced
   later without touching the hub.**
3. **Player slots** — cap at 4 (clean DSU mapping) or keep 8 with documented DSU
   collision? Analysis §5.10. **Recommendation: accept 8, map to DSU `min(player-1,3)`,
   and log a warning on overflow rather than silently colliding.**
4. **`led-players.json`** — keep as the cross-tool contract (recommended, it is what the
   Bazzite scripts write) and extend its authority to DSU slot + gamepad name so the
   three agree (fixes analysis §4.3 inconsistency).
5. **Joy-Con pairing** — explicit config (`pair_id` / `partner_mac`) over autodetect.
   Analysis §4.5 says autodetect is unpredictable and the detector classifies by
   uinput name. **Recommendation: explicit only, in v1.**

---

## 5. Protocol details confirmed by t12 (report §8), binding on tasks 03/05/07

### 5.1 CCCD discovery (task 03) — do not guess `value_handle + 1`
Legacy `att.py::discover_cccds` issues `ATT_FIND_INFO_REQ (0x04)` over
`[value_handle+1, next_char.decl_handle-1]`, or `[value_handle+1, service.end_handle]`
for the last characteristic in a service. Every `0x2902` in that range is assigned, so the
**highest handle wins**. No fallback, no probe, no derivation: if no CCCD is found the
handle stays `None`, and only then does `device.py::_resolve_handles` substitute the
hard-coded NSO-GC defaults (`0x000B` input, `0x001B` command-response). ATT errors,
non-FIND_INFO replies and inverted ranges silently leave `None`. `_by_uuid` is a flat
UUID→characteristic map (last service wins) built only when full discovery succeeded.

### 5.2 Vibration payload (task 07)
Written with **ATT Write Command (0x52)**, unacknowledged, no CCCD.

```
sample = 5 bytes, u40 LE:
  bits 0-8    lf_freq   (0x1FF)          default 0x0E1
  bit  9      lf_en_tone
  bits 10-19  lf_amp    (0x3FF)
  bits 20-28  hf_freq   (0x1FF)          default 0x1E1
  bit  29     hf_en_tone
  bits 30-39  hf_amp    (0x3FF)
block  = 1 header byte (0x50 + (seq & 0x0F), wraps 0x50..0x5F) + sample*3 = 16 B
Pro Controller 2 : 0x00 | blockL | blockR          = 33 B (blocks byte-identical, same header)
Joy-Con L/R, GC  : 0x00 | block                    = 17 B
```
Zero-amp sample = `e1 00 10 1e 00`; full LF amplitude = `e1 fc 1f 1e 00`.
**Stop packet keeps the frequencies**: same layout with amplitudes 0, sent once when the
target drops to ≤0.001; sustain resends every 16 ms (~60 Hz).
Consequence for task 05: a 33-byte Pro payload needs `ATT_MTU >= 36`. At MTU 23 the write
is impossible and legacy silently does nothing — our MTU check must reject < 36 for
HD-rumble-capable PIDs, not just < 63.

### 5.3 Force feedback (task 06/07)
Only `UI_FF_UPLOAD` with `effect.type == FF_RUMBLE (0x50)` is stored, keyed by effect id:
`(strong_magnitude, weak_magnitude)` as `u16` 0..65535. `retval = 0`, then `UI_END_FF_UPLOAD`
(with `request_id = event.value` — python-evdev 2.0.0 had a bug leaving it 0). Erase pops the
id. `FF_PERIODIC`/`FF_CONSTANT` are advertised but never stored, so playback yields (0,0).
`EV_FF(value == 0)` → `(0.0, 0.0)` without clearing stored magnitudes; `value != 0` →
`(strong/65535.0, weak/65535.0)`; `ff_replay.length` ignored (no auto-stop).
**Legacy bug not to port**: `FF_GAIN (0x60)` / `FF_AUTOCENTER (0x61)` fall through the same
path and alias effect ids 96/97, i.e. they stop rumble. Handle `FF_GAIN` as a global gain
multiplier, or ignore it explicitly.

---

## 6. Decisions (approved by Project Lead) — ADRs

### ADR-1 — uinput node creation via raw ioctls *(task 06a)*
**Decision:** keep `evdev` for event/axis/key *constants and `InputEvent` encoding*, but
create the node directly on `/dev/uinput` with ioctls. Rejected: bumping `evdev`
(unverified availability of a `phys` setter) and dropping the `phys` contract (would break
the IMU uaccess udev rule, `ngc_evdev_uniq_shim.c` and SDL pairing).
**Required ioctls** (`_IOW('U'=0x55, nr, size) = (1<<30)|(size<<16)|(0x55<<8)|nr`):
`UI_DEV_SETUP`(3, size of `uinput_setup`), `UI_DEV_DESTROY`(2), `UI_ABS_SETUP`(4, size of
`uinput_abs_setup`), `UI_SET_EVBIT`(100,int), `UI_SET_KEYBIT`(101,int),
`UI_SET_ABSBIT`(103,int), `UI_SET_MSCBIT`(104,int), `UI_SET_FFBIT`(107,int),
`UI_SET_PHYS`(108,`char*`), `UI_SET_PROPBIT`(110,int).
Order: open `/dev/uinput` → `UI_SET_*BIT` for every capability → `UI_ABS_SETUP` per axis →
`UI_DEV_SETUP` (`input_id` + name + `ff_effects_max`) → `UI_SET_PHYS("ngc/<MAC>")` →
`UI_DEV_CREATE`. Events are then written to the same fd as
`struct input_event { timeval; __u16 type; __u16 code; __s32 value; }` (24 B on 64-bit,
16 B where `time_t` is 32-bit — size it from `libc::timeval`, never hard-code 24).
**Acceptance test (hardware-free):** create a node, then `evdev::Device::open(path)` and
assert `physical_path() == Some("ngc/<MAC>")` and that the `input_id` matches.
Legacy `gamepad.py:126-136` is the reference for vendor/product/bustype values.

### ADR-2 — privileged scan stop behind a seam *(task 08)*
**Decision:** keep the proven shell-outs, but never call `Command` from hub logic.
```rust
pub trait ScanControl: Send + Sync {
    fn prepare_global(&self) -> io::Result<()>;        // pkill decky-bluetooth-wake-control,
                                                       // bluetoothctl scan off, Adapter1.StopDiscovery
    fn stop_le_scan(&self, force: bool) -> io::Result<()>; // btmgmt -i N stop-find -l, then stop-find
    fn start_le_scan(&self) -> io::Result<()>;
    fn remove_bluez_device(&self, mac: &str) -> io::Result<()>; // never while we own a session
}
```
Hard rules carried from `bridge.py`: rate-limit `stop_le_scan` to one call per 350 ms unless
`force`; every child gets a 1.5 s deadline, then `kill()` — a hung `btmgmt` must never block
the hub; index defaults to 0, overridable by `NGC_HCI`. A `MgmtSocketScanControl` can
replace it later without touching the hub.

### ADR-3 — player slots 1..8, DSU collapses *(tasks 03/09)*
**Decision:** accept 1..8. `PlayerSlot(u8)` newtype with `PlayerSlot::new` validating 1..=8,
and `dsu_slot() = (player - 1).min(3)`. Overflow onto DSU slot 3 emits **one** warning per
(mac, player) pair, never a silent collision. `LED_PATTERNS` already covers 1..8.

### ADR-4 — `led-players.json` stays, and becomes authoritative for all three views
**Decision:** keep reading `~/.config/nso-gc/led-players.json` (exact legacy path, shape
`{"AA:BB:..": 1}`) and make the override feed **LED pattern, DSU slot, and the `(P<n>)`
gamepad name** — fixing analysis §4.3 where three views disagreed. Unknown/malformed file or
an out-of-range value falls back to the configured `player`, logged at warn once.

### ADR-5 — explicit Joy-Con pairing only *(task 11)*
**Decision:** no autodetect of complementary PIDs. Config gains per-entry
`partner_mac: Option<String>` and `owns_slot: bool` (default `true`). In `Combined` mode the
owner drives LED pattern, DSU slot and the gamepad name; the partner's input is merged into
the owner's pad and it publishes no slot. In `Split` mode both own their slots. Because the
Bazzite detectors classify by uinput name string, the combined pad must publish a name they
recognise — the agreed name is `Nintendo Switch 2 Combined Joy-Cons` (accepted as `generic`
in v1; a detector-side change is out of scope here).

### Config schema after these ADRs
```json
{
  "adapter_mac": "AA:BB:CC:DD:EE:FF",
  "mode": "combined",
  "idle_sleep_seconds": 300,
  "enable_rumble": true,
  "gc_impact_threshold": 0.52,
  "controllers": [{
    "mac": "..", "name": "Joy-Con 2 (Left)", "product_id": 8295, "serial": "..",
    "player": 1, "bonded": true, "partner_mac": null, "owns_slot": true
  }]
}
```
All new fields `#[serde(default)]` so existing configs keep loading; `partner_mac` and
`owns_slot` default to `None`/`true`.

---

## 7. Hub/worker reference behavior confirmed by t12 (report §9) — binding on tasks 04/08

### 7.1 Scan loop (legacy `bridge.py::_scan_loop`)
1. Snapshot workers. **If no worker is disconnected**: set `scanning=false`, sleep 1.5 s,
   `continue` — the daemon does not scan while every pad is connected.
2. `scanning=true`; start scanner (3 tries; an `InProgress` error triggers
   `prepare_global` + forced scan-off + 0.5/1.0/1.5 s backoff).
3. Sleep **0.50 s** — this is the only advert-collection window.
4. `scanner.stop()`, `scanning=false`, settle **0.10 s**.
5. `now = monotonic()`; `pending` = disconnected workers whose `last_seen` is within TTL,
   sorted by seen timestamp **descending**.
6. If pending: `prepare_global()`, then `remove_bluez_device()` for each still-disconnected
   pending MAC, sleep **0.08 s**.
7. Under one connect-lock acquisition, dial each pending MAC **serially**; on success drop
   `last_seen`, on failure force a scan-off. A failed connect does **not** clear `last_seen`.
8. TTL sweep with the pre-connect `now`; idle counter; sleep **0.05 s** if any pad is
   connected, else **0.025 s**.

`last_seen` is keyed by **uppercase MAC** and stores `(monotonic_ts, mode)` where `mode` is
`pairing` for `reconnect == 0` and `wake` otherwise (absent block ⇒ `wake`); it is recorded
only after `_accept` passes. TTLs: wake 4 s, pairing 45 s, applied at selection *and* sweep.
With two pads due, the second waits up to ~16 × 0.80 s × 2 address types ≈ 25.6 s — the
pending list is frozen before dialing, and the next cycle re-checks TTL against a fresh
`now`. `_HUB_IDLE_RESTART_CYCLES` (80) resets on firing and in the else-branch, but is
**frozen** in the all-connected early-out. Port note: our hub must keep the early-out and
the frozen counter, or we will re-scan while pads are live.

### 7.2 status.json (in addition to §ADR-3/4)
`service` ∈ {stopping, error, running}; `updated_at` is set inside the write; `headline`
truncates to 120 chars while `hub_error` keeps the full string; `player` is the **configured**
player (hence the additive `effective_player` in task 02C). Battery percent =
`clamp((mv - 2950) * 100 / 1300, 0, 100)`, `None` when `mv == 0`. Published event-driven
from start/activate/teardown **and** every 1.5 s; serialized by a write lock; unlinked at
shutdown. `led-players.json` is re-read on **every** use (no cache, no mtime check, two reads
per 1.5 s tick while connected), `$XDG_CONFIG_HOME/nso-gc/`, flat `{"MAC": n}`, lenient
parsing that skips bad entries and validates nothing else. An LED push that throws leaves the
dedup cache unchanged, so it retries forever with no backoff.

### 7.3 Per-input-frame order (all on the reader thread in legacy)
`parse` → `battery_mv` (every frame) → `last_input_at` (every frame) → `last_button_at`
**only when the button mask changes** → `calibrated_input` (both sticks always; analog
trigger path for NSO GC only, non-GC uses digital ZL/ZR) → uinput emit (diffed, single SYN)
→ motion emit → DSU build+send. Legacy has **no Joy-Con L/R branch** in this path: a lone
Joy-Con still gets the full dual-stick layout, which is exactly what ADR-5 replaces.
Design consequence: uinput, motion and DSU run back-to-back on one thread, so fresh input
queues behind up to N UDP sends plus a CRC on the same thread that also carries command
responses. Our reader→bounded-channel design (2) removes that head-of-line hazard; no lock
conflict exists between uinput and DSU in legacy, so there is nothing to preserve there.
Note `last_button_at` ignores stick and gyro motion — holding a button does **not** postpone
idle sleep.

### 7.4 Teardown
On link loss (`full=False`): clear the rumble callback, `release_all()` (keys up + axes
centred + SYN) but **keep** the uinput node and the gamepad/IMU pair for the next session,
zero the DSU slot, close the controller, fire the topology hook, publish state. `full=True`
only at shutdown and only there are nodes closed; it **skips** `release_all()` and sends no
stop-rumble packet because the callback was cleared first. Port note: our shutdown path must
sequence stop-rumble **before** clearing the callback, and must still release keys — two
legacy ordering bugs we should not reproduce. `_teardown_partial()` on a failed activate also
closes a gamepad that was healthy from a previous session; ours should keep it.

---

## 8. Deliberate deviations from legacy (running log)

Every entry is a place where we knowingly do not reproduce `.old/`. Keep this list current —
it is the audit trail for "why does Rust behave differently".

| # | Area | Legacy | switch2d | Reason |
|---|---|---|---|---|
| 1 | CCCD discovery (`att.rs`) | single `FIND_INFO` per range; silently `None` on truncation | pages `FIND_INFO` from highest-entry-handle+1, max 32 pages, keeps earlier pages | a descriptor table larger than the MTU would otherwise lose CCCDs |
| 2 | CCCD fallback | `value_handle+1` guess when properties look notify-capable | no guess; GC defaults only on genuine absence, plus `HandleProvenance` + warn | the guess silently subscribes to the wrong attribute |
| 3 | `prepare_global` (`scan.rs`) | aborts the sequence when `bluetoothctl scan off` fails | best-effort: log and still force the HCI scan stop | returning early leaves discovery running, which breaks create-connection |
| 4 | Calibration failure (`controller.rs`) | 3-try retry then raise → worker teardown | warn and continue on `StickCalibration::default()` | default calibration still yields usable input; a dead session is worse |
| 5 | MTU (`controller.rs`) | request 247, no check, mis-parse truncated reports | hard error below 66 (63-byte report + 3-byte ATT header) | silent mis-parse is undebuggable |
| 6 | Shutdown ordering (§7.4) | clear rumble callback, then close → stop packet never sent | stop rumble first, release keys, then close | motors can stay spinning |
| 7 | Vibration preset on connect (`0x0A/0x02`) | sent as a vibration self-test | deferred until the payload is confirmed by the Analyzer | not guessing wire formats |
| 8 | `FF_GAIN` / `FF_AUTOCENTER` | alias into the effect map (ids 96/97) and stop rumble | handled explicitly or ignored, never aliased | legacy bug |
| 9 | Idle-sleep timer | button-mask changes only; stick and gyro do not count | same, kept for behavioural parity | deliberate parity, revisit if users complain |

### ADR-6 — advertisement source: BlueZ `GetManagedObjects` over `busctl` *(task 04B)*
**Decision (Project Lead approved):** new `src/bluetooth/bluez.rs` implements `AdvertSource`
by invoking

```
busctl call org.bluez / org.freedesktop.DBus.ObjectManager GetManagedObjects --json=short
```

**Verified on the target machine before adoption** (unprivileged, `systemd 262`): one call
returns every object, including `/org/bluez/hciN` with `org.bluez.Adapter1.Address` and — once
discovery has run — `org.bluez.Device1` objects carrying `Address`, `RSSI`, `Connected` and
`ManufacturerData` as an `a{qv}` dict. `serde_json` (already a dependency) parses it directly;
no new crate, no root, no D-Bus binding.

Consequences:
* Replaces adapter auto-detection (`bluetoothctl list` scraping) — the adapter MAC comes from
  the same payload.
* `bluetoothctl scan on` (via `ScanControl`) is what makes BlueZ populate device objects; the
  hub therefore starts discovery before polling and stops it via ADR-2 before dialing.
* Rejected alternatives: raw `mgmt` sockets (needs CAP_NET_ADMIN for discovery, which would
  break the zero-sudo claim) and the `bluer` crate (works, but adds a D-Bus stack to a project
  that deliberately bypasses BlueZ GATT).

**Known risk — staleness.** `GetManagedObjects` exposes a *cached* property set, so a pad that
stops advertising may keep its `ManufacturerData` in BlueZ for a while, whereas legacy saw
only live adverts. Mitigation: require `RSSI` to be present and greater than -127 (BlueZ only
sets it from live adverts) before an entry becomes eligible for `SeenAdverts::record`, and keep
per-MAC connect-attempt rate limiting in the hub. This is a bounded degradation (a retry every
wake TTL) rather than a correctness break, because the hub only ever dials known bonded MACs
that BlueZ reports as `Connected: false`.
