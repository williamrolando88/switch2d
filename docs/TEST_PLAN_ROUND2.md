# Test Round 2 — connect fix, calibration, GATT UUIDs

Build under test: **`1e50b79`** — binary frozen at `~/switch2d-test/switch2d`.
Round 1 findings: `TEST_RESULTS.md` (repo root).
Use **this** binary, not `cargo run`: the source tree keeps moving and a rebuild
mid-session makes the results unattributable.

## What round 1 proved, so we do not re-test it

| Round 1 finding | Status in this build |
|---|---|
| Blinking-LED auto-scan swallowed its output (stdio buffering) | Fixed by the tester (`b223678`), carried forward |
| `pair` exited instantly, LEDs died before you could look | Fixed: 6 s hold, plus the LED-off explanation text |
| Player LEDs / bonding | **Confirmed working both pads. Do not re-test unless it regresses** |
| `monitor` / `run` failed `ENOSYS (38)` + `EINVAL (22)` on bonded pads | Fixed: fresh socket per address type, discovery stop per attempt, BlueZ Device object removed once before the first round, 16 short rounds × 800 ms |

## Three questions this round must answer

1. **Does a bonded pad connect at all now?** And if so, after how many rounds?
2. **Is calibration actually read** from Joy-Con 2, or are we silently running on
   built-in defaults? (Round 1 said "controller reports none" on *both* pads — every
   stick number so far came from defaults.)
3. **Which vibration UUID does the Right pad report?** Round 1 fell back to the
   GameCube handle `0x0012` on the Right only.

Everything below exists to answer those three, plus the normal input sanity checks.

---

## 0. Pre-flight (2 minutes)

```fish
# the daemon unit must stay stopped: it restart-loops and competes for the adapter
systemctl --user stop switch2d.service
systemctl --user status switch2d.service | head -5

# nothing of ours left over
pgrep -af "switch2d" | grep -v pgrep

# controller state
~/switch2d-test/switch2d status
test -w /dev/uinput; and echo "uinput OK"
```

Expected: service `inactive`, no processes, both Joy-Con listed as bonded
(Player 1 Left `BC:89:A6:45:83:CC`, Player 2 Right `BC:89:A6:49:8D:74`).

## 1. Capture everything to a log (do this first, once)

```fish
set LOG ~/switch2d-test/round2-(date +%H%M).log
echo "log -> $LOG"
```

Every command below assumes you pipe through `2>&1 | tee $LOG`. Without the
log we cannot triage; paraphrased reports cost us a whole round.

---

## 2. Connect test — LEFT pad

Wake it (press any button until the LEDs cycle), then immediately:

```fish
RUST_LOG=debug ~/switch2d-test/switch2d monitor BC:89:A6:45:83:CC --seconds 60 2>&1 | tee $LOG
```

**While it runs**, do the following slowly, one thing at a time:

1. Leave both sticks and triggers alone for 5 seconds.
2. Both sticks to full deflection: up, down, left, right — hold each ~1 s.
3. Every button once: `A B X Y`, `L` `R`, `ZL` `ZR`, `+` `-`, `HOME`, `CAPTURE`,
   both stick clicks, and the `SL`/`SR` shoulder rails if reachable.
4. Rotate the pad slowly to the left, then to the right.

### What to read in the log

| Line | Meaning | What we need |
|---|---|---|
| `Dial to <MAC> succeeded after N rounds` | connected | **N** (1 = first round, >1 = the retry envelope earned its keep) |
| `Attempting L2CAP connect (type …)` repeated | still failing | the final `Error:` line, verbatim |
| `GATT characteristic UUIDs: [...]` | discovered UUID set, once per process | **paste the whole line** |
| `calib read stick=… area=…` | calibration evidence | **paste both sticks, both areas** |
| `[#n] btn=… cal=(…)` | live decode | the stick extremes are the important numbers |
| `[stats] reports=…/s` | link health | note if it is near 0 or erratic |

## 3. Connect test — RIGHT pad

Repeat section 2 with `BC:89:A6:49:8D:74`. This is the pad that reported the
`vibration=0x0012` fallback and the pad whose UUID list matters most.

---

## 4. The measurements that matter (fill in)

### 4.1 Connection

| Pad | Connected? | Rounds (N) | Final error text (if any) |
|---|---|---|---|
| Left | | | |
| Right | | | |

### 4.2 Calibration — copy the two `calib read` lines per pad

```text
Left  user:    <paste>
Left  factory: <paste>
Right user:    <paste>
Right factory: <paste>
```

For each: is `resp_len` equal to 11? Is `first8` `FF FF FF …`? Does the log say
`dead_stick=true`? Any `FAILED … kind=…`?

### 4.3 Stick range (the inherited defect)

At rest, and at full deflection in each direction, record `cal=` for both sticks:

| Input | Left stick `cal=` | Right stick `cal=` |
|---|---|---|
| rest | | |
| full up | | |
| full down | | |
| full left | | |
| full right | | |

Decision rule: reaching ≈`±1.00` means the inherited `~0.36` scaling defect is
**not** active on these pads. Capping near `0.36` confirms it and gives us the
numbers to validate a fix against.

### 4.4 Buttons

List any that were **missing**, **doubled**, or showed the **wrong name**.
If all 15 were correct, write "all 15 ok" — that is a result too.

### 4.5 IMU signs

When rotating left vs right, does `gyro=` go positive or negative? At rest, which
axis reads ≈`1.0 g`? Signs are unverified and we need one pad's real behaviour.

### 4.6 GATT UUID list

```text
Left  : <paste INFO line>
Right : <paste INFO line>
```

Does the Right list contain `fa19b0fb-cd1f-46a7-84a1-bbb09e00c149`?
Does the Left list contain `289326cb-a471-485d-a8f4-240c14f18212`?

---

## 5. Only if section 2 or 3 still fails

Collect this before reporting:

```fish
bluetoothctl info BC:89:A6:45:83:CC   # and the Right MAC
sudo -n btmgmt info                  # current LE/BRedR state of the adapter
dmesg | tail -30
```

Note whether the pad was **advertising when the dial started** (LEDs cycling) or
already asleep (LEDs off) — that single fact separates a protocol failure from a
wake-timing failure.

## 6. Cleanup after the session

```fish
pkill -f "switch2d monitor" ; or true
grep -c "ngc/" /proc/bus/input/devices   # must be 0
pgrep -af switch2d | grep -v pgrep        # must be empty
```

Leave `switch2d.service` **stopped** until the daemon rewrite lands: today it
exits instead of idling, so systemd respawns it every 3 s and each respawn
competes for the same adapter.

---

## 7. Reporting

Append your filled-in results to this file (or a sibling
`TEST_RESULTS_ROUND2.md`) and keep the raw log. Rules that made round 1 useful:

- Paste **raw output**, never a summary of what you think happened.
- State the pad's LED state at the moment of each command.
- One finding per block: severity `blocks-testing | wrong-behaviour | cosmetic`,
  command, expected, actual, reproducible.
- If something is not covered here, still report it — round 1's best finding
  (calibration defaults) was not on my list either.
