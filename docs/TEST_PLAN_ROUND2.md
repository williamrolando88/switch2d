# Test Round 2 — full plan: clean slate, pair, connect, decode

Build under test: **`1e50b79`** — binary frozen at `~/switch2d-test/switch2d`.
Round 1 findings: `TEST_RESULTS.md` (repo root).
Use **this** binary, not `cargo run`: the source tree keeps moving and a rebuild
mid-session makes the results unattributable.

This round starts from a **blank slate**: every paired Joy-Con is removed from both
BlueZ and `switch2d`, then the pads are paired again from scratch. That is
deliberate — round 1 was the first pairing on this machine, so nothing we saw was
tested against a pre-existing bond, and the connect failure only appeared *after*
bonding. A full purge makes the whole flow reproducible by anyone.

## What round 1 proved, so we do not re-test it

| Round 1 finding | Status in this build |
|---|---|
| Blinking-LED auto-scan swallowed its output (stdio buffering) | Fixed by the tester (`b223678`) |
| `pair` exited instantly, LEDs died before you could look | Fixed: 6 s hold + explanation text |
| `monitor` / `run` failed `ENOSYS (38)` + `EINVAL (22)` on bonded pads | Fixed: fresh socket per address type, discovery stop per attempt, BlueZ device removal once per dial, 16 short rounds × 800 ms |

## Questions this round must answer

1. **Does pairing work from a clean slate** on both pads, and are the Player LEDs
   correct (`0x01` = 1 LED, `0x03` = 2 LEDs)?
2. **Does a bonded pad connect now**, and after how many rounds?
3. **Is calibration actually read** from Joy-Con 2, or are we silently on defaults?
4. **Which vibration UUID does the Right pad report?**
5. Do sticks, buttons, IMU and battery decode correctly on real hardware?

---

## 0. Pre-flight (2 minutes)

```fish
# the daemon unit must stay stopped: it restart-loops and competes for the adapter
systemctl --user stop switch2d.service
systemctl --user status switch2d.service | head -5

# nothing of ours left over
pgrep -af switch2d | grep -v pgrep

# environment
test -w /dev/uinput; and echo "uinput OK"
command -v evtest bluetoothctl btmgmt

# capture everything from here on
set LOG ~/switch2d-test/round2-(date +%H%M).log
echo "log -> $LOG"
```

Every command below is meant to be piped through `2>&1 | tee $LOG`. Without the
log we cannot triage, and a paraphrased report costs us a whole round.

## 1. Clean slate — purge every trace of a previous pairing

Run this **before touching the controllers**. It is the step that makes the round
reproducible.

```fish
set LEFT  BC:89:A6:45:83:CC
set RIGHT BC:89:A6:49:8D:74

# 1a. forget them in switch2d (config.json)
~/switch2d-test/switch2d remove $LEFT
~/switch2d-test/switch2d remove $RIGHT
~/switch2d-test/switch2d status                      # "Bonded Controllers (0)"

# 1b. forget them in BlueZ (this is the part that races raw ATT connects)
bluetoothctl remove $LEFT
bluetoothctl remove $RIGHT
bluetoothctl devices                                  # neither MAC may appear

# 1c. clear stale override/state files left by earlier sessions
rm -f ~/.config/switch2d/led-players.json
rm -f ~/.config/switch2d/status.json
ls -l ~/.config/switch2d/

# 1d. no virtual nodes, no stray sessions
grep -c "ngc/" /proc/bus/input/devices                # must be 0
pgrep -af switch2d | grep -v pgrep                    # must be empty
```

**Expected:** config lists 0 controllers, `bluetoothctl devices` shows neither
Joy-Con (an Xbox pad or headphones may remain — those are fine), no `ngc/` nodes.

**If a MAC will not disappear** from `bluetoothctl devices`, the pad is awake and
re-advertising. Let it sleep (30 s untouched) and repeat 1b. Report it if it
persists — that is itself a finding.

> Note: purging the host side does not clear the key stored on the Joy-Con. Pressing
> **SYNC** for ~5 s (until the LEDs cycle) makes it advertise fresh, which is what
> step 2 relies on anyway.

## 2. Pair Joy-Con 2 (Left) from scratch

Turn the pad on and hold **SYNC** (the small round button between `SL`/`SR`) until
its LEDs cycle back and forth. Then:

```fish
~/switch2d-test/switch2d pair 2>&1 | tee -a $LOG
```

**Watch the pad during the 6-second hold**, and record what you see.

Expected log:

```text
Scanning for Nintendo Switch 2 controllers (10s)...
Found Switch 2 controller: BC:89:A6:45:83:CC
Connecting to BC:89:A6:45:83:CC (Player 1)...
L2CAP link established ... type: ...
Negotiated ATT MTU: 247
Identified controller: Joy-Con 2 (Left) (serial: ...)
Bonding sequence completed ...
Player LED set to Player 1
✅ Controller Successfully Paired & Bonded!
Holding connection for 6 seconds so you can verify the Player 1 LED...
Saved to config. (Joy-Con LEDs will turn off now ...)
```

**Expected on hardware:** exactly **one** LED solid during the hold, then all LEDs
off when the process exits (normal — the pad sleeps without a host).

**Failure to record:** `No Switch 2 controllers found`, `Bonding NOT confirmed!`,
`Failed to connect ...`, LEDs that keep cycling instead of settling, or an LED
pattern that does not match the player number.

## 3. Pair Joy-Con 2 (Right) from scratch

Same procedure, second pad:

```fish
~/switch2d-test/switch2d pair 2>&1 | tee -a $LOG
```

**Expected:** `Player 2`, **two** LEDs solid (`0x03`) during the hold, then off.

