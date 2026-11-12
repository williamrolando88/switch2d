# Test Round 4 — capture the actual HCI conversation on a failed wake connect

Build: **`70bf2e5`** (plus the `advert` liveness fix, commit will be given before you start).

Round 3 settled two questions for good:

- The bond is stored correctly — both pads advertise `host=0A:0B:0C:0D:0E:0F`.
- Input decoding works on real hardware — sticks, 15 buttons, IMU, battery, LEDs.

So the only open question is why a pad that is explicitly asking for this host still
refuses the connection. Round 4 does not change code; it records what the controller
actually puts on the air.

## Two candidate explanations, and how they differ

| | What would be true | What the capture shows |
|---|---|---|
| **A. Initiator address** | the kernel connects using a random (resolvable private) address, so the pad's directed advertising does not match the host it stored | `LE Create Connection` with `Own_Address_Type = random (0x01)` |
| **B. Advertising window / timing** | the pad only accepts a connection during a short directed-advertising burst, and our attempts land outside it | `Own_Address_Type = public (0x00)`, peer address correct, and `LE Connection Complete` with status `0x3E` (failed to be established) |

The distinction matters: A is fixable by changing how we initiate, B is a hard timing
constraint that changes the whole strategy.

## Steps

Terminal A (needs your sudo password, `btmon` is not covered by the btmgmt rule):

```fish
sudo btmon -w /tmp/btmon-round4.log
```

Terminal B — wake the Left pad (button press, LEDs cycle), then:

```fish
set LOG ~/switch2d-test/round4-(date +%H%M).log
~/switch2d-test/switch2d advert --seconds 15 2>&1 | tee -a $LOG
bluetoothctl devices 2>&1 | tee -a $LOG
RUST_LOG=debug ~/switch2d-test/switch2d monitor AA:BB:CC:DD:EE:01 --seconds 45 2>&1 | tee -a $LOG
```

Stop `btmon` (Ctrl-C), then extract the interesting frames:

```fish
btmon -r /tmp/btmon-round4.log 2>/dev/null | \
  grep -E "LE (Set Random Address|Create Connection|Connection Complete|Advertising Report|Enhanced Connection Complete)" \
  -A 6 | tee -a $LOG
```

Paste that block. It contains everything we need:

- the **advertising report**: the pad's address, its address type, and whether the adv
  is `ADV_IND` (connectable, general) or `ADV_DIRECT_IND` (directed, short burst);
- **LE Create Connection**: our `Own_Address_Type` and the peer address/type we used;
- **LE Connection Complete**: the HCI status — `0x00` connected, `0x3C`/`0x3E` failed.

## Privacy experiment (only after the capture)

Round 3 attributed the failure to `privacy` / `ll-privacy` being enabled on `hci0`.
That is plausible but not yet proven: the kernel normally uses the controller's public
address for a peer it has no IRK for, and these pads have no SMP bond by design, so
there should be no IRK. Test it directly rather than assume.

```fish
sudo btmgmt -i 0 power off
sudo btmgmt -i 0 privacy off
sudo btmgmt -i 0 power on
```

Then wake the pad and repeat the `monitor` command. Afterwards restore the setting:

```fish
sudo btmgmt -i 0 power off
sudo btmgmt -i 0 privacy on
sudo btmgmt -i 0 power on
```

If it connects with privacy off, the earlier HCI capture will show why, and the fix
belongs in our dial path rather than in the strategy.

## Phase P — privacy experiment (was skipped in round 3, do it explicitly)

Round 3 read `privacy` and `ll-privacy` in `btmgmt info` but never turned them off, so
that hypothesis is still open. `btmgmt` on this host does have the subcommands
(`btmgmt privacy <on|off>`, `btmgmt power <on|off>`), and `sudo -n` is not enough for
them — you need your interactive password. Do this in a normal terminal, not through
the frozen binary.

```fish
# 1. record the before-state
sudo btmgmt -i 0 info 2>&1 | tee -a $LOG

# 2. the sequence (privacy can only be changed while powered off)
sudo btmgmt -i 0 power off
sudo btmgmt -i 0 privacy off
sudo btmgmt -i 0 power on

# 3. VERIFY it actually stuck - this is the step that was missed last time
sudo btmgmt -i 0 info 2>&1 | tee -a $LOG
```

In step 3, `current settings:` must **no longer contain** `privacy` or `ll-privacy`.
If they are still there, `bluetoothd` re-applied them on power-up; go to Phase P2.

```fish
# 4. with privacy off, wake the pad and dial
~/switch2d-test/switch2d monitor AA:BB:CC:DD:EE:01 --seconds 45 2>&1 | tee -a $LOG
```

### Phase P2 — only if privacy came back in step 3

Stop BlueZ entirely so it cannot re-apply the setting. Our dial path does not need it
(raw L2CAP), it only loses the advert source and the object removal, both of which are
best-effort.

```fish
sudo systemctl stop bluetooth
sudo btmgmt -i 0 power on
sudo btmgmt -i 0 info 2>&1 | tee -a $LOG        # privacy now stays off
# wake the pad, then:
~/switch2d-test/switch2d monitor AA:BB:CC:DD:EE:01 --seconds 45 2>&1 | tee -a $LOG
```

This variant is also a result on its own: if the pad connects with `bluetoothd`
stopped, then BlueZ's presence — not our code — is what blocks the reconnect, and the
fix is architectural (keep BlueZ away from the pad, or let BlueZ own the link).

### Restore afterwards, always

```fish
sudo btmgmt -i 0 power off
sudo btmgmt -i 0 privacy on
sudo btmgmt -i 0 power on
sudo systemctl start bluetooth
systemctl --user stop switch2d.service
sudo btmgmt -i 0 info 2>&1 | tee -a $LOG        # privacy back, service down
```

Do not leave the box with privacy off or `bluetoothd` stopped.

## Also capture, from the round 3 log

The calibration diagnostics were in that session but not pasted. They decide the Right
pad's stick range:

```fish
grep -E "calib read|memory read" ~/switch2d-test/round3-0519.log | tee -a $LOG
```

Both sticks, both areas, with `resp_len` and the first bytes.

## Repeat for the Right pad

Same four steps with `AA:BB:CC:DD:EE:02`. One pad is enough to prove the mechanism; the
second confirms it is not model-specific.

## What each outcome means

| Capture shows | Meaning | Next step |
|---|---|---|
| `Own_Address_Type = 0x01` (random) | explanation A confirmed | force a public initiator address in the dial path |
| privacy off and the pad **connects** | A confirmed without needing the capture | same fix, and Phase P2 tells us whether BlueZ may own the link instead |
| `Own_Address_Type = 0x00`, peer correct, status `0x3E` | explanation B confirmed | the pad only accepts inside a burst: change dialing strategy (connect immediately on the advert, not on a timer) |
| `Own_Address_Type = 0x00`, peer correct, status `0x00` | we connect at HCI level and then drop | the fault is above L2CAP — look at the ATT/handshake side |
| `ADV_DIRECT_IND` visible | the pad really is calling us specifically, which narrows timing to the burst | measure how long after the button press the first burst starts |
| no advertising report at all while LEDs cycle | the pad advertises only to a bonded peer — check whether BlueZ is hiding the report from us | capture with `btmon` filter off |

## Do not

Do not purge, re-pair, or change the pad order. Round 3 left both pads bonded, and that
state is exactly what we need; a re-pair would reset the very thing under test.
