# switch2-daemon

**High-performance Linux userspace bridge and virtual gamepad daemon for Nintendo Switch 2 controllers.**

Written in pure Rust, `switch2-daemon` connects Nintendo Switch 2 controllers (Joy-Con 2 Left & Right, Pro Controller 2, and NSO GameCube controllers) over Bluetooth Low Energy (BLE) and exposes them as native Linux `/dev/uinput` gamepads.

---

## Key Improvements Over Previous Prototypes

1. **Native Combined Joy-Con Mode:**
   - Automatically fuses Joy-Con 2 (Left) and Joy-Con 2 (Right) into **a single unified virtual controller** in userspace.
   - Works in all native Linux games, standalone emulators, and non-Steam applications without needing Steam Input to merge them.
   - Also supports Split Mode for 2-player gaming.

2. **Zero Python Dependencies:**
   - Single standalone ~5MB static binary.
   - No Python runtime, virtual environments (`.venv`), or runtime manager collisions (`mise`, `asdf`, `pyenv`).

3. **High-Performance & Low Latency:**
   - Written in Rust using asynchronous I/O and zero-allocation packet decoding.
   - Near-zero CPU utilization and sub-millisecond input report latency.

4. **Zero-Sudo Architecture:**
   - Direct L2CAP communication with no required elevated sudo helpers or brittle `btmgmt` hacks.

---

## Supported Controllers

| Controller | Status | Controls | Gyro (IMU) | Vibration |
|---|---|---|---|---|
| **Joy-Con 2 (Combined Pair)** | ✅ Supported | Full dual-stick + ABXY + D-Pad + 4 Triggers | ✅ 6-Axis | ✅ HD Rumble |
| **Joy-Con 2 (Split / Solo)** | ✅ Supported | Single stick + horizontal layout | ✅ 6-Axis | ✅ HD Rumble |
| **Pro Controller 2** | ✅ Supported | Dual analog sticks + digital triggers | ✅ 6-Axis | ✅ HD Rumble |
| **NSO GameCube** | ✅ Supported | Analog L/R triggers + C-Stick | ✅ 6-Axis | ✅ Preset Rumble |

---

## Architecture

```
                  ┌────────────────────────────────────────────────────────┐
                  │                    switch2-daemon                      │
┌──────────────┐  │                                                        │  ┌──────────────────────┐
│ Joy-Con 2 (L)│─►│ BLE L2CAP ──► ATT Client ──► Protocol Parser ──┐       │─►│ /dev/uinput          │
└──────────────┘  │ (Low Security)                                 │       │  │ (Combined Gamepad)   │
                  │                                                ├─► Fused│  │                      │
┌──────────────┐  │                                                │   State│  │ • Sticks: L + R     │
│ Joy-Con 2 (R)│─►│ BLE L2CAP ──► ATT Client ──► Protocol Parser ──┘       │  │ • Buttons: D-Pad+ABXY│
└──────────────┘  │                                                        │  │ • Triggers: L/ZL+R/ZR│
                  └────────────────────────────────────────────────────────┘  └──────────────────────┘
```

---

## Credits & Prior Art

This project is an independent ground-up Rust reimplementation building upon community reverse-engineering efforts:
- **[trevlars/switch2-controllers-linux](https://github.com/trevlars/switch2-controllers-linux):** Pioneering Linux userspace bridge and system integration work.
- **Nadeflore/switch2-controllers:** GATT service discovery and packet structure definitions.
- **ndeadly:** Input report specifications and GameCube trigger analysis.
- **bitaxislabs:** Switch 2 BLE protocol documentation and packet offsets.

---

## License

GPL-3.0-or-later
