# 🧪 Hardware Validation Test Results & Session Log

Date: 2026-11-11
Target Hardware: Nintendo Switch 2 Controllers (Joy-Con 2 Left & Joy-Con 2 Right)
Host: Linux (Bazzite) / Adapter `8C:17:59:41:D6:1E`
Initial Commit: `eb41905`

---

## 1. Environment & Pre-Flight Status

- **uinput Permissions**: Verified `/dev/uinput` is writable by user (`uinput OK`).
- **Required Tools**: `evtest`, `btmgmt`, `bluetoothctl` verified present.
- **Background Daemon**: `switch2d.service` was found running in an active auto-restart loop (restarting every 3s because 0 controllers were paired, restart counter >2300). The service was explicitly stopped with `systemctl --user stop switch2d` to prevent radio contention on adapter `8C:17:59:41:D6:1E`.
- **Controllers to Test**:
  - Joy-Con 2 (Left): Serial `HBW71022994899`, MAC `BC:89:A6:45:83:CC`
  - Joy-Con 2 (Right): Pending pairing & validation

---

## 2. Test Execution Log & Findings

### Finding 1: Auto-Scan Failure via `bluetoothctl` Stdio Block Buffering

```text
FINDING
  severity: blocks-testing
  build:    eb41905
  controller: Joy-Con 2 (Left) (BC:89:A6:45:83:CC)
  command:  cargo run --release -- pair
  expected: Scan for 10s, detect controller in pairing mode, print "Found Switch 2 controller: <MAC>" and connect.
  actual:   
    ===========================================================
       Scanning for Nintendo Switch 2 controllers (10s)...
       👉 Hold the circular SYNC button on your Joy-Con
          until the green LEDs cycle back and forth.
    ===========================================================
    No Switch 2 controllers found in pairing mode.
    Tip: If you know the MAC address, run: switch2d pair <MAC>
  extra:
    Inspection of `scan_for_joycon()` in src/main.rs:
    `Command::new("bluetoothctl").args(&["scan", "on"]).stdout(Stdio::piped()).spawn()`
    followed by `tokio::time::sleep(8s)` and `child.kill()`.
    Because bluetoothctl standard output is connected to a pipe (not a TTY), glibc applies 4096-byte block buffering.
    When `child.kill()` (SIGKILL) is invoked, the process is terminated without flushing its libc buffer,
    resulting in an empty stdout stream (only 27 bytes "SetDiscoveryFilter success\n" captured).
  reproducible: always
```

**Resolution & Action**:
Modified `scan_for_joycon()` to execute `bluetoothctl --timeout 8 scan on` synchronously within `tokio::task::spawn_blocking`, allowing `bluetoothctl` to handle its own clean timeout exit and flush stdout completely.

---

### Finding 2: `switch2d pair` Instant Process Exit Drops BLE Link & Extinguishes LEDs

```text
FINDING
  severity: wrong-behaviour
  build:    eb41905
  controller: Joy-Con 2 (Left) (BC:89:A6:45:83:CC)
  command:  cargo run --release -- pair BC:89:A6:45:83:CC
  expected: 
    README states: "the controller's player LED settles solid on Player 1 for the first and Player 2 for the second."
  actual:   
    INFO Connecting to Switch 2 controller BC:89:A6:45:83:CC...
    INFO L2CAP link established with BC:89:A6:45:83:CC (type: 1)
    INFO Negotiated ATT MTU: 247
    INFO Subscribed to command responses (handle 0x001B)
    INFO Identified controller: Joy-Con 2 (Left) (serial: HBW71022994899)
    WARN Using default right stick calibration (controller reports none)
    INFO Bonding sequence completed for BC:89:A6:45:83:CC
    INFO Player LED set to Player 1
    INFO Input streaming active (handle 0x000A)
    INFO Saved config to /home/wmorales/.config/switch2d/config.json
    ===========================================================
      ✅ Controller Successfully Paired & Bonded!
      Device:  Joy-Con 2 (Left)
      Address: BC:89:A6:45:83:CC
      Player:  1
    ===========================================================
    The LED on your controller is now set to Player 1.
    Saved to config. switch2d will now connect to it automatically!

    Tester report:
    "no, it never stopped in led 1. Since it never locked in position 1, I kept repeating the sync process because after a couple of seconds the leds just went off, so I pressed the sync button again to have the moving leds and they went off, I think I made this process 5 or 6 times and never stopped with a solid first led as confirmation."

  extra:
    The pairing routine completes and main() exits immediately (<50ms).
    Process termination closes the raw AF_BLUETOOTH L2CAP socket.
    Once disconnected, the Joy-Con hardware enters low-power standby and turns off its LEDs immediately.
    The user is left with no visual confirmation that the LED was solid, and pressing SYNC resets the controller's bonding/advertising state.
  reproducible: always
```

