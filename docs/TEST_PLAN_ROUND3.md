# Test Round 3 — pair, validate in a live session, then attack the wake path

Build: **`1e50b79` + the two new commands** (`pair --hold`, `advert`) — I will tell you
the exact commit to use before you start; do not start on the old binary, `pair --hold`
is what makes phase B possible.

Why this shape: round 2 proved we connect from SYNC mode and never after a wake. So we
first **earn a session we know works**, measure everything in it, and only then drop the
link and characterise exactly what happens when the pad tries to come back. That way a
failure in phase C cannot contaminate the decode results, and a decode bug cannot be
mistaken for a connect bug.

---

## Background you need for the decision tree

A Nintendo pad's advertisement carries the **host address it wants to reconnect to**
(6 bytes at offset 10 of the manufacturer block, wire order = display MAC reversed):

| Host field | Meaning |
|---|---|
| `00:00:00:00:00:00` | no host stored → pairing mode, connectable by anyone (TTL 45 s) |
| our adapter MAC | bonded wake → the pad wants **us** (TTL 4 s) |
| any other address | bonded to a *different* host → it will ignore this machine (TTL 4 s) |

This is the field that decides everything below, and we have never looked at it on real
hardware.

---

## Phase A — clean slate and pair *while keeping the link*

```fish
set LEFT  BC:89:A6:45:83:CC
set RIGHT BC:89:A6:49:8D:74
systemctl --user stop switch2d.service
set LOG ~/switch2d-test/round3-(date +%H%M).log

# purge (same as round 2, so the round is reproducible)
~/switch2d-test/switch2d remove $LEFT ; ~/switch2d-test/switch2d remove $RIGHT
bluetoothctl remove $LEFT ; bluetoothctl remove $RIGHT
rm -f ~/.config/switch2d/led-players.json ~/.config/switch2d/status.json
```

Wake the Left pad, hold SYNC until the LEDs cycle, then:

```fish
~/switch2d-test/switch2d pair --hold 90 2>&1 | tee -a $LOG
```

**While the link is held**, do the full input validation — this is the only session we
know connects, so use it:

1. Sticks at rest 5 s → note `cal=` (~`0.00` expected).
2. Both sticks full deflection each way, hold ~1 s each → **record the extremes**.
3. Every button once, slowly: `A B X Y`, `L` `R`, `ZL` `ZR`, `+` `-`, `HOME`,
   `CAPTURE`, both stick clicks, `SL`/`SR` if reachable.
4. Rotate the pad left, then right → note `gyro=` sign per direction.
5. Note battery mV/% and that it does not jitter.
6. Keep an eye on the **Player LED** the whole time: it must stay solid for the duration
   of the hold. If it dies before the timeout, say so — that would mean the link dropped
   early.

Copy the `calib read stick=…` lines from the log if present.

Ctrl-C or let the 90 s expire.

## Phase B — what did the bond actually store?

Within ~10 seconds of the link dropping (the pad advertises in wake mode right after),
capture the advertisement:

```fish
~/switch2d-test/switch2d advert --seconds 20 2>&1 | tee -a $LOG
```

The header line prints our adapter MAC; each advert line prints the pad's `host=` field.

**This is the crux of the round.** Compare the host bytes with our adapter:

| `host=` value | Meaning |
|---|---|
| `00:00:00:00:00:00` | the bond never landed — our `SET_MAC` is wrong or ignored |
| `1E:D6:41:59:17:8C` (our adapter, reversed) | the bond landed; the pad wants us |
| anything else | the pad is bonded to a stale/foreign host |

Do this for the Right pad too, after its own pairing.

## Phase C — wake reconnect attempt

Wake the pad again with a button press, then immediately:

```fish
RUST_LOG=debug ~/switch2d-test/switch2d monitor $LEFT --seconds 45 2>&1 | tee -a $LOG
```

Record: did it connect; if yes, after how many rounds; if no, the final `Error:` line
verbatim.

## Phase D — only if Phase C fails: is our initiator address even the one the pad wants?

A pad in wake mode may use **directed advertising** to the stored host address. If our
controller presents a different address than the one we bonded (privacy/RPA, or an LE
identity address that is not the BR/EDR address), the connection request is silently
ignored — which looks exactly like a timeout.

```fish
sudo -n btmgmt info 2>&1 | tee -a $LOG      # note: addr, le address, privacy setting
sudo -n btmgmt -i 0 connect -t le $LEFT 2>&1 | tee -a $LOG
```

If `btmgmt info` shows privacy enabled, or an LE address different from
`8C:17:59:41:D6:1E`, retry Phase C **after**:

```fish
sudo -n btmgmt -i 0 privacy off
```

and also try the kernel's own connect immediately after a wake:

```fish
sudo -n btmgmt -i 0 disconnect $LEFT   # only if the previous connect succeeded
```

## Phase E — Right pad

Repeat A–D with `$RIGHT` (Player 2, two LEDs).

---

## Decision matrix

| Phase B host field | Phase C result | Conclusion | Next step |
|---|---|---|---|
| zeros | fails | bond never stored | fix `SET_MAC` payload/order; we are bonding to a pad that forgot us |
| our MAC | connects | success | run the daemon test, then close the round |
| our MAC | fails | the pad wants us and still refuses | Phase D decides: privacy/initiator address, or a genuine pad-side limitation |
| other address | fails | stale host burned into the pad | re-bond in SYNC mode (the upstream project's own remedy) |
| our MAC + Phase D `btmgmt connect` **succeeds** | fails in our socket path | our raw L2CAP connect is at fault, not the pad | fix our connect, not the strategy |
| our MAC + `btmgmt connect` **fails the same way** | fails | pad refuses any initiator in wake mode | strategy decision: keep-alive, or BlueZ-managed connection |

## Known good, do not re-test

Pairing itself, Player LED patterns (1 LED / 2 LEDs), the 6 s hold behaviour, the
clean-slate purge, and the CLI error text — all verified in round 2.

## Honest context

The upstream project never ran Joy-Con 2 on hardware at all (its README marks both
Joy-Con 2 columns `?` as "never verified"), and its own README calls wake-connect
"still needs real-world confirmation" and "flaky in development", shipping a watchdog
that restarts the service when it sees "adverts seen but no connect". So phase C is
genuinely exploratory: we may be the first to determine whether this works on this
hardware, and either answer is a result worth having.
