# Test Round 3 — why does a woken pad refuse the connection?

Build: `1e50b79`. Everything below is **read-only diagnostics**: no code changes, no
purging, no re-pairing. Time needed: ~15 minutes per pad.

Round 2 established a hard fact: **pairing works** (pad in SYNC mode, `LE_PUBLIC`),
**reconnecting a woken pad does not** (16/16 rounds, both pads). That difference is
the whole question of this round.

## The hypothesis we are testing

A Nintendo pad in wake mode does not just "advertise" — its manufacturer block
carries a **bonded-host field** (6 bytes at offset 10 of the Nintendo block). That
is the address of the host it expects to reconnect to. If the pad stored the wrong
host address, it is looking for a machine that will never answer, and our
create-connection is ignored → exactly the timeouts we see.

So: **what host address is the pad advertising for, and does it match our adapter
`8C:17:59:41:D6:1E`?**

## Steps

```fish
set LEFT  BC:89:A6:45:83:CC
set RIGHT BC:89:A6:49:8D:74
systemctl --user stop switch2d.service
set LOG ~/switch2d-test/round3-(date +%H%M).log
```

### 1. Wake the pad and capture its advertisement

Wake the Left pad (press any button; LEDs start cycling), then within a second:

```fish
bluetoothctl --timeout 20 scan on >/dev/null 2>&1 &
sleep 6
busctl --system call org.bluez / org.freedesktop.DBus.ObjectManager \
    GetManagedObjects --json=short > /tmp/objs.json
wait
```

Now extract the pad's advert. The JSON has one entry per device; look for the pad's
MAC and paste the whole `ManufacturerData` value:

```fish
python3 -c "
import json
d=json.load(open('/tmp/objs.json'))['data'][0]
for path,ifaces in d.items():
    dev=ifaces.get('org.bluez.Device1')
    if not dev: continue
    if 'BC:89:A6:45:83:CC' in path.upper():
        print('path    ', path)
        print('address ', dev.get('Address'), dev.get('AddressType'))
        print('name    ', dev.get('Name'), dev.get('Alias'))
        print('rssi    ', dev.get('RSSI'))
        print('md      ', dev.get('ManufacturerData'))
" 2>&1 | tee -a $LOG
```

Also record:

```fish
bluetoothctl devices 2>&1 | tee -a $LOG     # note (public) vs (random) and the MAC
```

### 2. What the advert says

The Nintendo block is the entry whose key is `0x7e05` (VID `0x057E` little-endian).
Paste it whole. We are reading, in order:

| bytes | meaning |
|---|---|
| 0–1 | VID, `7e 05` |
| 2–3 | PID (e.g. `69 20` = Pro 2, `73 20` = GC) |
| 4–9 | controller MAC, wire order |
| 10–15 | **bonded host MAC, wire order** — the field this round exists for |
| 16– | state bytes (battery etc.) |

Decide: does bytes 10–15 equal our adapter in wire order
(`1E D6 41 59 17 8C`)? Or is it all zeros (nobody bonded), all `FF`, or some other
address?

### 3. Can the kernel itself connect? (independent of our code)

```fish
sudo -n btmgmt info 2>&1 | tee -a $LOG
sudo -n btmgmt -i 0 connect -t le $LEFT 2>&1 | tee -a $LOG
```

and, if the sudoers rule does not cover `connect`, run it with an interactive
password:

```fish
sudo btmgmt -i 0 connect -t le $LEFT 2>&1 | tee -a $LOG
```

Record the exact result, even `Command not permitted`. If it connects, disconnect
immediately with `sudo btmgmt -i 0 disconnect $LEFT` — do not leave the link up.

### 4. Our own dial, same session

Wake the pad again (if it slept) and:

```fish
RUST_LOG=debug ~/switch2d-test/switch2d monitor $LEFT --seconds 40 2>&1 | tee -a $LOG
```

### 5. Repeat 1–4 for the Right pad

---

## How to read the results

| Observation | Conclusion |
|---|---|
| Host field = `1E D6 41 59 17 8C` (our adapter), connect still times out | The pad wants us; the failure is in the radio/kernel path. Next step is a `btmon` capture. |
| Host field = all zeros / all `FF` | The pad never stored a host: our bond sequence did not take effect. Fix the bond payload, not the connect code. |
| Host field = some other address | The pad bonded to a wrong or stale host address. Compare with what we send in `SET_MAC` and check byte order. |
| `btmgmt connect` succeeds where `monitor` fails | Decisive: our raw L2CAP path is at fault, not the pad or the adapter. |
| `btmgmt connect` also fails with `0x3e`/timeout | The pad is not accepting connections in wake mode at all — then we need to know what a Switch does differently (see below). |
| Pad appears in `bluetoothctl devices` with a different MAC / `(random)` | The pad is using a private address; we must dial the advertised address, not the bonded one. |

## Why this is not (yet) a dead end

The Switch does not have a magic channel either: it bonds once, then the pad
advertises and the console issues a normal BLE create-connection. The Nintendo
advert block exists precisely so the pad can say *which host* it wants — which means
the reconnect decision is host-address based, and that is something we can inspect
and reproduce. The dead-end only becomes real if step 3 shows the pad refuses
connections from any host while woken *and* refuses to advertise a host field at
all; in that case the wake path would need to be triggered by something we have not
identified (for example, the pad may only accept a connection while its LEDs are in
a specific phase of the wake animation).

Do not change code based on this round. Send the log.
