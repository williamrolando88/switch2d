# Test Round 6 — the whole product, from a clean slate

Build: `~/switch2d-test/switch2d` — record its identity before you start, it's the only
way to attribute the results later:

```fish
sha256sum ~/switch2d-test/switch2d | tee ~/switch2d-test/round6-build.txt
```

This round starts from **zero** — both Joy-Cons unpaired, config cleared — and ends with a
working daemon, so it is the round you can hand to someone else and have them reproduce.
Rounds 1–5 each tested one broken thing; this one tests whether the thing works.

## Logging rules (they are not optional)

```fish
set LOG ~/switch2d-test/round6-(date +%H%M).log
echo "log -> $LOG"
```

Every command goes through `2>&1 | tee -a $LOG`. The log file is the deliverable — it is
read directly, so a summary is worth less than the raw text. `-v` or `RUST_LOG=debug`
both enable debug output in this build; use plain runs unless a step says otherwise.

## What this round must prove

1. Pairing from scratch works on both pads, with correct Player LEDs.
2. Both sticks reach full range on real hardware — including the **Right** pad, whose
   calibration used to fall back to defaults (the `0.74` cap).
3. The daemon runs both pads at once and the combined node carries both.
4. Both pads' input arrives **simultaneously** (the round-1 starvation bug).
5. A woken pad reconnects without pressing SYNC.
6. The daemon survives a `bluetoothd` restart.

---

## 0. Pre-flight

```fish
systemctl --user stop switch2d.service          # the old unit must stay stopped
systemctl --user status switch2d.service | head -3
pgrep -af switch2d | grep -v pgrep              # must be empty
test -w /dev/uinput; and echo "uinput OK"
command -v evtest bluetoothctl btmgmt
```

## 1. Clean slate — purge every trace

```fish
set LEFT  BC:89:A6:45:83:CC
set RIGHT BC:89:A6:49:8D:74

~/switch2d-test/switch2d remove $LEFT
~/switch2d-test/switch2d remove $RIGHT
bluetoothctl remove $LEFT
bluetoothctl remove $RIGHT
rm -f ~/.config/switch2d/led-players.json ~/.config/switch2d/status.json
bluetoothctl devices                             # neither MAC present
grep -c "ngc/" /proc/bus/input/devices           # 0
```

**Pass:** `switch2d status` shows 0 bonded, and neither Joy-Con appears in
`bluetoothctl devices` (Xbox pad / headphones may remain).

If a MAC refuses to disappear, let the pad sleep ~30 s and repeat — it is re-advertising.

## 2. LE privacy must be off (otherwise nothing below can reconnect)

```fish
systemctl status switch2d-disable-le-privacy --no-pager 2>&1 | tee -a $LOG
sudo btmgmt -i 0 info 2>&1 | tee -a $LOG
```

**Pass:** the unit is `loaded … enabled` and `active (exited)`, and `current settings:`
does **not** contain `privacy` ( `ll-privacy` is fine). If `privacy` is listed:

```fish
sudo systemctl restart switch2d-disable-le-privacy
sudo btmgmt -i 0 info | grep "current settings"
```

and if it still shows up, stop here and paste the unit's status — every later step would
fail for that reason alone.

## 3. Pair Joy-Con 2 (Left) from scratch

Hold **SYNC** until the LEDs cycle, then:

```fish
~/switch2d-test/switch2d pair 2>&1 | tee -a $LOG
```

**Pass:** found via scan, `✅ Controller Successfully Paired & Bonded!`, Player 1, and
during the 6 s hold exactly **one** solid LED, dark afterwards.

