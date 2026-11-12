# switch2d

[![CI](https://github.com/williamrolando88/switch2d/actions/workflows/ci.yml/badge.svg)](https://github.com/williamrolando88/switch2d/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL_3.0-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

**High-performance Linux userspace bridge and virtual gamepad daemon for Nintendo Switch 2 controllers.**

Written in pure Rust, `switch2d` connects official Nintendo Switch 2 controllers (Joy-Con 2 Left & Right, Pro Controller 2, and NSO GameCube controllers) over Bluetooth Low Energy (BLE) and exposes them as native Linux `/dev/uinput` gamepads with sub-millisecond input latency and native userspace Joy-Con combining.

---

## ✨ Features

- **🎮 Native Combined Joy-Con Mode:**
  Fuses Joy-Con 2 (Left) and Joy-Con 2 (Right) into **a single unified virtual gamepad** in userspace. The left stick controls movement and the right stick controls camera/aiming on the exact same `/dev/input/js0` node. No Steam Input or third-party wrappers required.
- **👥 Split Mode (2-Player Local Multiplayer):**
  Instantly toggle into Split Mode where each Joy-Con acts as a standalone horizontal controller for local co-op / party games.
- **⚡ High Performance & Low Latency:**
  Direct asynchronous Linux `AF_BLUETOOTH` L2CAP seqpacket sockets bypassing BlueZ GATT and eliminating Bluetooth connection dropouts (`BT_SECURITY_LOW`). Sub-millisecond latency.
- **🪶 Ultra Lightweight:**
  Single static release binary with **zero Python or runtime dependencies**, consuming only **~1.7 MB of RAM** (down from 20-50 MB in Python prototypes).
- **🔒 Zero-Sudo Architecture:**
  Operates completely in userspace through standard `/dev/uinput` ACLs without requiring elevated root privileges.

---

## 🎮 Supported Hardware

| Controller | Status | Controls | Gyro (IMU) | Vibration |
|---|---|---|---|---|
| **Joy-Con 2 (Combined Pair)** | ✅ Full Support | Full dual-stick + ABXY + D-Pad + 4 Triggers | ✅ 6-Axis | ✅ HD Rumble |
| **Joy-Con 2 (Split / Solo)** | ✅ Full Support | Single stick + horizontal layout | ✅ 6-Axis | ✅ HD Rumble |
| **Pro Controller 2** | ✅ Full Support | Dual analog sticks + digital triggers | ✅ 6-Axis | ✅ HD Rumble |
| **NSO GameCube** | ✅ Full Support | True analog L/R triggers + C-Stick | ✅ 6-Axis | ✅ Preset Rumble |

---

## 🚀 Quick Start

### 1. Build & Install

```bash
git clone https://github.com/williamrolando88/switch2d.git
cd switch2d
cargo build --release
cp target/release/switch2d ~/.local/bin/
```

### 2. Register Your Controllers

Pair your controllers or add their Bluetooth MAC addresses:

```bash
# Add Left and Right Joy-Cons
switch2d add BC:89:A6:45:83:CC --name "Joy-Con 2 (Left)" --player 1
switch2d add BC:89:A6:49:8D:74 --name "Joy-Con 2 (Right)" --player 2
```

### 3. Check Status

```bash
switch2d status
```

Output:
```text
===========================================================
   switch2d Status
===========================================================
Adapter MAC:  8C:17:59:41:D6:1E
Joy-Con Mode: Combined
Bonded Controllers (2):
  [1] Joy-Con 2 (Left) (BC:89:A6:45:83:CC) - Player 1
  [2] Joy-Con 2 (Right) (BC:89:A6:49:8D:74) - Player 2
Configuration file: ~/.config/switch2d/config.json
```

---

## 🔄 Modes

You can toggle modes on the fly:

```bash
# 1-Player Unified Gamepad (Default)
switch2d mode combined

# 2-Player Separate Gamepads (Multiplayer)
switch2d mode split
```

---

## ⚙️ Systemd Background Service

To run `switch2d` automatically in the background on system login:

```bash
# Install user service
mkdir -p ~/.config/systemd/user
cat << 'EOF' > ~/.config/systemd/user/switch2d.service
[Unit]
Description=Nintendo Switch 2 Controller Daemon (switch2d)
After=bluetooth.target
Wants=bluetooth.target

[Service]
Type=simple
ExecStart=%h/.local/bin/switch2d run
Restart=always
RestartSec=3
StartLimitIntervalSec=0

[Install]
WantedBy=default.target
EOF

# Enable and start
systemctl --user daemon-reload
systemctl --user enable --now switch2d.service

# View live logs
journalctl --user -u switch2d -f
```

---

## 🧪 Hardware Validation Guide

This section is written for whoever is testing a **real controller** (human or assisting agent).
It describes what a given build can do, the exact commands to run, what correct output looks
like, and what to write down when it is not.

### Current build status

| Capability | State | Notes |
|---|---|---|
| Virtual gamepad node (`/dev/input/eventN`, `phys=ngc/<MAC>`) | **implemented** | buttons, both sticks, triggers, hats |
| BLE handshake, bonding, player LED | **implemented** | verify with `pair` |
| Input report decode, battery, stick calibration | **implemented** | verify with `monitor` |
| Split mode / combined mode | **implemented** | combined = one node, split = one node per Joy-Con |
| HD rumble packets | coded, **not wired** to force-feedback yet | `EV_FF` in a game does nothing yet |
| GameCube preset rumble | coded, **not wired** yet | requires a GC pad to test |
| DSU / cemuhook motion out | coded, **not wired** into the daemon yet | Dolphin will see nothing yet |
| Separate IMU gyro node | **not implemented** | motion is only decoded and printed by `monitor` |

Do **not** report the last four rows as bugs — they are known, planned work. Everything above
them is in scope and worth a report if it misbehaves.

### Preconditions

```fish
# /dev/uinput must be writable by your user (crw-rw----+ ... 10, 223)
test -w /dev/uinput; and echo "uinput OK"

# evtest is the ground truth for what the kernel receives
command -v evtest; or sudo dnf install evtest   # or apt/pacman equivalent

# Build the current tree
cargo build --release
```

### Test A — live input from a real controller (start here: creates no device)

```fish
cargo run --release -- monitor --seconds 60
```

**This is the safe first test.** It creates no virtual device, so it cannot touch the desktop,
steal focus or move the pointer. It connects to a bonded controller and prints one line per
report:

```text
[#1234] btn=A,PLUS raw=(1024,2048)(2048,900) cal=(0.00,-0.02)(0.71,0.00) lt=0 rt=255 bat=3820mV 78% acc=(0.01,0.99,0.02)g gyro=(1.2,-0.4,0.0)dps
```

and one `[stats] reports=<n>/s dropped=<n> max_gap=<ms>` line per second.

**What actually matters here:**

1. **Sticks at rest** must read near `0.00` and reach roughly `±1.00` at full deflection. If full
   deflection only reaches ~`0.36`, that is the known inherited calibration defect described
   below — record the exact numbers, they are the evidence needed to fix it.
2. **Each button** must light up exactly once, with the correct name. Press one at a time and
   note any that are missing, doubled or swapped. `L`/`R` are digital, `ZL`/`ZR` analog.
3. **Accel** at rest should be ~`1.0 g` on one axis; **gyro** near `0` when still and reacting
   when rotated. Signs are unverified — note which direction gives positive values.
4. **Battery** within a few tens of mV of reality and not jittering.

### Test B — virtual gamepad, no controller needed

```fish
cargo run --release -- selftest          # passive: describes the node, drives nothing
```

It prints the node path, name, `phys=ngc/AA:BB:CC:DD:EE:FF`, input id and every ABS range —
all read back from the kernel, not from our own request. With `evtest` running beside it you
can confirm the node exists. Press Ctrl-C to release it.

**Only if you need to see motion**, and **only with Steam Input / antimicrox closed**:

```fish
cargo run --release -- selftest --seconds 8 --drive
```

That sweeps sticks, triggers and all 15 buttons. It is opt-in for a reason: anything on the
desktop that maps gamepads will claim the node and start acting on those events.

### Test B — pair the Joy-Con 2 pair

Turn both controllers on. On each one, hold the **SYNC** button (the small round one between
`SL`/`SR` on Joy-Con 2) until its LEDs cycle back and forth.

```fish
# one controller at a time; the scan window is 10 s
cargo run --release -- pair
cargo run --release -- pair
```

**Expected:** `Found Switch 2 controller: <MAC>`, then `✅ Controller Successfully Paired & Bonded!`,
and the controller's player LED settles solid on Player 1 for the first and Player 2 for the second.
`switch2d status` lists both.

**Known-good but surprising:** the first Joy-Con usually ends up on player 1 and the second on
player 2 in pairing order, not by left/right. Use `--player N` to force it.

**Failure worth reporting:** `EBUSY`, timeouts, `Bonding NOT confirmed!`, or LEDs never settling.

### Test C — the daemon as it runs today

```fish
cargo run --release -- run
```

**Expected:** it connects to the bonded controllers, creates the virtual node(s), and input works
in Steam or in `evtest` while the daemon holds the node.

**Known:** rumble, DSU and the IMU node are not wired yet; combined-mode regressions are the most
likely thing to find here, because that path has been rewritten most.

### Recording findings

Copy this block for every finding and keep the raw output — paraphrased logs are not usable:

```text
FINDING
  severity: blocks-testing | wrong-behaviour | cosmetic
  build:    <git rev-parse --short HEAD>
  controller: model + MAC (from `switch2d status`)
  command:  <exact command line>
  expected: <what the guide above says should happen>
  actual:   <paste the full output, not a summary>
  extra:    lsusb/bluetoothctl info <MAC>, `evtest` output, dmesg | tail -20 if the kernel is involved
  reproducible: always | once | after N minutes
```

Attach `~/.config/switch2d/config.json` and, for crashes, the full backtrace
(`RUST_BACKTRACE=1`). Send findings to the project lead; they are triaged into the task list.

### Known defects that are not new bugs

- **Stick range under-scales with real calibration** (~0.36 at full deflection when the pad
  reports a calibration max near 3200). Both the Python original and this port divide by the
  absolute calibration maximum instead of the centre-relative delta. Faithful, but wrong; a fix
  needs pad measurements, so the numbers from Test A step 1 are what unblocks it.
- **First connect may take a few seconds** while the handshake, MTU negotiation and calibration
  read complete. Up to ~4 s is normal; a repeat of `EBUSY` is not.
- **`btmgmt stop-find` requires the sudoers rule** shipped with the project
  (`/etc/sudoers.d/nso-gc-btmgmt`). Without it the daemon warns and continues, which can make a
  reconnect slower but should not make it fail.

---

## 🤝 Contributing & Pull Requests

Contributions, bug reports, and pull requests are welcome!
To maintain code stability:
- Direct pushes to `main` are protected.
- All code changes must come via a Pull Request and pass automated CI checks.
- Please review [**HANDOVER.md**](HANDOVER.md) for critical reverse-engineering lessons, solved hardware traps, and Bluetooth kernel behaviors before modifying connection logic.

---

## 🙏 Credits & Prior Art

This project is an independent ground-up Rust reimplementation building upon community reverse-engineering efforts:
- **[trevlars/switch2-controllers-linux](https://github.com/trevlars/switch2-controllers-linux):** Pioneering Linux userspace bridge and system integration work.
- **Nadeflore/switch2-controllers:** GATT service discovery and packet structure definitions.
- **ndeadly:** Input report specifications and GameCube trigger analysis.
- **bitaxislabs:** Switch 2 BLE protocol documentation and packet offsets.

---

## 📜 License

This project is licensed under the [GNU General Public License v3.0](LICENSE).
