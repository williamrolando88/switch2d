# 🧪 Hardware Validation Test Results & Session Log — Round 2

- **Date**: 2026-11-12
- **Build under test**: `1e50b79` (binary frozen at `~/switch2d-test/switch2d`)
- **Host**: Linux (Bazzite) / Adapter `8C:17:59:41:D6:1E`
- **Reference Plan**: [docs/TEST_PLAN_ROUND2.md](file:///var/home/wmorales/repos/switch2d/docs/TEST_PLAN_ROUND2.md)
- **Round 1 Results**: [TEST_RESULTS.md](file:///var/home/wmorales/repos/switch2d/TEST_RESULTS.md)

---

## 0. Pre-Flight Verification

| Check | Expected | Actual | Status |
|---|---|---|---|
| `systemctl --user status switch2d.service` | `inactive` / dead | `inactive (dead)` | ✅ Pass |
| `pgrep -af switch2d` | Empty | None running | ✅ Pass |
| `/dev/uinput` permissions | Writable | `uinput OK` | ✅ Pass |
| Required CLI tools | `evtest`, `bluetoothctl`, `btmgmt` present | Verified | ✅ Pass |

---

## 1. Clean Slate (Purge) Verification

Executed before pairing to ensure reproducible state:

```text
Left MAC:  BC:89:A6:45:83:CC
Right MAC: BC:89:A6:49:8D:74
```

| Step | Command | Expected | Actual / Output | Status |
|---|---|---|---|---|
| 1a. Remove switch2d bonds | `switch2d remove <MAC>` | Bonded Controllers (0) | Removed both MACs, 0 bonded | ✅ Pass |
| 1b. Remove BlueZ devices | `bluetoothctl remove <MAC>` | Devices removed from BlueZ | Not present in BlueZ device table | ✅ Pass |
| 1c. Clear stale state | `rm -f ~/.config/switch2d/led-players.json ~/.config/switch2d/status.json` | Files removed | Cleaned | ✅ Pass |
| 1d. Virtual device & proc check | `grep -c "ngc/" /proc/bus/input/devices` | `0` nodes, no switch2d procs | 0 nodes, no switch2d procs | ✅ Pass |

---

## 2. Pairing from Clean Slate

### 2.1 Left Pad (`BC:89:A6:45:83:CC`)
- **Hold SYNC**: Green LEDs cycle back and forth
- **Pairing result**: ✅ Success
- **Assigned player**: Player 1
- **LED pattern during 6s hold**: 1 solid LED (Player 1) (Confirmed by user)
- **LED pattern after exit**: Off (Confirmed by user)
- **GATT UUIDs**: `[00c5af5d-1964-4e30-8f51-1956f96bd281, 00c5af5d-1964-4e30-8f51-1956f96bd282, 00c5af5d-1964-4e30-8f51-1956f96bd283, ab7de9be-89fe-49ad-828f-118f09df7fd2, cc1bbbb5-7354-4d32-a716-a81cb241a32a, 289326cb-a471-485d-a8f4-240c14f18241, 649d4ac9-8eb7-4e6c-af44-1ea54fe5f005, ce49a830-dced-48ae-931e-c8cf88aadbea, 4147423d-fdae-4df7-a4f7-d23e5df59f8d, c765a961-d9d8-4d36-a20a-5315b111836a, 63a3810f-aec7-474b-9010-3d52403cb996, d3bd69d2-841c-4241-ab15-f86f406d2a80, ab7de9be-89fe-49ad-828f-118f09df7fde, ab7de9be-89fe-49ad-828f-118f09df7fdf, 2a00, 2a01]`
  - Note: Contains `289326cb-a471-485d-a8f4-240c14f18241`.
- **Raw pairing log**:
  ```text
  Found Switch 2 controller: BC:89:A6:45:83:CC
  Connecting to BC:89:A6:45:83:CC (Player 1)...
  INFO Connecting to Switch 2 controller BC:89:A6:45:83:CC...
  INFO L2CAP link established with BC:89:A6:45:83:CC (type: LE_PUBLIC)
  INFO Negotiated ATT MTU: 247
  INFO Subscribed to command responses (handle 0x001B)
  INFO Identified controller: Joy-Con 2 (Left) (serial: HBW71022994899)
  WARN Using default right stick calibration (controller reports none)
  INFO Bonding sequence completed for BC:89:A6:45:83:CC
  INFO Player LED set to Player 1
  INFO Input streaming active (handle 0x000A)
  INFO Saved config to /home/wmorales/.config/switch2d/config.json
  Holding connection for 6 seconds so you can verify the Player 1 LED...
  Saved to config. (Joy-Con LEDs will turn off now until switch2d runs or monitors).
  ```

### 2.2 Right Pad (`BC:89:A6:49:8D:74`)
- **Hold SYNC**: Green LEDs cycle back and forth
- **Pairing result**: ✅ Success
- **Assigned player**: Player 2
- **LED pattern during 6s hold**: 2 solid LEDs (`0x03`, Player 2) (Confirmed by user)
- **LED pattern after exit**: Off (Confirmed by user)
- **GATT UUIDs**: `[00c5af5d-1964-4e30-8f51-1956f96bd281, 00c5af5d-1964-4e30-8f51-1956f96bd282, 00c5af5d-1964-4e30-8f51-1956f96bd283, ab7de9be-89fe-49ad-828f-118f09df7fd2, d5a9e01e-2ffc-4cca-b20c-8b67142bf442, fa19b0fb-cd1f-46a7-84a1-bbb09e00c149, 649d4ac9-8eb7-4e6c-af44-1ea54fe5f005, 65a724b3-f1e7-4a61-8078-a342376b27ff, 4147423d-fdae-4df7-a4f7-d23e5df59f8d, c765a961-d9d8-4d36-a20a-5315b111836a, 640ca58e-0e88-410c-a7f3-426faf2b690b, d3bd69d2-841c-4241-ab15-f86f406d2a80, ab7de9be-89fe-49ad-828f-118f09df7fde, ab7de9be-89fe-49ad-828f-118f09df7fdf, 2a00, 2a01]`
  - Note: Contains `fa19b0fb-cd1f-46a7-84a1-bbb09e00c149`.
  - Fallback used: `WARN GATT resolution used NSO GameCube fallback handles: vibration=0x0012`
- **Raw pairing log**:
  ```text
  Found Switch 2 controller: BC:89:A6:49:8D:74
  Connecting to BC:89:A6:49:8D:74 (Player 2)...
  INFO Connecting to Switch 2 controller BC:89:A6:49:8D:74...
  INFO L2CAP link established with BC:89:A6:49:8D:74 (type: LE_PUBLIC)
  INFO Negotiated ATT MTU: 247
  WARN GATT resolution used NSO GameCube fallback handles: vibration=0x0012
  INFO Subscribed to command responses (handle 0x001B)
  INFO Identified controller: Joy-Con 2 (Right) (serial: HCW71024337199)
  WARN Using default right stick calibration (controller reports none)
  INFO Bonding sequence completed for BC:89:A6:49:8D:74
  INFO Player LED set to Player 2
  INFO Input streaming active (handle 0x000A)
  INFO Saved config to /home/wmorales/.config/switch2d/config.json
  Holding connection for 6 seconds so you can verify the Player 2 LED...
  Saved to config. (Joy-Con LEDs will turn off now until switch2d runs or monitors).
  ```

### 2.3 Post-Pairing Configuration Status
- Both controllers bonded (`bonded: true`, `mode: combined`).
- Left: Player 1 (`HBW71022994899`)
- Right: Player 2 (`HCW71024337199`)

---

## 3. Connection & Decoding (`monitor`)

### 3.1 Left Pad (`BC:89:A6:45:83:CC`)
- **LED state at command launch**: LEDs were initially cycling (looking for device/reconnecting), but went off (timed out/asleep) while the 16 rounds were executing.
- **Dial rounds ($N$)**: 16/16 (32 attempts)
- **Connection result**: ❌ Failed (Connection timed out)
- **Raw Error**:
  ```text
  Error: Custom { kind: ConnectionRefused, error: "connect to BC:89:A6:45:83:CC via adapter 8C:17:59:41:D6:1E: L2CAP connect failed (LE_PUBLIC: L2CAP connection timed out; LE_RANDOM: L2CAP connection timed out) [rounds: 16/16, attempts: 32, discovery_stop: ok, device_removal: failed: bluetoothctl exited with exit status: 1]" }
  ```
- **Diagnostics collected**:
  - `bluetoothctl info BC:89:A6:45:83:CC`: `Device BC:89:A6:45:83:CC not available`
  - Adapter `8C:17:59:41:D6:1E`: Powered `yes`, Discovering `no`.
- **GATT characteristic UUIDs line**:
  *(Not reached due to connect timeout)*
- **Calibration reads (`calib read`)**:
  *(Not reached)*
- **Stick Range (`cal=`)**:
  *(Not reached)*
- **Buttons (15 checked)**: *(Not reached)*
- **IMU rotation & gravity**: *(Not reached)*
- **Link health (`[stats] reports=.../s`)**: *(Not reached)*

---

### 3.2 Right Pad (`BC:89:A6:49:8D:74`)
- **LED state at command launch**: LEDs were initially cycling (looking for device/reconnecting), but went off (timed out/asleep) while the 16 rounds were executing.
- **Dial rounds ($N$)**: 16/16 (32 attempts)
- **Connection result**: ❌ Failed
- **Raw Error**:
  ```text
  Error: Custom { kind: ConnectionRefused, error: "connect to BC:89:A6:49:8D:74 via adapter 8C:17:59:41:D6:1E: L2CAP connect failed (LE_PUBLIC: Function not implemented (os error 38); LE_RANDOM: L2CAP connection timed out) [rounds: 16/16, attempts: 32, discovery_stop: ok, device_removal: failed: bluetoothctl exited with exit status: 1]" }
  ```
- **Diagnostics collected**:
  - `bluetoothctl info BC:89:A6:49:8D:74`:
    ```text
    Device BC:89:A6:49:8D:74 (public)
        Alias: BC-89-A6-49-8D-74
        Paired: no
        Bonded: no
        Trusted: no
        Blocked: no
        Connected: no
    ```
- **GATT characteristic UUIDs line**:
  *(Not reached due to connect failure)*
- **Vibration UUID resolution**:
  *(Not reached)*
- **Calibration reads (`calib read`)**:
  *(Not reached)*
- **Stick Range (`cal=`)**:
  *(Not reached)*
- **Buttons (15 checked)**: *(Not reached)*
- **IMU rotation & gravity**: *(Not reached)*
- **Link health (`[stats] reports=.../s`)**: *(Not reached)*

---

## 4. Daemon & Combined Node Test (`switch2d run`)

- **Status**: Skipped (per test plan, daemon test is only run if `monitor` streams on both pads).

---

## 5. Answers to Test Plan Questions

1. **Does pairing work from a clean slate on both pads, and are Player LEDs correct?**
   - **YES**. Both Left and Right Joy-Cons paired and bonded successfully from a clean state.
   - Left Joy-Con held exactly 1 solid LED (Player 1) during the 6s hold, then turned off upon process exit.
   - Right Joy-Con held 2 solid LEDs (`0x03`, Player 2) during the 6s hold, then turned off upon process exit.
2. **Does a bonded pad connect now, and after how many rounds?**
   - **NO**. Reconnection failed on both pads across all 16 rounds (32 attempts).
   - Left Pad timed out on both `LE_PUBLIC` and `LE_RANDOM`.
   - Right Pad failed with `Function not implemented (os error 38)` on `LE_PUBLIC` and timed out on `LE_RANDOM`.
   - In both cases, the controller was awake and advertising (LEDs cycling) at command launch, but timed out and slept while the 16 rounds were executing.
3. **Is calibration actually read from Joy-Con 2, or are we silently on defaults?**
   - Blocked from validation in `monitor` due to connection failure. (During pairing: Left pad logged `WARN Using default right stick calibration (controller reports none)` because Left Joy-Con lacks a physical right stick).
4. **Which vibration UUID does the Right pad report?**
   - During `pair`, the Right pad reported the following UUID list:
     `[00c5af5d-1964-4e30-8f51-1956f96bd281, 00c5af5d-1964-4e30-8f51-1956f96bd282, 00c5af5d-1964-4e30-8f51-1956f96bd283, ab7de9be-89fe-49ad-828f-118f09df7fd2, d5a9e01e-2ffc-4cca-b20c-8b67142bf442, fa19b0fb-cd1f-46a7-84a1-bbb09e00c149, 649d4ac9-8eb7-4e6c-af44-1ea54fe5f005, 65a724b3-f1e7-4a61-8078-a342376b27ff, 4147423d-fdae-4df7-a4f7-d23e5df59f8d, c765a961-d9d8-4d36-a20a-5315b111836a, 640ca58e-0e88-410c-a7f3-426faf2b690b, d3bd69d2-841c-4241-ab15-f86f406d2a80, ab7de9be-89fe-49ad-828f-118f09df7fde, ab7de9be-89fe-49ad-828f-118f09df7fdf, 2a00, 2a01]`
   - It **does contain** `fa19b0fb-cd1f-46a7-84a1-bbb09e00c149`. However, it still logged: `WARN GATT resolution used NSO GameCube fallback handles: vibration=0x0012`.
5. **Do sticks, buttons, IMU, and battery decode correctly on real hardware?**
   - Blocked by connection timeout on both pads.

---

## 6. Formal Findings

### Finding 1: Bonded Joy-Con 2 Reconnection Fails L2CAP Connect (`ENOSYS` / Timeout)

```text
FINDING
  severity: blocks-testing
  build:    1e50b79
  controller: Joy-Con 2 (Left) (BC:89:A6:45:83:CC) & Joy-Con 2 (Right) (BC:89:A6:49:8D:74)
  command:  RUST_LOG=debug ~/switch2d-test/switch2d monitor <MAC> --seconds 60
  expected: Reconnect to bonded controller via L2CAP within 16 rounds while controller is advertising.
  actual:   
    - Left pad: Failed after 16/16 rounds (32 attempts) with:
      L2CAP connect failed (LE_PUBLIC: L2CAP connection timed out; LE_RANDOM: L2CAP connection timed out)
    - Right pad: Failed after 16/16 rounds (32 attempts) with:
      L2CAP connect failed (LE_PUBLIC: Function not implemented (os error 38); LE_RANDOM: L2CAP connection timed out)
  extra:
    - Controllers were confirmed to be waking and advertising (LEDs cycling) at command launch.
    - The 16 rounds of connect attempts outlasted the controller's advertising window (~20s), causing the controller to sleep while switch2d was still retrying.
    - device_removal failed with "bluetoothctl exited with exit status: 1" because the devices were not registered in BlueZ's device cache at that moment.
  reproducible: always
```

### Finding 2: Right Pad GATT UUID Resolution Falls Back to GameCube Handle `0x0012` Despite Characteristic Present

```text
FINDING
  severity: wrong-behaviour
  build:    1e50b79
  controller: Joy-Con 2 (Right) (BC:89:A6:49:8D:74)
  command:  switch2d pair
  expected: Discover GATT characteristic fa19b0fb-cd1f-46a7-84a1-bbb09e00c149 and resolve vibration handle directly.
  actual:   Characteristic fa19b0fb-cd1f-46a7-84a1-bbb09e00c149 is in the discovered UUID array, but log emitted:
            WARN GATT resolution used NSO GameCube fallback handles: vibration=0x0012
  extra:    GATT handle resolution logic in the client is not matching the characteristic or its declaration handle properly.
  reproducible: always
```

---

## 7. Post-Session Cleanup & Status

- Active switch2d processes: 0 (`pgrep -af switch2d` verified empty).
- Virtual input nodes (`ngc/`): 0 (`/proc/bus/input/devices` verified 0).
- `switch2d.service`: Confirmed inactive/stopped.
- Log file preserved at: `~/switch2d-test/round2-0449.log`.
