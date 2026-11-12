# switch2d

[![CI](https://github.com/williamrolando88/switch2d/actions/workflows/ci.yml/badge.svg)](https://github.com/williamrolando88/switch2d/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL_3.0-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

**High-performance Linux userspace bridge and virtual gamepad daemon for Nintendo Switch 2 controllers.**

Written in pure Rust, `switch2d` connects official Nintendo Switch 2 controllers over Bluetooth
Low Energy and exposes them as native Linux `/dev/uinput` gamepads, with userspace Joy-Con
combining, rumble, motion (DSU/cemuhook) and a gyro evdev node.

---

## ✨ Features

- **🎮 Combined Joy-Con mode** — Joy-Con 2 (Left) and (Right) presented as **one** virtual
  gamepad, including a single player number on both halves.
- **👥 Split mode** — each Joy-Con as a standalone horizontal controller. Coded; not yet
  validated on hardware.
- **🕹️ Full input mapping** — sticks, triggers, D-pad, all face/shoulder/system buttons,
  including the Switch 2 `C` button.
- **📳 Rumble** — HD rumble for Joy-Con 2 and Pro Controller 2 (60 Hz stream), preset clips for
  the GameCube pad, driven from standard Linux force feedback (`EV_FF`).
- **🎯 Motion** — DSU/cemuhook output for Dolphin/Cemu/Ryujinx, plus a separate gyro evdev node
  per controller.
- **🔋 Battery and player LEDs** — read from the pad, published in `switch2d status`, LEDs kept
  in sync with the effective player number.
- **⚡ Low latency** — direct `AF_BLUETOOTH` L2CAP sockets with `BT_SECURITY_LOW`, bypassing the
  BlueZ GATT stack.
- **🔒 No sudo at runtime** — runs entirely as your desktop user; the only privileged pieces are
  installed once (see [Install](#-install)).
- **🪶 Small** — single static binary, no Python or other runtime dependencies.

---

## 🎮 Supported Controllers

See [docs/SUPPORT.md](docs/SUPPORT.md) for the authoritative, per-feature matrix.

| Controller | Status |
|---|---|
| **Joy-Con 2 (Left)** | ✅ validated on hardware |
| **Joy-Con 2 (Right)** | ✅ validated on hardware |
| **Pro Controller 2** | 🧪 speculative — same protocol family, never tested on hardware |
| **NSO GameCube Controller** | 🧪 speculative — analog triggers and preset rumble never exercised |

Only Joy-Con 2 has been verified against real hardware. Do not read "supported" into the
speculative rows; treat them as code that needs a tester.

---

## 📋 Requirements

- Linux with BlueZ and a Bluetooth adapter supporting LE.
- `btmgmt` and `busctl` (part of BlueZ).
- Write access to `/dev/uinput` — provided by the udev rule installed below.
- Optional but recommended: `evtest` and `jstest` for diagnosing input.

---

## ⚙️ Install

`switch2d` runs as your desktop user. Everything below is a one-time setup; after it, nothing
needs root.

```fish
git clone https://github.com/williamrolando88/switch2d.git
cd switch2d

# 1. Build and install into ~/.cargo/bin (must be on PATH).
cargo install --path . --locked

# 2. udev rules: uinput/evdev access and player-LED permissions without manual ACLs.
sudo install -Dm644 system/70-switch2d.rules /etc/udev/rules.d/70-switch2d.rules
sudo udevadm control --reload-rules
sudo udevadm trigger

# 3. LE privacy unit. Required for reconnects after a controller wakes; see below.
sudo install -Dm755 system/disable-le-privacy.sh /usr/local/bin/disable-le-privacy.sh
sudo install -Dm644 system/switch2d-disable-le-privacy.service /etc/systemd/system/switch2d-disable-le-privacy.service
sudo systemctl daemon-reload
sudo systemctl enable --now switch2d-disable-le-privacy.service

# 4. Least-privilege sudoers rule, so stopping an HCI scan never prompts for a password.
echo "$USER ALL=(root) NOPASSWD: /usr/bin/btmgmt -i 0 stop-find -l, /usr/bin/btmgmt -i 0 stop-find" \
    | sudo tee /etc/sudoers.d/switch2d-btmgmt
sudo chmod 0440 /etc/sudoers.d/switch2d-btmgmt

# 5. Optional: run the daemon at login as a user service.
install -Dm644 system/switch2d.service ~/.config/systemd/user/switch2d.service
systemctl --user daemon-reload
systemctl --user enable switch2d.service
```

If an older `switch2d.service` exists from a previous install, disable it first with
`systemctl --user disable --now switch2d.service` and replace the file.

---

## 🚀 Usage

```fish
switch2d doctor        # preflight: says exactly what is wrong and how to fix it
switch2d pair          # pair, bond and set the player LED (run once per controller)
switch2d status        # bonded controllers, mode, adapter
switch2d run           # run the daemon in the foreground
```

### Pairing

Turn a controller on and hold its **SYNC** button (the small round one between `SL`/`SR` on
Joy-Con 2) until the LEDs cycle, then:

```fish
switch2d pair                       # scan and pair
switch2d pair --player 2            # force a player number
switch2d pair AA:BB:CC:DD:EE:01     # skip the scan when you know the MAC
```

The player LED settles solid while the link is held and goes dark when `pair` exits — that is
normal: a Joy-Con only shows LEDs while a host is connected. `switch2d run` lights it again.

Re-pairing an already bonded controller keeps its player number.

### Modes

```fish
switch2d mode combined   # one unified gamepad (default)
switch2d mode split      # one gamepad per Joy-Con
```

### Diagnostics

```fish
switch2d monitor --seconds 30     # live decoded input, one line per report; creates no device
switch2d advert --seconds 15      # what the pads are advertising, including the host they want
switch2d selftest                 # create a virtual pad and report what the kernel sees
switch2d selftest --seconds 8 --drive   # ...and sweep sticks/buttons (close Steam first!)
```

`monitor` is the safe way to check a controller: it creates no virtual device, so it cannot
disturb the desktop or steal focus.

---

## 🔒 LE privacy (required for reconnects)

A Switch 2 controller stores the **public** Bluetooth address of the host that paired with it.
When the kernel runs with LE privacy enabled it initiates connections from a rotating resolvable
private address, which the pad does not recognise, so it silently ignores the connection: the
pad advertises, the host times out, forever. With privacy off the pad reconnects normally.

The installed unit turns that policy flag off after every `bluetoothd` start:

```fish
sudo systemctl status switch2d-disable-le-privacy   # should be active (exited)
sudo btmgmt -i 0 info | grep "current settings"     # "privacy" must not be listed
```

**Trade-off, stated honestly:** with LE privacy off the kernel uses the adapter's public address
toward *every* LE device, so LE peers can recognise this machine across sessions instead of
seeing a rotating address. `ll-privacy` may stay enabled; only the `privacy` policy flag matters
here. Revert with `sudo systemctl disable --now switch2d-disable-le-privacy.service` and
`sudo systemctl restart bluetooth`.

---

## 🩺 Troubleshooting

Run `switch2d doctor` first — it checks uinput access, the privacy unit, the adapter, a stale
user unit, the DSU port, the config file and the udev rule, and prints the fix for anything that
is wrong.

| Symptom | Likely cause |
|---|---|
| `L2CAP connect failed (... os error 38 ...)` while the pad's LEDs cycle | LE privacy is on: see [LE privacy](#-le-privacy-required-for-reconnects) |
| `EBUSY` / connect times out only when Steam is running | Steam's own LE scan; the daemon already stops discovery before dialling, but a background scan by another app can still compete |
| Controller pairs, then LEDs go dark | normal — LEDs only light while a host is connected |
| `switch2d doctor` warns about the udev rule | install `system/70-switch2d.rules` and run `sudo udevadm control --reload-rules && sudo udevadm trigger` |
| Game sees the controller but rumble does nothing | check `evtest` first; if rumble works there, the game is not sending `EV_FF` |

Logs from the user service:

```fish
journalctl --user -u switch2d -f
```

---

## ⚠️ Known limitations

- **Split mode** is coded but has not been validated on real hardware.
- **Pro Controller 2** and the **NSO GameCube Controller** are speculative: no hardware evidence.
- **Motion signs** (accelerometer/gyro axes) are unverified on hardware; if a game's motion feels
  inverted, that is a known open question rather than a configuration error.
- The daemon dials a pad only when it sees it advertising. Silence in the log while a controller
  sleeps is correct behaviour, not a hang.

---

## 🤝 Contributing

Contributions, bug reports and pull requests are welcome.

- Direct pushes to `main` are protected; changes come via pull requests and CI.
- Read [HANDOVER.md](HANDOVER.md) before touching connection logic — it records the
  reverse-engineering traps that produced the current design.
- `docs/SUPPORT.md` states what is validated versus speculative. A PR that promotes a controller
  must include hardware evidence; a test enforces that the table and the code agree.

---

## 🙏 Credits & Prior Art

An independent ground-up Rust reimplementation building on community reverse-engineering:

- **[trevlars/switch2-controllers-linux](https://github.com/trevlars/switch2-controllers-linux):** Pioneering Linux userspace bridge and system integration work.
- **Nadeflore/switch2-controllers:** GATT service discovery and packet structure definitions.
- **ndeadly:** Input report specifications and GameCube trigger analysis.
- **bitaxislabs:** Switch 2 BLE protocol documentation and packet offsets.

---

## 📜 License

This project is licensed under the [GNU General Public License v3.0](LICENSE).