## 4. Verify the slate is now deliberately dirty

```fish
~/switch2d-test/switch2d status 2>&1 | tee -a $LOG
cat ~/.config/switch2d/config.json
```

Expected: both controllers listed with the right player numbers, `bonded: true`,
and `mode: combined`.

---

## 5. Connect + decode — LEFT

Wake the pad (any button, LEDs start cycling), then immediately:

```fish
RUST_LOG=debug ~/switch2d-test/switch2d monitor BC:89:A6:45:83:CC --seconds 60 2>&1 | tee -a $LOG
```

**While it runs**, slowly and one at a time:

1. Leave both sticks and triggers alone for 5 s.
2. Both sticks to full deflection: up, down, left, right — hold each ~1 s.
3. Every button once: `A B X Y`, `L` `R`, `ZL` `ZR`, `+` `-`, `HOME`, `CAPTURE`,
   both stick clicks, and `SL`/`SR` if reachable.
4. Rotate the pad slowly left, then right.

### What to read in the log

| Line | Meaning | What we need |
|---|---|---|
| `Dial to <MAC> succeeded after N rounds` | connected | **N** (1 = first round, >1 = the envelope earned its keep) |
| `Attempting L2CAP connect (type …)` repeated | still failing | the final `Error:` line verbatim |
| `GATT characteristic UUIDs: [...]` | discovered UUID set, once per process | paste the whole line |
| `calib read stick=… area=…` | calibration evidence | paste both sticks, both areas |
| `[#n] btn=… cal=(…)` | live decode | the stick extremes are the numbers that matter |
| `[stats] reports=…/s` | link health | note if near 0 or erratic |

## 6. Connect + decode — RIGHT

Repeat section 5 with `BC:89:A6:49:8D:74`. This is the pad that reported the
`vibration=0x0012` fallback, so its UUID list matters most.

## 7. Daemon — only if 5 and 6 both stream

```fish
~/switch2d-test/switch2d run 2>&1 | tee -a $LOG
```

In a second terminal, watch both nodes and press buttons on both pads:

```fish
evtest            # pick the "Nintendo Switch 2 Combined Joy-Cons" node
```

Expected: one combined node, sticks/buttons from **both** pads, and a
`Dial to … succeeded` line per pad. Ctrl-C the daemon when done.

Known limits of this build: rumble does nothing, DSU emits nothing, and the
daemon still exits instead of idling when a pad is missing — report those only if
they behave differently from this description.

---

## 8. Measurements to fill in

### 8.1 Pairing from clean slate

| Pad | Pairing succeeded | Player | LEDs seen during hold | LEDs after exit | Errors |
|---|---|---|---|---|---|
| Left | | | | | |
| Right | | | | | |

### 8.2 Connection

| Pad | Connected? | Rounds (N) | Final error text (if any) |
|---|---|---|---|
| Left | | | |
| Right | | | |

### 8.3 Calibration — paste the `calib read` lines

```text
Left  user:    <paste>
Left  factory: <paste>
Right user:    <paste>
Right factory: <paste>
```

Per line: is `resp_len` 11? Is `first8` `FF FF FF …`? Does it say `dead_stick=true`?
Any `FAILED … kind=…`?

### 8.4 Stick range (the inherited defect)

| Input | Left stick `cal=` | Right stick `cal=` |
|---|---|---|
| rest | | |
| full up | | |
| full down | | |
| full left | | |
| full right | | |

Decision rule: ≈`±1.00` means the inherited `~0.36` scaling defect is **not**
active on these pads. Capping near `0.36` confirms it and gives us the numbers a
fix must be validated against.

### 8.5 Buttons

Missing / doubled / wrong-named buttons. If all 15 are correct, write
"all 15 ok" — that is a result too.

### 8.6 IMU

Rotating left vs right: does `gyro=` go positive or negative? At rest, which axis
reads ≈`1.0 g`? Signs are unverified and we need one pad's real behaviour.

### 8.7 GATT UUID lists

```text
Left  : <paste INFO line>
Right : <paste INFO line>
```

Does the Right list contain `fa19b0fb-cd1f-46a7-84a1-bbb09e00c149`?
Does the Left list contain `289326cb-a471-485d-a8f4-240c14f18241`?

---

## 9. If something fails — evidence to collect

```fish
bluetoothctl info BC:89:A6:45:83:CC     # and the Right MAC
bluetoothctl devices
sudo -n btmgmt info                     # adapter state, LE/BRedR
dmesg | tail -30
```

Also state the one fact that separates causes: **was the pad advertising when the
command started** (LEDs cycling) or **already asleep** (LEDs off)?

## 10. Cleanup

```fish
pkill -f "switch2d monitor"; or true
pkill -f "switch2d run"; or true
grep -c "ngc/" /proc/bus/input/devices    # must be 0
pgrep -af switch2d | grep -v pgrep        # must be empty
```

Leave `switch2d.service` **stopped** until the daemon rewrite lands: today it
exits instead of idling, so systemd respawns it every 3 s and each respawn
competes for the same adapter.

Do **not** re-pair after the test unless you intend another round — the purge in
step 1 is what makes each round comparable.

---

## 11. Reporting

Append the filled-in results here (or to `TEST_RESULTS_ROUND2.md`) and keep the
raw log. Rules that made round 1 useful:

- Paste **raw output**, never a summary of what you think happened.
- State the pad's LED state at the moment of each command.
- One finding per block: severity `blocks-testing | wrong-behaviour | cosmetic`,
  command, expected, actual, reproducible.
- Report anything not on this list — round 1's best finding (calibration quietly
  defaulting on both pads) was not on my list either.