**Resolution & Action**:
1. Added a 6-second verification hold (`tokio::time::sleep(Duration::from_secs(6))`) in `Commands::Pair` before dropping the connection so the tester can visually observe and confirm the solid player LED pattern.
2. Updated user-facing output to clarify that Joy-Con LEDs shut off when disconnected after `pair` exits, and will illuminate when `switch2d run` or `switch2d monitor` actively holds the connection.
3. **Verification**: Re-ran `cargo run --release -- pair` with Left Joy-Con in sync mode. Auto-scan detected `BC:89:A6:45:83:CC` immediately, connected, and held link for 6s. Tester confirmed: **"it stopped with 2 leds on then they turned off again"** (Player 2 pattern `0x03` = 2 LEDs). Confirmed `COMMAND_LEDS` is operational and the prior issue was solely link teardown on process exit. Left Joy-Con configuration restored to Player 1.

---

### Step 2 Pairing: Joy-Con 2 (Right)

```text
Found Switch 2 controller: BC:89:A6:49:8D:74
Connecting to BC:89:A6:49:8D:74 (Player 2)...
INFO Connecting to Switch 2 controller BC:89:A6:49:8D:74...
INFO L2CAP link established with BC:89:A6:49:8D:74 (type: 1)
INFO Negotiated ATT MTU: 247
WARN GATT discovery incomplete; using NSO GameCube fallback handles: vibration=0x0012
INFO Subscribed to command responses (handle 0x001B)
INFO Identified controller: Joy-Con 2 (Right) (serial: HCW71024337199)
WARN Using default right stick calibration (controller reports none)
INFO Bonding sequence completed for BC:89:A6:49:8D:74
INFO Player LED set to Player 2
INFO Input streaming active (handle 0x000A)
INFO Saved config to ~/.config/switch2d/config.json
===========================================================
  ✅ Controller Successfully Paired & Bonded!
  Device:  Joy-Con 2 (Right)
  Address: BC:89:A6:49:8D:74
  Player:  2
===========================================================
The LED on your controller is now set to Player 2.
Holding connection for 6 seconds so you can verify the Player 2 LED...
Saved to config. (Joy-Con LEDs will turn off now until switch2d runs or monitors).
```

*Tester Observation*:
Tester confirmed: **"two leds turned on in the right joycon, then they went off"** after the 6s verification window. Joy-Con 2 (Right) hardware acknowledgement of Player 2 pattern (`0x03`) confirmed.

*Hardware Behavior Note (Wake & Discovery)*:
When disconnected, pressing any key on either Joy-Con triggers wake advertisement, where the green LEDs cycle back and forth (marquee) searching for a Bluetooth host. Because peripheral Joy-Cons do not illuminate static LEDs without an active BLE host connection, the LEDs continue cycling until an active host (such as `switch2d run` or `switch2d monitor`) connects and issues `COMMAND_LEDS`. If no host connects, the controller times out and sleeps.

*Note*: Discovered warning `WARN GATT discovery incomplete; using NSO GameCube fallback handles: vibration=0x0012` during GATT layout resolution. Controller successfully identified as `Joy-Con 2 (Right)` with serial `HCW71024337199`.

