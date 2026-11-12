# Round 6B — verify the two fixes round 6 exposed

Build: **`b380fad`**, sha256 `afacee4ed953828d295d45165a910802fe01f5450bb692132e2e7d2f49bba003`.

Round 6 passed end to end, but its numbers caught two defects the report missed.
This is a short verification of those fixes only — about ten minutes, no purge, no
re-pairing of the other pad.

```fish
set LOG ~/switch2d-test/round6b-(date +%H%M).log
sha256sum ~/switch2d-test/switch2d | tee -a $LOG
systemctl --user stop switch2d.service
pgrep -af switch2d | grep -v pgrep          # must be empty: only ONE daemon may run
```

## A. Daemon calibration (the `0.74` that survived round 6)

Round 6 measured `ABS_X` peaking at ~23750 in `evtest` while `monitor` said `±1.00`: the
daemon was feeding `StickCalibration::default()` into the combined mapper. Fixed in
`5e8c4a9`.

```fish
~/switch2d-test/switch2d run 2>&1 | tee -a $LOG
```

Second terminal:

```fish
evtest        # Nintendo Switch 2 Combined Joy-Cons
```

Wake both pads, then:

1. **Left pad stick pushed straight left, then straight right**, hold each 1 s.
2. **Right pad stick pushed straight up, then straight down**, hold each 1 s.
3. Write down the extreme `ABS_X` / `ABS_Y` / `ABS_RX` / `ABS_RY` values.

**Pass condition:**

| Axis | Before (round 6) | Expected now |
|---|---|---|
| `ABS_X`, `ABS_Y` | ~±24000 | **±30000 or better** (ideally ±32767) |
| `ABS_RX`, `ABS_RY` | ~±24000 | **±30000 or better** |

Pushing **straight along one axis** matters: a circular gate caps a diagonal at ~0.71 of
the axis range, which is what round 6 probably measured.

## B. Re-pairing a bonded pad keeps its slot

With both pads bonded (Player 1 / Player 2) and this time **no `-p` flag**, re-pair the
Left pad while it is in SYNC mode:

```fish
~/switch2d-test/switch2d pair 2>&1 | tee -a $LOG
```

**Pass conditions:**

- log says `re-pairing … keeping Player 1` (not `assigning … to Player 3`)
- the pad lights exactly **1 LED**, not 3
- `switch2d status` still lists it as Player 1

Then confirm the Right pad is untouched and still Player 2.

## C. Reconnect (optional, only if you have patience)

Round 6 needed 9–12 dial rounds (8–11 s) after a wake, with a second daemon accidentally
running during part of it. This build logs one DEBUG line per round:

```fish
RUST_LOG=debug ~/switch2d-test/switch2d run 2>&1 | tee -a $LOG
```

Let a pad sleep, wake it, and paste the `Dial round n/16` lines plus the
`succeeded after N rounds` line. That tells us whether the failing rounds had
`scan_stop`/`device_removal` failures or were simply rounds where the pad was not yet
advertising.

**Important:** run exactly one daemon. Round 6's slow reconnects coincided with a second
daemon holding both links, which is worth ruling out before we change any timing.

## Do not report as failures

Rumble silent, DSU silent, no IMU node, split mode unvalidated, `vibration=0x0012`
fallback on the Right pad, idle sleep still unverified.
