# switch2d

[![CI](https://github.com/williamrolando88/switch2d/actions/workflows/ci.yml/badge.svg)](https://github.com/williamrolando88/switch2d/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL_3.0-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

**High-performance Linux userspace bridge and virtual gamepad daemon for Nintendo Switch 2 controllers.**

Written in pure Rust, `switch2d` connects official Nintendo Switch 2 controllers (Joy-Con 2 Left & Right, Pro Controller 2, and NSO GameCube controllers) over Bluetooth Low Energy (BLE) and exposes them as native Linux `/dev/uinput` gamepads with sub-millisecond input latency and native userspace Joy-Con combining.

---

## ✨ Features

- **🎮 Combined Joy-Con Mode (implemented; hardware validation pending):**
  Fuses Joy-Con 2 (Left) and Joy-Con 2 (Right) into **a single unified virtual gamepad** in userspace. Round 5 is the first end-to-end hardware test of this path.
- **⚡ High Performance & Low Latency:**
  Direct asynchronous Linux `AF_BLUETOOTH` L2CAP seqpacket sockets bypassing BlueZ GATT and eliminating Bluetooth connection dropouts (`BT_SECURITY_LOW`). Sub-millisecond latency.
- **🪶 Ultra Lightweight:**
  Single static release binary with **zero Python or runtime dependencies**, consuming only **~1.7 MB of RAM** (down from 20-50 MB in Python prototypes).
- **🔒 Zero-Sudo Architecture:**
  Operates completely in userspace through standard `/dev/uinput` ACLs without requiring elevated root privileges.
- **🧪 Coded but not shipped as working:** split mode, HD/preset rumble, DSU/cemuhook motion output and the separate IMU evdev node. These paths exist in the tree but are not wired/validated end to end; see [docs/SUPPORT.md](docs/SUPPORT.md) before relying on them.

---

## 🎮 Supported Hardware

See [docs/SUPPORT.md](docs/SUPPORT.md) for what is validated versus speculative.

| Controller | Status | What the evidence covers |
|---|---|---|
| **Joy-Con 2 (Left)** | ✅ validated on hardware (rounds 1-4) | Pairing, player LED, 11 buttons, left stick, battery and IMU decode. |
| **Joy-Con 2 (Right)** | ✅ validated on hardware (rounds 1-4), with a known stick-calibration defect | Everything above, but the right stick currently caps at about ±0.74 because calibration falls back to defaults. |
| **Pro Controller 2** | 🧪 speculative - never tested on hardware | Code follows the same protocol family; buttons, sticks, rumble, IMU and LEDs are unverified. |
| **NSO GameCube** | 🧪 speculative - never tested on hardware | Analog-trigger and preset-rumble paths exist in code but have never run. |

"Full support" claims are intentionally gone. Rumble, DSU motion output, the IMU evdev node and split mode are coded but not wired/validated; [docs/SUPPORT.md](docs/SUPPORT.md) is the authoritative list.

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
switch2d add AA:BB:CC:DD:EE:01 --name "Joy-Con 2 (Left)" --player 1
switch2d add AA:BB:CC:DD:EE:02 --name "Joy-Con 2 (Right)" --player 2
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
Adapter MAC:  0A:0B:0C:0D:0E:0F
Joy-Con Mode: Combined
Bonded Controllers (2):
  [1] Joy-Con 2 (Left) (AA:BB:CC:DD:EE:01) - Player 1
  [2] Joy-Con 2 (Right) (AA:BB:CC:DD:EE:02) - Player 2
Configuration file: ~/.config/switch2d/config.json
```

---

## 🔄 Modes

You can toggle modes on the fly:

> Combined mode is implemented and is the current round-5 validation target. Split mode is
> coded but has not been validated on hardware; see [docs/SUPPORT.md](docs/SUPPORT.md).

```bash
# 1-Player Unified Gamepad (Default)
switch2d mode combined

# 2-Player Separate Gamepads (Multiplayer)
switch2d mode split
```

---

## ⚙️ Install

`switch2d` runs as your desktop user; the only privileged pieces are the LE-privacy unit
and the udev rules.

```fish
# 1. Build and install into ~/.cargo/bin (this directory must be on PATH).
cargo install --path . --locked

# 2. Install the LE-privacy unit (commands in LE Privacy below).
#    Required for Joy-Con reconnects.

# 3. udev rules: uinput/evdev uaccess and LED permissions without manual ACLs.
sudo install -Dm644 system/70-switch2d.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger

# 4. Install the user unit and enable it at login.
install -Dm644 system/switch2d.service ~/.config/systemd/user/switch2d.service
systemctl --user daemon-reload
systemctl --user enable switch2d.service

# 5. Verify the host, then run in the foreground.
switch2d doctor
switch2d run
```

`switch2d` must be on `PATH`: the unit starts `%h/.cargo/bin/switch2d run`.
If an older `switch2d.service` exists, disable it first with
`systemctl --user disable --now switch2d.service`, then replace the file.
Step 2 is the [LE Privacy](#-le-privacy-required-for-controller-reconnects) unit,
required for controller reconnects. See [docs/SUPPORT.md](docs/SUPPORT.md) for
what is validated.

---

## 🔒 LE Privacy (required for controller reconnects)

A Switch 2 controller stores the **public** Bluetooth address used during pairing. If the kernel
runs with LE privacy enabled it initiates LE connections from a rotating resolvable private
address (RPA); the pad never bonded that address and silently ignores the connection. Measured
A/B on the reference machine: privacy on = 32/32 dial attempts failed (`ENOSYS` / HCI `0x3E` on
the public address type, timeout on random); privacy off = connected on the first round and
streamed 3975 input reports over 45 s with zero drops.

Install the unit that turns the policy flag off after every bluetoothd start:

```fish
# Copy the privacy fix into place
sudo install -Dm755 system/disable-le-privacy.sh /usr/local/bin/disable-le-privacy.sh
sudo install -Dm644 system/switch2d-disable-le-privacy.service /etc/systemd/system/switch2d-disable-le-privacy.service

# Enable it (idempotent: an adapter that is already correct is not power-cycled)
sudo systemctl daemon-reload
sudo systemctl enable --now switch2d-disable-le-privacy.service

# Verify: "privacy" must not appear under "current settings:"
sudo btmgmt -i 0 info
```

**Trade-off, stated honestly:** this is a host-wide setting. With LE privacy off the kernel
advertises and connects using the adapter's public address toward *every* LE device, not only
Joy-Cons, so LE peers can recognise this machine across sessions instead of seeing a rotating
address. `ll-privacy` may stay enabled — only the `privacy` policy flag changes the initiator
address the pad sees. Revert with
`sudo systemctl disable --now switch2d-disable-le-privacy.service` followed by
`sudo systemctl restart bluetooth`.

### Troubleshooting: reconnects time out / ENOSYS 0x3E

Symptom: the pad advertises (LEDs cycling) but `switch2d monitor` / `switch2d run` logs
`L2CAP connect failed (... Function not implemented (os error 38) ...)` round after round.

```fish
sudo btmgmt -i 0 info
```

If `current settings:` lists `privacy`, the unit above is missing, not enabled, or bluetoothd was
restarted after it ran (`PartOf=bluetooth.service` should pull it back in — check
`systemctl status switch2d-disable-le-privacy.service`).

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
| Split mode / combined mode | combined implemented (round 5 validation); split coded, not validated | combined = one node, split = one node per Joy-Con |
| HD rumble packets | coded, **not wired** to force-feedback yet | `EV_FF` in a game does nothing yet |
| GameCube preset rumble | coded, **not wired** yet | requires a GC pad to test |
| DSU / cemuhook motion out | coded, **not wired** into the daemon yet | Dolphin will see nothing yet |
| Separate IMU gyro node | coded (`src/uinput/motion.rs`), **not wired** into the daemon yet | motion is decoded and printed by `monitor`; the node module exists |

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

### Round 2

The current plan is [`docs/TEST_PLAN_ROUND7.md`](docs/TEST_PLAN_ROUND7.md): install the
program the way a user would, verify with `switch2d doctor`, then play a real game -
including the first hardware check of rumble, DSU motion in Dolphin, and idle sleep.
Earlier rounds: [`ROUND6B`](docs/TEST_PLAN_ROUND6B.md),
[`ROUND6`](docs/TEST_PLAN_ROUND6.md), [`ROUND5`](docs/TEST_PLAN_ROUND5.md),
[`ROUND5`](docs/TEST_PLAN_ROUND5.md), [`ROUND4`](docs/TEST_PLAN_ROUND4.md),
[`ROUND3`](docs/TEST_PLAN_ROUND3.md), [`ROUND2`](docs/TEST_PLAN_ROUND2.md).

Historical note: [`docs/TEST_PLAN_ROUND3.md`](docs/TEST_PLAN_ROUND3.md): round 2
proved pairing works but reconnect-after-wake does not, so round 3 is read-only
diagnostics on the pad's advertisement and the kernel connect path. The previous
round is [`docs/TEST_PLAN_ROUND2.md`](docs/TEST_PLAN_ROUND2.md). Start
there: it supersedes Test A's checklist for this build (`2aaa36a`). Step 1 purges
both Joy-Cons from BlueZ and switch2d, and pairing is re-verified from scratch.

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
