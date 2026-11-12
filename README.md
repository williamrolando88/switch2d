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
