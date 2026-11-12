# switch2d

[![CI](https://github.com/williamrolando88/switch2d/actions/workflows/ci.yml/badge.svg)](https://github.com/williamrolando88/switch2d/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL_3.0-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![Rust](https://img.shields.io/badge/rust-1.89%2B-orange.svg)](https://www.rust-lang.org)

Use Nintendo Switch 2 controllers on Linux as normal gamepads.

`switch2d` talks to the controller directly over Bluetooth Low Energy and creates a virtual
gamepad, so games, Steam and emulators see a standard Linux controller. It does not need root
while running, and it does not use the BlueZ GATT stack, which is what makes the connection
stable enough for gaming.

- Joy-Con 2 (Left/Right), alone or **combined into one gamepad**
- Sticks, triggers, D-pad, all buttons including the Switch 2 `C` button
- Rumble, motion (DSU/cemuhook) and a separate gyro device
- Battery level and player LEDs
- Works with combined mode, split mode, Steam and Dolphin

---

## Requirements

- Linux with BlueZ (`bluetoothctl`, `btmgmt`, `busctl`) and a Bluetooth adapter that supports LE
- Rust toolchain (`cargo`) to install — [rustup.rs](https://rustup.rs) if you do not have it
- `evtest` is optional but very useful for checking input (`sudo dnf install evtest` /
  `sudo apt install evtest`)

Installation needs `sudo` for system files (a udev rule, a small privacy unit and a sudoers
rule) — expect to type your password, possibly more than once. The daemon itself always runs as
your normal user.

---

## Install

### 0. Coming from the older Python bridge?

If this machine ever ran the previous `nso-gc` bridge, its units can still be enabled and will
fight this one for the Bluetooth adapter. Disable them first:

```bash
systemctl --user disable --now nso-gc-watchdog.timer
systemctl --user disable --now nso-gc.service nso-gc-watchdog.service nso-gc-after-gamescope.service
```

`install.sh` and `switch2d doctor` both warn about these, so you can also just run the installer
and read its warnings. If you never installed the Python bridge, ignore this step.

### 1. Install the program

```bash
git clone https://github.com/williamrolando88/switch2d.git
cd switch2d
./install.sh
```

Add `--service` if you want it to start automatically at login:

```bash
./install.sh --service
```

The installer is safe to re-run and skips anything already in place. It installs:

| What | Where |
|---|---|
| the `switch2d` binary | `~/.cargo/bin/switch2d` |
| udev rule (gamepad/motion devices and player LEDs) | `/etc/udev/rules.d/70-switch2d.rules` |
| LE-privacy unit (required for reconnects) | `/etc/systemd/system/switch2d-disable-le-privacy.service` |
| least-privilege sudo rule for `btmgmt stop-find` | `/etc/sudoers.d/switch2d-btmgmt` |

### 2. Verify the installation

```bash
switch2d doctor
```

A healthy machine prints one `PASS` line per check and exits 0:

```text
PASS  uinput: /dev/uinput is writable
PASS  le-privacy unit: switch2d-disable-le-privacy.service: is-enabled=enabled, is-active=active
PASS  bluetooth adapter: <hostname> (8C:17:..:D6:1E) [MAC partially redacted]
PASS  discovery: StartDiscovery accepted (adapter is discoverable); Device1 objects: 0, with ManufacturerData: 0
PASS  sudo btmgmt: sudo -n -l /usr/bin/btmgmt is allowed (no password prompt)
PASS  legacy bridge units: no legacy nso-gc units enabled or active
PASS  running instances: no other switch2d process
PASS  DSU port: 127.0.0.1:26760 is free
PASS  config: ~/.config/switch2d/config.json parsed; 0 bonded controller(s)
PASS  udev uaccess rule: /etc/udev/rules.d/70-switch2d.rules
```

Anything not `PASS` comes with a `fix:` line — run that, then `switch2d doctor` again. **Do not
continue while a check says `FAIL`**: a missing `le-privacy unit` in particular makes reconnects
impossible, and every later symptom would be a symptom of that.

If `switch2d: command not found`, your shell has not seen `~/.cargo/bin` yet. Open a new
terminal, or add `~/.cargo/bin` to `PATH`.

---

## Pair your controllers

Pairing is needed **once per controller**. After that it reconnects on its own.

1. Turn the controller on.
2. Hold the small round **SYNC** button (between `SL` and `SR` on a Joy-Con) until its LEDs
   cycle back and forth — while they cycle, the controller is discoverable.
3. Run:

```bash
switch2d pair
```

The scan window is 10 seconds. Expected result:

```text
Found Switch 2 controller: AA:BB:CC:DD:EE:01
Connecting to AA:BB:CC:DD:EE:01 (Player 1)...
✅ Controller Successfully Paired & Bonded!
Player:  1
Holding connection for 6 seconds so you can verify the Player 1 LED...
```

The player LED is solid during that hold and goes dark when the command exits. **That is
normal**: a Joy-Con only lights its LEDs while a host is connected.

Repeat for the second controller. Useful variants:

```bash
switch2d pair --player 2                 # force the player number
switch2d pair AA:BB:CC:DD:EE:01          # skip the scan when you know the address
```

Re-pairing an already bonded controller keeps its player number, so it is safe to run again if
something feels wrong.

Check the result:

```bash
switch2d status
```

---

## Run it

```bash
switch2d run
```

It prints what it is doing, connects to the bonded controllers when they wake, and creates the
gamepad. Press `Ctrl+C` to stop: virtual devices are removed and the controllers are released.

If you installed with `--service`, the same daemon runs in the background:

```bash
systemctl --user start switch2d
systemctl --user stop switch2d
systemctl --user status switch2d
journalctl --user -u switch2d -f     # live log
```

**Run only one copy at a time.** `switch2d run` refuses to start if the service is already
running, and tells you the pid that owns the adapter.

Wake a controller with any button. It connects in a few seconds and its player LED lights up.

---

## Check that it works

The safe way is `monitor`: it creates no virtual device and cannot disturb your desktop.

`monitor` connects to **one** controller at a time: by default the first bonded controller in
your config. Pass a MAC to check a specific controller.

```bash
switch2d monitor --seconds 30                     # first bonded controller
switch2d monitor AA:BB:CC:DD:EE:02 --seconds 30   # a specific one
```

One line per report, for example:

```text
[#1234] btn=A,PLUS raw=(1024,2048)(2048,900) cal=(0.00,-0.02)(0.71,0.00) lt=0 rt=255 bat=3820mV 78% acc=(0.01,0.99,0.02)g gyro=(1.2,-0.4,0.0)dps
[stats] reports=89/s dropped=0 max_gap=12
```

Sticks should read near `0.00` at rest and reach about `±1.00` at full deflection, buttons
should appear with their names, and battery should be steady.

To see it as a real gamepad while the daemon runs:

```bash
evtest            # pick "Nintendo Switch 2 Combined Joy-Cons"
```

---

## Playing games

Games see an ordinary Linux gamepad. Two notes save time:

**Steam** — the controller appears as `Nintendo Switch 2 Combined Joy-Cons`. If the game ignores
it, check Steam's controller settings, and remember that a `switch2d selftest --drive` run while
Steam is open will be interpreted as real input.

**Dolphin** — motion comes from `switch2d` over DSU:

1. `switch2d run`
2. Dolphin → Controller → GameCube Pad → Configure
3. Set **Motion Source** to `DSU Client` (default address `127.0.0.1:26760`)
4. The motion calibration screen should react when you move the controller

**Rumble** — driven by standard Linux force feedback. To check it independently of any game:

```bash
evtest /dev/input/eventN     # the combined node; use the force-feedback section
```

If the pad buzzes there, rumble works and a silent game is the game not sending effects.

Motion, rumble and the gyro device are implemented but only lightly validated on hardware: if
something feels inverted or silent, that is worth reporting rather than a misconfiguration.

### Face buttons

Switch controllers print letters in the Nintendo diamond, while most PC games assume the Xbox
diamond. By default `switch2d` reports physical positions, so pressing the right-hand `A` sends
the game its right-hand button. If a game or Steam prompt expects Xbox letters — `A` at the
bottom — set `"face_button_layout": "labels"` in `~/.config/switch2d/config.json` and restart
the daemon (`systemctl --user restart switch2d`).

---

## Two controllers as one, or as two

```bash
switch2d mode combined     # default: one gamepad, both halves, one player number
switch2d mode split        # two gamepads, one per Joy-Con (2D/horizontal mapping)
```

Combined mode lights the same player number on both halves, like a Joy-Con grip. Split mode is
implemented but has not been validated on hardware as thoroughly as combined mode.

---

## Configuration

`~/.config/switch2d/config.json` holds the paired controllers, the mode and a few timings.
`switch2d` writes it; you rarely need to edit it. Handy commands:

```bash
switch2d status
switch2d mode combined
switch2d remove AA:BB:CC:DD:EE:01     # forget one controller
switch2d add AA:BB:CC:DD:EE:01 --name "Joy-Con 2 (Left)" --player 1
```

The daemon also reads `~/.config/switch2d/led-players.json` if it exists (written by some
player-LED tools) and lets it override the player number used for the LEDs and DSU slot.

---

## Troubleshooting

Start with `switch2d doctor` — it checks every environment piece and prints the fix.

| Symptom | Cause and fix |
|---|---|
| Connect fails with `os error 38` / `Function not implemented`, pad keeps blinking | LE privacy is blocking reconnects. `switch2d doctor` should show the privacy unit as `PASS`; if not, re-run `./install.sh` |
| Pad pairs, then LEDs go dark | Expected. LEDs only light while a host is connected |
| `switch2d run` says another instance is running | The user service is up: `systemctl --user stop switch2d`, or stop `run` with `Ctrl+C` |
| Nothing happens when pressing buttons | Is the daemon running? Is the controller bonded (`switch2d status`)? Was it paired on this machine? |
| LEDs never settle during `pair` | Hold SYNC until they cycle, keep the controller close to the adapter, and check `sleep 1; switch2d doctor` for the privacy unit |
| A game does not see the controller | Close Steam first and check `evtest`; the node is named `Nintendo Switch 2 Combined Joy-Cons` |
| Rumble does nothing | Check `evtest` first. If it buzzes there, the game is not sending force feedback |
| `doctor` warns about the udev rule | `./install.sh` again (it installs the rule), then unplug/replug the adapter |
| `doctor` warns about `nso-gc` units | Leftover from the older Python bridge; run the disable command it prints |
| `doctor` warns about the DSU port | Another program holds `127.0.0.1:26760`; motion is unavailable, controllers still work |
| Input feels delayed or drops | `journalctl --user -u switch2d -f` while playing; report the `dropped=` and `max_gap=` numbers |

Logs are the most useful thing in a bug report:

```bash
journalctl --user -u switch2d --since "10 min ago"
switch2d -v run          # verbose, in the foreground
```

---

## Known limitations

- **Pro Controller 2** and the **NSO GameCube Controller** are supported in code but have never
  been tested on hardware. Treat them as unverified.
- **Split mode** is implemented but less validated than combined mode.
- Motion axis signs are not verified against every game; if motion is inverted for you, that is
  a known open question rather than a setup mistake.
- The daemon dials a controller only after it wakes; while a controller sleeps, silence in the
  log is correct behaviour.

---

## Uninstall

```bash
./install.sh --uninstall     # removes udev rule, privacy unit, sudoers rule, user unit
cargo uninstall switch2d     # removes the binary
rm -rf ~/.config/switch2d    # optional: forget pairings and settings
```

That sequence is the complete removal: no switch2d files, units, rules or pairings are left.
Run all three before reporting an install problem.

---

## Contributing

Contributions and bug reports are welcome: direct pushes to `main` are protected, so changes go
through pull requests and CI. [HANDOVER.md](HANDOVER.md) explains the Bluetooth behaviour that
shaped this design, and [docs/SUPPORT.md](docs/SUPPORT.md) records which controllers and
features are verified against real hardware.

---

## Credits

An independent Rust implementation built on community reverse-engineering:

- **[trevlars/switch2-controllers-linux](https://github.com/trevlars/switch2-controllers-linux)** — pioneering Linux userspace bridge
- **Nadeflore/switch2-controllers** — GATT discovery and packet structures
- **ndeadly** — input report and GameCube trigger work
- **bitaxislabs** — Switch 2 BLE protocol notes

## License

[GNU General Public License v3.0](LICENSE)
