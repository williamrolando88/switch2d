# Round 6B — verify the two fixes round 6 exposed

Build: **`b380fad`**, frozen at `~/switch2d-test/switch2d`, sha256
`afacee4ed953828d295d45165a910802fe01f5450bb692132e2e7d2f49bba003`.

Round 6 passed end to end, but two of its numbers disagreed with its own verdict. This
round is a targeted re-check of exactly those two things — **no purge, no re-pairing of
both pads**, about twelve minutes.

**Why not the full procedure:** since round 6 only two narrow patches landed (daemon
calibration, re-pair player selection). Everything else in round 6 exercised code that has
not changed. A full clean-slate round is owed after the daemon rewrite (09B), which does
change structure.

Do the steps in this order: A needs the daemon, B needs it stopped.

---

## 0. Prep

```fish
set LOG ~/switch2d-test/round6b-(date +%H%M).log
echo "log -> $LOG"

sha256sum ~/switch2d-test/switch2d | tee -a $LOG
# must print: afacee4ed953828d295d45165a910802fe01f5450bb692132e2e7d2f49bba003

systemctl --user stop switch2d.service
pgrep -af switch2d | grep -v pgrep
# must print NOTHING. If a daemon is already running, stop it: two daemons is what
# confused round 6's reconnect timings and produced a 16-round failure batch.
```

If the sha256 differs, stop — the binary was rebuilt and the results are not comparable.

## A. Daemon calibration (the `0.74` that survived round 6)

Round 6 measured `ABS_X` peaking near 24000 in `evtest` while `monitor` reported `±1.00`:
the daemon was feeding `StickCalibration::default()` into the combined mapper. Fixed in
`5e8c4a9`.

**Terminal 1:**

```fish
~/switch2d-test/switch2d run 2>&1 | tee -a $LOG
```

Wake both pads (press any button) and wait until the log shows two
`LSCAP link established` lines and both LEDs are solid — one pad with 1 LED, the other
with 2.

**Terminal 2** — record the output, do not eyeball the numbers:

```fish
grep -B3 "ngc/" /proc/bus/input/devices | grep -o "event[0-9]*"
# use that eventN below
evtest /dev/input/eventN 2>&1 | tee ~/switch2d-test/round6b-evtest.log
```

Do **only** these motions, each slowly, holding about a second at the extreme:

1. Left pad stick → **straight left**, then **straight right**, then release.
2. Left pad stick → **straight up**, then **straight down**, then release.
3. Right pad stick → **straight left**, then **straight right**, then release.
4. Right pad stick → **straight up**, then **straight down**, then release.

Then `Ctrl-C` evtest.

> Why "straight": the stick's circular gate caps a diagonal at ~0.71 of the axis range,
> which is most likely what round 6 actually measured. Only straight-axis pushes are a
> fair test of the calibration.

**Pass condition**

| Axis | Round 6 | Expected now |
|---|---|---|
| `ABS_X`, `ABS_Y` | ~±24000 | **±30000 or better** (ideally ±32767) |
| `ABS_RX`, `ABS_RY` | ~±24000 | **±30000 or better** |

## B. Re-pairing a bonded pad keeps its slot

**Stop the daemon first** (Ctrl-C in terminal 1) — it holds the link and `pair` needs the
pad in SYNC mode.

```fish
~/switch2d-test/switch2d status | tee -a $LOG      # note both players first
~/switch2d-test/switch2d pair 2>&1 | tee -a $LOG
```

Put the **Left** pad in SYNC mode when prompted (hold the round button until the LEDs
cycle). **Do not pass `-p`.**

**Pass conditions**

- log says `re-pairing … keeping Player 1`, **not** `assigning … to Player 3`
- the pad lights exactly **1** LED, not 3
- `~/switch2d-test/switch2d status` still shows Left = Player 1, Right = Player 2

## C. Optional — reconnect timing

Round 6 needed 9–12 dial rounds (8–11 s) after a wake, but a second daemon was holding
both links for part of that. This build logs one line per round:

```fish
RUST_LOG=debug ~/switch2d-test/switch2d run 2>&1 | tee -a $LOG
```

Let one pad sleep (untouched ~60 s), wake it, and let it connect. The log then contains
`Dial round n/16 …` lines and a `succeeded after N rounds` line. No action needed beyond
that — the file is read directly.

Skip this if you have had enough; it is the least important of the three.

---

## Reporting

Say "done" (or give the log path) and nothing else is required — the logs are read
directly. If a step fails, the useful extras are:

```fish
bluetoothctl info <MAC>
sudo btmgmt -i 0 info | grep "current settings"
dmesg | tail -20
```

plus the pad's LED state at that moment.

## Do not report as failures

Rumble silent, DSU silent, no IMU evdev node in the daemon, split mode unvalidated,
`vibration=0x0012` fallback on the Right pad, and idle sleep still unverified — that last
one is deliberately deferred to the full round after the daemon rewrite.