---

### Finding 3: `switch2d monitor` Fails with `ENOSYS (os error 38)` & Socket Reuse `EINVAL (os error 22)`

```text
FINDING
  severity: blocks-testing
  build:    eb41905
  controller: Both Joy-Con 2 Left (BC:89:A6:45:83:CC) and Right (BC:89:A6:49:8D:74)
  command:  
    cargo run --release -- monitor --seconds 60
    cargo run --release -- monitor BC:89:A6:49:8D:74 --seconds 60
  expected: Connects to bonded Joy-Con, illuminates solid Player LED (1 or 2), and streams decoded input reports for 60s.
  actual:   
    ===========================================================
       Live monitor  <MAC>  (Player N)
       Press buttons; Ctrl-C to stop early.
    ===========================================================
     INFO Connecting to Switch 2 controller <MAC>...
    Error: Custom { kind: ConnectionRefused, error: "L2CAP connect failed (pub: Function not implemented (os error 38), rnd: Invalid argument (os error 22))" }
  extra:
    1. In Linux kernel AF_BLUETOOTH (`net/bluetooth/`), `bt_to_errno()` returns `-ENOSYS` (errno 38) when the Bluetooth controller hardware rejects an LE connection with an unmapped/unsupported HCI status code.
    2. Socket reuse defect in `src/bluetooth/l2cap.rs`: When `self.connect_timeout(..., BDADDR_LE_PUBLIC, ...)` fails, it immediately attempts `self.connect_timeout(..., BDADDR_LE_RANDOM, ...)` on the identical socket file descriptor (`self.fd`). In Linux, a socket on which `connect()` has failed cannot be reused; subsequent `connect()` calls immediately fail with `EINVAL` (os error 22: Invalid argument).
    3. `Commands::Monitor` lacks the `ShellScanControl::prepare_global()` pre-connection scan suspension present in `Commands::Pair` (HANDOVER Bug 3), as well as BlueZ device object cleanup (HANDOVER Bug 4).
    4. Tester observation: On both Left and Right controllers, pressing a button initiates wake advertisement (cycling LEDs), but connection fails immediately. The controller's LEDs cycle for ~10 seconds and then time out and extinguish.
  reproducible: always
```

---

### Finding 4: Test C (`switch2d run`) Reconnect Loop Fails to Connect Both Controllers

```text
FINDING
  severity: blocks-testing
  build:    eb41905
  controller: Both Joy-Con 2 Left (BC:89:A6:45:83:CC) and Right (BC:89:A6:49:8D:74)
  command:  cargo run --release -- run
  expected: Daemon creates `/dev/uinput` node, connects to bonded Joy-Cons upon wake button press, sets Player LEDs (1 and 2 solid), and forwards inputs to the unified virtual gamepad.
  actual:   
    INFO Starting switch2d with adapter 8C:17:59:41:D6:1E
    INFO Active mode: Combined
    INFO Exposed virtual gamepad 'Nintendo Switch 2 Combined Joy-Cons' (phys: ngc/switch2-combined)
    INFO Connecting to Switch 2 controller BC:89:A6:45:83:CC...
    INFO Connecting to Switch 2 controller BC:89:A6:49:8D:74...
    (repeats every 1.5s in continuous reconnection loop, failing on every attempt)

    Tester observation:
    "I was pressing buttons first in left, then after seconds in right, then after seconds in both at the same time, neither of them stoped moving its leds, in the end after I stopped interaction the leds turned off again."

  extra:
    1. Kernel confirms `/dev/input/input211` ("Nintendo Switch 2 Combined Joy-Cons") created successfully.
    2. In `src/daemon.rs`, `SwitchController::connect_with` is invoked in an async task with `BondPolicy::Skip`. Each attempt delegates to `L2capSocket::connect_le` which hits the identical `ENOSYS` (os error 38) on `BDADDR_LE_PUBLIC` followed by `EINVAL` (os error 22) on `BDADDR_LE_RANDOM`.
    3. Neither controller can connect to the daemon while `connect_le` fails to negotiate the connection.
  reproducible: always
```

