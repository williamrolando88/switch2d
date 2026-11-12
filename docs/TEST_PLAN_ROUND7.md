# Round 7 — install it properly, then play a real game

Build: **`d31c244`**. This round does not use a frozen copy: it installs the program the
way a user would, and tests the installed binary.

Round 6/6B proved the bridge works when run by hand. This round answers a different
question: **is this a product?** Install → `switch2d doctor` → pair → daemon → a game that
rumbles.

## What is being verified, and what is genuinely new

| Item | Prior evidence | This round |
|---|---|---|
| `switch2d` as a command | none | `cargo install`, run from PATH |
| Udev/ACL setup | manual on the dev box | udev rule + doctor must pass |
| Pair / connect / reconnect | rounds 4, 5, 6 | again, via the installed service |
| Key mapping | `evtest`, rounds 5, 6 | **in a game** |
| Rumble | unit tests only | **first time on real hardware** |
| DSU motion | loopback test only | **Dolphin motion calibration screen** |
| IMU node | node creation only | visible to the game/SDL |
| Idle sleep (300 s) | never verified | left running during play |

---

## 0. Install

```fish
cd /var/home/wmorales/repos/switch2d
set LOG ~/switch2d-test/round7-(date +%H%M).log

cargo install --path . --locked 2>&1 | tee -a $LOG
which switch2d                                    # must resolve
switch2d --help | head -20
```

Install the system bits (the only sudo in the round, all one-time):

```fish
# LE privacy: without this, reconnect after wake fails with os error 38
sudo install -Dm755 system/disable-le-privacy.sh /usr/local/bin/disable-le-privacy.sh
sudo install -Dm644 system/switch2d-disable-le-privacy.service /etc/systemd/system/switch2d-disable-le-privacy.service
sudo systemctl daemon-reload
sudo systemctl enable --now switch2d-disable-le-privacy.service

# udev: uinput/evdev ACLs and player LEDs
sudo install -Dm644 system/70-switch2d.rules /etc/udev/rules.d/70-switch2d.rules
sudo udevadm control --reload-rules
sudo udevadm trigger

# the daemon itself runs as your user, never as root
sudo systemctl --user disable --now switch2d.service 2>/dev/null   # the old unit, if present
mkdir -p ~/.config/systemd/user
install -Dm644 system/switch2d.service ~/.config/systemd/user/switch2d.service
systemctl --user daemon-reload
```

## 1. Preflight must be clean

```fish
switch2d doctor 2>&1 | tee -a $LOG
```

**Pass condition: no FAIL, and the `udev uaccess rule` line is PASS.** A remaining WARN is
acceptable only if you can explain it from the detail text.

## 2. Start the daemon

Run it in the foreground for this round — logs are the point:

```fish
switch2d run 2>&1 | tee -a $LOG
```

(The service works too; then read `journalctl --user -u switch2d -f | tee -a $LOG`.)

Wake both pads. **Pass:** two `L2CAP link established`, both LEDs solid on the **same**
number (combined mode: the pair shares one player number now), and the log shows a DSU
line and no IMU warning.

```fish
grep -E "DSU motion listening|IMU node|vibration handle|Player LED" $LOG | tee -a $LOG
```

The line `vibration handle 0x00XX (uuid ..., pid 0x20xx)` is what this round exists for on
the rumble side — record both pads' values.

## 3. First: prove rumble exists at all

Do this **before** any game, so a game's own quirks can't confuse the result.

```fish
# a real FF device now exists; check the kernel agrees
evtest /dev/input/eventN        # the combined node
```

`evtest` can upload and play an effect: choose the device, then in the "Querying …" menu
look for the force-feedback section (`Event code FF_RUMBLE`). If your `evtest` build can
run `evtest --test-rumble`, even better.

**Pass condition:** the pad buzzes. Note **which half** buzzes — both, or only one.
If nothing buzzes, stop here and report; everything else in this round is unaffected.

## 4. A real game

Pick one of these, in order of what it proves:

| Target | Why | What to watch |
|---|---|---|
| **Dolphin** with any GameCube/Wii game whose controller config uses a GameCube pad + DSU motion | exercises sticks, buttons, rumble **and** DSU motion at once, and has a built-in motion-calibration screen | Controller → GameCube Pad → Configure: sticks/buttons respond; Motion Source = DSU Client; the motion calibration screen moves with the pad |
| **Steam** with any controller-enabled game | exercises the virtual gamepad the way most people will use it | game shows "Switch 2 Combined Joy-Cons", buttons correct, rumble effect fires |
| A native SDL game | exercises the IMU node and the joystick node together | `SDL_GAMECONTROLLER_USE_BUTTON_LABELS=0 steam-run game`, check `SDL` debug output if it misbehaves |

For each game, record:

1. Is the controller recognised, with the expected name?
2. Do all sticks and buttons do what the game expects? Specifically: **A/B/X/Y order** and
   **shoulder vs trigger** — a game is harsher than `evtest` about swapped axes.
3. Does rumble fire, and on which half?
4. Any input lag, dropout, or reconnection mid-game.

## 5. Idle sleep — the one item never verified

With a pad connected and untouched for **more than `idle_sleep_seconds` (300 s)**:

- **Pass:** the pad's LEDs go out (it was put to sleep and the link dropped), the log records
  the sleep, and pressing a button brings it back and streaming resumes.
- This takes five minutes; do it while a game is paused.

## 6. Leave it running, then stop

Play normally for a while. Then Ctrl-C and check nothing is left behind:

```fish
pgrep -af switch2d | grep -v pgrep      # empty
grep -c "ngc/" /proc/bus/input/devices  # 0
```

---

## Results to report

| Step | Result | Notes |
|---|---|---|
| 0 install + `which switch2d` | | |
| 1 `doctor`: any FAIL? udev PASS? | | |
| 2 both pads, LEDs same number | | |
| 2 `vibration handle` for each pad | | paste both lines |
| 3 rumble via evtest: buzzes? which half? | | |
| 4 game: recognised, name | | |
| 4 game: buttons/sticks correct | | |
| 4 game: rumble | | |
| 4 DSU motion (Dolphin calibration) | | |
| 5 idle sleep after 300 s | | |
| 6 clean shutdown | | |

## Known and expected — do not report

- **Split mode** is coded, never validated on hardware.
- **Pro Controller 2** and **NSO GameCube** are speculative: no hardware evidence exists.
- **`WARN GATT resolution … vibration=0x0012`** should no longer appear; if it does, it now
  means the *final* resolution failed, which is a real finding worth reporting.
- **`Skipping bonding (BondPolicy::Skip)`** on reconnect is normal.
- The daemon retrying a sleeping pad is now advertisement-driven: silence in the log until
  you press a button is correct behaviour, not a hang.

## If a game misbehaves but evtest was fine

That narrows it to the mapping layer, and the useful artefacts are: the game's own controller
config screen, `evtest` output for the same presses, and the `switch2d` log around the
launch. Note whether the game uses SDL2 or SDL3, and whether it reads the **combined** node
or both per-pad nodes.
