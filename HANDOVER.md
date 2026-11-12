# 📋 Engineering Handover & Known Bugs Reference: `switch2d`

> [!IMPORTANT]
> **To All Autonomous Agents & Collaborators Working on `switch2d`:**
> This document details every critical bug, hardware quirk, kernel behavior, and protocol trap discovered and solved during the reverse engineering and Rust implementation of `switch2d`.
> **DO NOT** revert these architectural safeguards or re-introduce these known failure modes.

---

## 📌 Index of Solved Bugs & Hardware Traps

1. [Bug 1: Dual-Controller Lockout (Upstream Python Bug)](#1-dual-controller-lockout-upstream-python-bug)
2. [Bug 2: Bluetooth Hardware `EBUSY (os error 16)` on Parallel Connect](#2-bluetooth-hardware-ebusy-os-error-16-on-parallel-connect)
3. [Bug 3: Connection Timeout Caused by Active LE Discovery / Steam Scan](#3-connection-timeout-caused-by-active-le-discovery--steam-scan)
4. [Bug 4: Immediate Disconnect on Standard Bluetooth SMP Pairing](#4-immediate-disconnect-on-standard-bluetooth-smp-pairing)
5. [Bug 5: Marquee / Cycling LEDs & Dropped Unsynchronized Commands](#5-marquee--cycling-leds--dropped-unsynchronized-commands)
6. [Bug 6: 12-bit Joystick Endianness and Axis Sign Conventions](#6-12-bit-joystick-endianness-and-axis-sign-conventions)
7. [Bug 7: Split Mode 90° Axis Rotation & Button Reorientation](#7-split-mode-90-axis-rotation--button-reorientation)
8. [Bug 8: Linux `/dev/uinput` Permissions & Zero-Sudo Operation](#8-linux-devuinput-permissions--zero-sudo-operation)

---

### 1. Dual-Controller Lockout (Upstream Python Bug)

- **Symptom**: Only one Joy-Con could connect at a time. As soon as Joy-Con 2 (Left) connected, Joy-Con 2 (Right) was completely ignored and could never connect.
- **Root Cause**:
  In upstream `switch2-controllers-linux` (`ngc/bridge.py`), lines 331–338:
  ```python
  # BUGGY CODE in upstream:
  if connected_count:
      hub._scanning = False
      continue
  ```
  The bridge stopped scanning as soon as the *first* controller connected (`connected_count > 0`), locking out player 2.
- **Fix in `switch2d`**:
  Each controller runs an **isolated, independent reconnection worker loop** in [`src/daemon.rs`](file:///var/home/wmorales/repos/switch2d/src/daemon.rs). A connection on controller 1 does not stop or suppress listening for controller 2.

---

### 2. Bluetooth Hardware `EBUSY (os error 16)` on Parallel Connect

- **Symptom**: When both Joy-Cons advertise at once, one connects and the other immediately crashes or fails with `Device or resource busy (os error 16)`.
- **Root Cause**:
  Linux kernel Bluetooth subsystem (`net/bluetooth/l2cap_sock.c`) and Bluetooth HCI Controller hardware can only process **ONE** outgoing LE Connection Request (`HCI_LE_Create_Connection`) at any single moment.
  If two parallel Tokio tasks call `connect_le` simultaneously on two raw L2CAP sockets, the hardware returns `0x0C (Command Disallowed)`, which the kernel surfaces to userspace as `EBUSY (errno 16)`.
- **Fix in `switch2d`**:
  All connection attempts are serialized through an asynchronous lock:
  ```rust
  // In src/daemon.rs:
  let conn_permit = conn_lock.lock().await;
  let res = tokio::task::spawn_blocking(move || {
      SwitchController::connect(...)
  }).await;
  drop(conn_permit); // Release lock immediately so next pad can connect
  ```
  > [!WARNING]
  > **DO NOT remove `connect_lock`!** Removing this lock will immediately cause `EBUSY` when booting or waking both controllers together.

---

### 3. Connection Timeout Caused by Active LE Discovery / Steam Scan

- **Symptom**: `connect_le()` times out after 8–10 seconds even though the Joy-Con is visibly advertising with green flashing lights.
- **Root Cause**:
  Most modern Bluetooth adapters cannot perform LE Discovery (scanning) and create an outgoing LE L2CAP connection simultaneously.
  On Linux / Bazzite / SteamOS, Steam and `steamos-manager` keep background LE discovery running continuously. Standard BlueZ `StopDiscovery` only cancels the local D-Bus session, leaving the HCI radio scan active.
- **Fix in `switch2d`**:
  1. Call `bluetoothctl scan off` and HCI-level `btmgmt -i 0 stop-find -l`.
  2. A dedicated least-privilege sudoers rule was configured at `/etc/sudoers.d/nso-gc-btmgmt` for `/usr/bin/btmgmt -i 0 stop-find -l` so it runs without password prompts.

---

### 4. Immediate Disconnect on Standard Bluetooth SMP Pairing

- **Symptom**: Pairing via GNOME Bluetooth Settings, KDE Bluetooth, or `bluetoothctl pair` fails or disconnects after 0.5s.
- **Root Cause**:
  Switch 2 Joy-Cons do **not** use Bluetooth Classic or Security Manager Protocol (SMP). If the OS attempts SMP key exchange or encryption, the controller firmware abruptly closes the link.
- **Fix in `switch2d`**:
  Bypass BlueZ's pairing engine entirely:
  1. Open a raw L2CAP socket with `BT_SECURITY_LOW` (level = 1, key_size = 0).
  2. Connect directly to fixed ATT CID (`ATT_CID = 4`) using `SOCK_SEQPACKET`.
  3. Perform GATT and input handling in userspace. See [`src/bluetooth/l2cap.rs`](file:///var/home/wmorales/repos/switch2d/src/bluetooth/l2cap.rs).

---

### 5. Marquee / Cycling LEDs & Dropped Unsynchronized Commands

- **Symptom**: Green LEDs cycle back and forth forever in pairing mode, never showing solid player LED (LED 1 or LED 2).
- **Root Causes**:
  1. **Asynchronous Command Replies**: The Switch 2 controller does **NOT** respond to `ATT_READ_REQ` (0x0A) on characteristic handles. All command responses arrive as `ATT_HANDLE_VALUE_NTF` (0x1B) on the command response characteristic (`0x001A`).
  2. **Missing CCCD Subscription**: Before the controller will accept commands, the host MUST subscribe to the command response CCCD (`0x001B`).
  3. **Unsynchronized Command Execution**: If commands (like `COMMAND_LEDS`) are written without awaiting the controller's ACK packet (`resp[0] == command_id && resp[1] == 0x01`), subsequent packets collide or are dropped.
  4. **Bonding Sequence**: In pairing mode (Sync held), the controller requires the 4-step bonding sequence (`COMMAND_PAIR`: `SET_MAC`, `PAIR_LTK1`, `PAIR_LTK2`, `FINISH`) before acknowledging player LEDs.
- **Fix in `switch2d`**:
  In [`src/controller.rs`](file:///var/home/wmorales/repos/switch2d/src/controller.rs):
  - Subscribe to CCCD `0x001B` before sending commands.
  - Implement `write_command(...)` with condition variable waiting on `ATT_HANDLE_VALUE_NTF` for handle `0x001A`.
  - Transmit bonding keys so the Joy-Con recognizes this host adapter.
  - Send `COMMAND_LEDS` with payload `[pattern, 0, 0, 0]`. When ACK arrives, the controller locks its LEDs solid.

---

### 6. 12-bit Joystick Endianness and Axis Sign Conventions

- **Symptom**: Joysticks output erratic values, snap to corners, or have inverted vertical axes in games.
- **Root Cause**:
  1. **Packing**: 3 bytes contain two 12-bit unsigned values (0..4095).
     - Byte 0, 1, 2 decoded as little-endian `u32`:
     - `raw_x = value & 0x0FFF;`
     - `raw_y = (value >> 12) & 0x0FFF;`
     - Neutral center `(2048, 2048)` is `0x800 | (0x800 << 12) = 0x800800`, yielding bytes `[0x00, 0x08, 0x80]`.
  2. **Linux evdev Convention**:
     - In Linux evdev, pushing an analog stick **UP** produces a **negative** value (`-32768`), while pushing **DOWN** produces a **positive** value (`+32767`).
     - Nintendo reports raw higher Y when pushing up.
- **Fix in `switch2d`**:
  In [`src/protocol/calibration.rs`](file:///var/home/wmorales/repos/switch2d/src/protocol/calibration.rs) and [`src/uinput/combined.rs`](file:///var/home/wmorales/repos/switch2d/src/uinput/combined.rs):
  `ABS_Y` is emitted as `-scaled_y` and `ABS_RY` as `-scaled_ry`.

---

### 7. Split Mode 90° Axis Rotation & Button Reorientation

- **Symptom**: In 2-player Split Mode, holding a Joy-Con horizontally causes pushing up to move sideways, and face buttons do not match standard ABXY positions.
- **Root Cause**:
  - **Joy-Con Left** is rotated **90° Counter-Clockwise (CCW)** when held sideways with the rail on top.
  - **Joy-Con Right** is rotated **90° Clockwise (CW)** when held sideways with the rail on top.
- **Fix in `switch2d`**:
  Implemented in [`src/uinput/combined.rs`](file:///var/home/wmorales/repos/switch2d/src/uinput/combined.rs):
  - **Left Joy-Con Horizontal**:
    - Stick: `ABS_X = -ly`, `ABS_Y = -lx`
    - Buttons: Left Arrow $\rightarrow$ `BTN_SOUTH`, Down $\rightarrow$ `BTN_EAST`, Up $\rightarrow$ `BTN_WEST`, Right $\rightarrow$ `BTN_NORTH`
    - Shoulders: `SL_L` $\rightarrow$ `BTN_TL`, `SR_L` $\rightarrow$ `BTN_TR`
  - **Right Joy-Con Horizontal**:
    - Stick: `ABS_X = ry`, `ABS_Y = rx`
    - Buttons: Physical A $\rightarrow$ `BTN_SOUTH`, X $\rightarrow$ `BTN_EAST`, B $\rightarrow$ `BTN_WEST`, Y $\rightarrow$ `BTN_NORTH`
    - Shoulders: `SL_R` $\rightarrow$ `BTN_TL`, `SR_R` $\rightarrow$ `BTN_TR`

---

### 8. Linux `/dev/uinput` Permissions & Zero-Sudo Operation

- **Symptom**: Virtual device creation fails with `Permission denied (os error 13)`.
- **Root Cause**:
  Standard `/dev/uinput` permissions default to `root:root 0600` on minimal distributions.
- **Fix in `switch2d`**:
  Modern systemd-logind and udev assign ACL permissions (`uaccess`) to the locally logged-in user on seat0. On Bazzite / Fedora, verify permissions with `getfacl /dev/uinput`. The daemon runs fully unprivileged under the user's systemd user manager (`systemctl --user`).
  **Never require running `switch2d` as root.**

---

## 🛠️ Verification Checklist for Future Changes

Before submitting any Pull Request:
- [ ] Run `cargo check --verbose` and `cargo test --verbose` (must be warning-free).
- [ ] Verify that `connect_lock` in `src/daemon.rs` remains intact.
- [ ] Verify that command writes on `0x0014` wait for ACK on `0x001A`.
- [ ] Check memory consumption with `systemctl --user status switch2d` (target: < 5 MB RAM).
- [ ] Ensure all interactive console instructions provided to users use **Fish shell** syntax.