---

## 3. Summary of Code Modifications Made During Testing

| File | Change | Purpose |
|---|---|---|
| [`src/main.rs`](src/main.rs#L370-L376) | Added 6s connection hold before `pair` CLI exit | Allows tester visual confirmation of solid player LED before L2CAP socket disconnects |
| [`src/main.rs`](src/main.rs#L750-L775) | Replaced `child.kill()` with `bluetoothctl --timeout 8 scan on` | Resolves glibc stdio pipe block-buffering truncation during auto-discovery |

### Exact Git Diff (`src/main.rs`):

```diff
diff --git a/src/main.rs b/src/main.rs
index 55d6e6b..eee978b 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -371,7 +371,9 @@ async fn main() -> Result<(), Box<dyn std::error::Error>> {
                     println!("  Player:  {}", next_player);
                     println!("===========================================================");
                     println!("The LED on your controller is now set to Player {}.", next_player);
-                    println!("Saved to config. switch2d will now connect to it automatically!");
+                    println!("Holding connection for 6 seconds so you can verify the Player {} LED...", next_player);
+                    tokio::time::sleep(Duration::from_secs(6)).await;
+                    println!("Saved to config. (Joy-Con LEDs will turn off now until switch2d runs or monitors).");
                 }
                 Err(e) => {
                     println!("❌ Failed to connect to {}: {}", target_mac, e);
@@ -749,31 +751,29 @@ async fn run_selftest(seconds: u64, drive: bool) -> Result<(), Box<dyn std::erro
 }
 
 async fn scan_for_joycon() -> Option<String> {
-    // Start discovery via bluetoothctl in background for 8 seconds
-    let mut child = Command::new("bluetoothctl")
-        .args(&["scan", "on"])
-        .stdout(std::process::Stdio::piped())
-        .stderr(std::process::Stdio::null())
-        .spawn()
-        .ok()?;
-
-    tokio::time::sleep(Duration::from_secs(8)).await;
-    let _ = child.kill();
-    let output = child.wait_with_output().ok()?;
-
-    let text = String::from_utf8_lossy(&output.stdout);
-    for line in text.lines() {
-        if line.contains("Device") {
-            let parts: Vec<&str> = line.split_whitespace().collect();
-            for part in parts {
-                if is_valid_mac(part) && (part.starts_with("BC:") || line.to_lowercase().contains("joy-con")) {
-                    return Some(part.to_uppercase());
-                }
-            }
-        }
-    }
-
-    None
+    tokio::task::spawn_blocking(|| {
+        let output = Command::new("bluetoothctl")
+            .args(&["--timeout", "8", "scan", "on"])
+            .output()
+            .ok()?;
+
+        let text = String::from_utf8_lossy(&output.stdout);
+        for line in text.lines() {
+            if line.contains("Device") {
+                let parts: Vec<&str> = line.split_whitespace().collect();
+                for part in parts {
+                    let clean = part.trim();
+                    if is_valid_mac(clean) && (clean.starts_with("BC:") || line.to_lowercase().contains("joy-con")) {
+                        return Some(clean.to_uppercase());
+                    }
+                }
+            }
+        }
+
+        None
+    })
+    .await
+    .ok()?
 }
 
 fn is_valid_mac(s: &str) -> bool {
```

---

## 4. Current State & Next Steps

Both controllers are bonded in `~/.config/switch2d/config.json`:
- `[1] Joy-Con 2 (Left) (BC:89:A6:45:83:CC) - Player 1`
- `[2] Joy-Con 2 (Right) (BC:89:A6:49:8D:74) - Player 2`

**Next Test**:
- **Test A**: Live input report decode via `cargo run --release -- monitor --seconds 60` (Left, then Right).
  - Sticks at rest (~0.00) vs full deflection (checking ~0.36 scaling defect).
  - Buttons (one-by-one verification).
  - Accel/Gyro readings.
  - Battery voltage & percentage.
