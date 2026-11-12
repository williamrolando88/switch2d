# Test Round 5 — the daemon, both pads, end to end

Build: **`b916382`** — binary at `~/switch2d-test/switch2d`.
Supersedes nothing: rounds 1–4 were about connecting at all. **This is the first
round where the product is supposed to work as a product.**

## Logging — read this first

Every command goes through `tee`. I read these files directly, so the log is the
deliverable, not a summary:

```fish
set LOG ~/switch2d-test/round5-(date +%H%M).log
echo "log -> $LOG"
```

`RUST_LOG=debug` and `-v` both work now (the binary used to ignore `RUST_LOG`; fixed in
this build). Use plain runs first; only re-run with debug if something fails.

## Step 1 — install the LE-privacy unit (once)

```fish
sudo install -Dm755 system/disable-le-privacy.sh /usr/local/bin/disable-le-privacy.sh
sudo install -Dm644 system/switch2d-disable-le-privacy.service /etc/systemd/system/switch2d-disable-le-privacy.service
sudo systemctl daemon-reload
sudo systemctl enable --now switch2d-disable-le-privacy.service
systemctl status switch2d-disable-le-privacy.service --no-pager 2>&1 | tee -a $LOG
sudo btmgmt -i 0 info 2>&1 | tee -a $LOG
```

**Pass condition:** `current settings:` no longer lists `privacy` (the `ll-privacy`
capability may stay). If `privacy` is still there, the unit could not power-cycle the
adapter — paste its `systemctl status` output.

Note: this is the only sudo in the whole round. Nothing below needs root.

## Step 2 — daemon startup with no pads awake

```fish
systemctl --user stop switch2d.service    # keep the old unit out of the way
~/switch2d-test/switch2d run 2>&1 | tee -a $LOG
```

In a second terminal, check the node exists:

```fish
grep -A3 "ngc/" /proc/bus/input/devices | tee -a ~/switch2d-test/round5-nodes.log
```

**Pass condition:** one combined gamepad with `phys=ngc/...`, and no crash or restart
loop while nothing is connected. Leave the daemon running.

## Step 3 — wake both pads

Press a button on the Left pad, wait for it to connect, then the Right.

**Pass conditions, in order:**

1. Both appear: `INFO L2CAP link established ... (type: LE_PUBLIC)` ×2.
2. Both Player LEDs go solid (1 LED and 2 LEDs) and **stay** solid.
3. The daemon keeps running — no `EBUSY`, no exit, no restart.

## Step 4 — input through the combined node

```fish
evtest          # pick "Nintendo Switch 2 Combined Joy-Cons"
```

Do this slowly and note each result:

1. **Left stick** → only the combined left stick moves.
2. **Right stick** → only the combined right stick moves.
3. **A/B/X/Y on the Right pad** and **the Left pad's buttons** → all appear, correct
   mapping, nothing duplicated.
4. **Press both pads' buttons simultaneously** → both register. (Round 1's HANDOVER
   Bug 1 was exactly this: one pad silently starving the other.)
5. **ZL/ZR** analog range, and **L/R** as digital.
6. Leave everything alone for 30 s → the node must go quiet (no drifting axes, no
   stuck buttons).

## Step 5 — the thing that has never worked before: reconnect

1. Let a pad go to sleep (untouched ~60 s, LEDs off).
2. Wake it with a button.
3. **Pass condition:** it reconnects on its own within a few seconds, LED returns, input
   resumes — with `privacy off` and no SYNC press.
4. Repeat for the other pad, then do both at once.
5. Then: `sudo systemctl restart bluetooth`, and check whether the daemon recovers
   (the unit is `PartOf=bluetooth.service`, so it should re-apply privacy; the daemon
   may need a push). Record what actually happens — this is the most likely real-world
   failure, because Steam and BlueZ restarts are common.

## Step 6 — idle sleep (only if time allows)

`idle_sleep_seconds` is 300 in the config. With a pad connected and untouched for 5+
minutes, we expect the daemon to sleep the controller and drop the link, then wake it
again on a button press. If you don't want to wait, say so and we test it later.

## Step 7 — stop cleanly

Ctrl-C the daemon, then:

```fish
pgrep -af switch2d | grep -v pgrep      # must be empty
grep -c "ngc/" /proc/bus/input/devices  # must be 0
```

**Pass condition:** the virtual node disappears and no controller is left holding a
stale link.

---

## Results to report

| Step | Pass/Fail | Notes |
|---|---|---|
| 1 privacy unit installed, `privacy` gone | | |
| 2 node created while idle | | |
| 3 both pads connect, LEDs solid | | |
| 4 all inputs through the combined node | | |
| 4.4 simultaneous presses | | |
| 5 reconnect without SYNC (each pad, both) | | |
| 5.5 recovery after `bluetoothd` restart | | |
| 6 idle sleep | | |
| 7 clean shutdown, no leftovers | | |

For any failure: the raw log lines around it, plus what the LEDs were doing.

## Known-open items that are NOT failures of this round

- **Right pad stick caps near ±0.74** — its calibration read still falls back to
  defaults. The new build prints the evidence at WARN:
  `Using default right stick calibration: user=0x001FC062 (N bytes), factory=0x000130E8 (...)`.
  Paste that line if you see it; it decides the fix.
- **Right pad vibration falls back to handle 0x0012** — instrumented, not yet fixed.
- **Rumble does nothing** in the daemon (not wired yet), **DSU emits nothing**, **no IMU
  node**. Expected: those are the next tasks.
- **`switch2d.service` (the old user unit) must stay stopped** until the daemon rewrite
  lands; it exits when idle and systemd respawns it every 3 s, which competes for the
  adapter.