Record: the `Dial to … succeeded after N rounds` line **N**. (Round 5 needed 2 for the
Left pad; if it is now 16 and failing, that's a regression, not bad luck.)

## 4. Pair Joy-Con 2 (Right) from scratch

Same, second pad: Player 2, **two** solid LEDs during the hold.

```fish
~/switch2d-test/switch2d status 2>&1 | tee -a $LOG
```

**Pass:** both listed, correct players, `bonded: true`.

## 5. Per-pad live validation (the one session we know connects)

Left pad awake, then:

```fish
~/switch2d-test/switch2d pair --hold 60 --help   # confirm the flag exists, then use:
RUST_LOG=debug ~/switch2d-test/switch2d monitor $LEFT --seconds 60 2>&1 | tee -a $LOG
```

Wait — `pair --hold` is the session that guarantees a connection; prefer it for the
measurements because it bonds **and** holds:

```fish
~/switch2d-test/switch2d pair $LEFT --hold 60 2>&1 | tee -a $LOG
```

While it holds, do all of this slowly:

| Action | Pass condition |
|---|---|
| Sticks at rest 5 s | `cal=` both `0.00`, no drift |
| Left stick full deflection, 4 directions | `[-1.00, 1.00]` |
| Every button once | correct name, no doubling, no misses |
| Rotate the pad | `gyro=` reacts; note sign per direction |
| Battery | stable, no jitter |

Then repeat for the Right pad. **On the Right pad the key line is the right stick
reaching `±1.00`** — it used to cap at `0.74`.

### Calibration evidence — paste these lines

```fish
grep -E "calib read|single-stick|Using default" $LOG | tee -a $LOG
```

Expected after the fix:
- Right pad: `single-stick pad: using the left calibration block for the right stick (0x001FC042)`
- Left pad: **no** `Using default right stick calibration` warning at all.

## 6. The daemon, both pads, combined node

```fish
~/switch2d-test/switch2d run 2>&1 | tee -a $LOG
```

Second terminal:

```fish
evtest        # choose "Nintendo Switch 2 Combined Joy-Cons"
```

Wake both pads. **Pass, in order:**

1. Two `L2CAP link established` lines, both `LE_PUBLIC`.
2. Player LEDs: one pad shows 1 LED, the other 2, both **stay** solid.
3. Daemon keeps running; no `EBUSY`, no exit, no restart.

Then, with `evtest` open, verify:

| Check | Pass condition |
|---|---|
| Left pad left stick | `ABS_X`/`ABS_Y` move, reach ≈ ±32767 |
| Right pad right stick | `ABS_RX`/`ABS_RY` move, reach ≈ ±32767 (not ±24000) |
| Left pad buttons | appear, correct names |
| Right pad buttons | appear, correct names |
| **Both pads pressed at once** | **both** register in the same second |
| ZL / ZR | analog range 0→255 |
| L / R | digital, full value |
| Idle 30 s | no drifting axes, no stuck buttons |

## 7. Reconnect without SYNC (the thing that never worked before round 4)

1. Leave one pad untouched until its LEDs go out (~60 s).
2. Press any button on it.
3. **Pass:** it reconnects on its own within a few seconds — LED returns, input resumes,
   no SYNC press. Record the `Dial to … succeeded after N rounds` line.
4. Repeat for the other pad.
5. Then sleep **both**, wake both within a second of each other, and confirm both return.

## 8. Does the daemon survive a BlueZ restart?

```fish
sudo systemctl restart bluetooth
```

Watch the daemon terminal for 30 s, then wake a pad.

**Record exactly what happens** — this is the only step with no prior data. Expected best
case: the privacy unit re-applies (`PartOf=bluetooth.service`), the daemon notices the
link drop, and the pad reconnects on the next wake. Worst case: the daemon needs a
restart. Either answer is useful; just say which.

```fish
systemctl status switch2d-disable-le-privacy --no-pager | head -6
sudo btmgmt -i 0 info | grep "current settings"
```

## 9. Idle sleep (optional, needs 5 minutes of patience)

`idle_sleep_seconds` is 300. With a pad connected and untouched for 5+ minutes, the
daemon should put it to sleep and drop the link, and a button press should bring it back.
Skip if you are short on time and say so.

## 10. Clean shutdown

Ctrl-C the daemon, then:

```fish
pgrep -af switch2d | grep -v pgrep       # empty
grep -c "ngc/" /proc/bus/input/devices   # 0
```

**Pass:** the node disappears, no pad is left holding a dead link, and re-running the
daemon works again immediately.

---

## Results

| Step | Pass/Fail | Notes (raw lines) |
|---|---|---|
| 1 clean slate | | |
| 2 privacy off | | |
| 3 pair Left (N rounds, 1 LED) | | |
| 4 pair Right (2 LEDs) | | |
| 5 Left stick full ±1.00 | | |
| 5 Right stick full ±1.00 | | |
| 5 buttons / IMU / battery | | |
| 5 calibration lines as expected | | |
| 6 daemon both pads, LEDs solid | | |
| 6 evtest: both sticks full range | | |
| 6 simultaneous presses | | |
| 7 reconnect Left / Right / both | | |
| 8 survives `bluetoothd` restart | | |
| 9 idle sleep | | |
| 10 clean shutdown | | |

## Known-open — do NOT report these as failures

- **Rumble does nothing** (HD stream and GC presets are coded, not wired to `EV_FF`).
- **DSU/cemuhook emits nothing**, and there is **no IMU evdev node** in the daemon yet.
- **Split mode** is coded but has never been validated on hardware.
- **Right pad vibration handle falls back to `0x0012`** even though its UUID is present;
  instrumented, not fixed. Rumble being silent is expected anyway.
- **Pro Controller 2 and NSO GameCube** are speculative: no hardware evidence exists.

## If something fails

Paste the log path and the ~20 lines around the failure, plus:

```fish
bluetoothctl info <MAC>
sudo btmgmt -i 0 info
dmesg | tail -20
```

and the pad's LED state at the moment of failure — advertising (cycling) versus asleep
(off) separates a protocol problem from a wake-timing one.
