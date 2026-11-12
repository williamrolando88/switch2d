# Supported controllers — what is tested and what is not

This project distinguishes **validated** behaviour (observed on real hardware, logged and
reproduced) from **speculative** behaviour (implemented from the protocol reference or
from the upstream Python project, never observed here). Nothing in the speculative rows
should be quoted as working.

Last updated: after round 6 hardware sessions (build `ab31ac3`).

Round 6 validated the full path end to end: clean-slate pairing, both pads, combined
input without starvation, wake reconnect without SYNC, and automatic recovery after a
`bluetoothd` restart. Two defects it exposed are listed as open in the table below.

## Legend

| Mark | Meaning |
|---|---|
| ✅ validated | exercised on real hardware, evidence in `TEST_RESULTS_ROUND*.md` |
| ⚠️ validated with a known defect | works, but a specific behaviour is wrong or unproven |
| 🧪 speculative | implemented from the upstream protocol work, **never observed on hardware** |
| ❌ unsupported | refused at connect / advert level |
| — | not implemented |

## Controllers

| Controller | PID | Status | Evidence / notes |
|---|---|---|---|
| Joy-Con 2 (Left) | `0x2067` | ✅ validated | Rounds 1–4. Pairing, LEDs (1 LED = P1), 11 buttons, left stick `±1.00`, IMU present, battery stable. |
| Joy-Con 2 (Right) | `0x2066` | ⚠️ validated with a known defect | Rounds 1–4. Pairing, LEDs (2 LEDs = P2), 11 buttons, IMU, battery. **Right stick caps at `±0.74`** because its calibration read falls back to defaults. |
| Pro Controller 2 | `0x2069` | 🧪 speculative | Same protocol family assumed. Untested: buttons, sticks, rumble, IMU, LEDs. Not refused, but do not trust it. |
| NSO GameCube Controller | `0x2073` | 🧪 speculative | Analog triggers and preset rumble paths exist in code but have never run. Upstream tested this model, we have not. Do not trust it. |
| Any other PID | — | ❌ unsupported | Filtered at the advertisement level. |

## Features, per controller

| Feature | Joy-Con 2 L/R | Pro 2 | NSO GC |
|---|---|---|---|
| Pairing in SYNC mode | ✅ | 🧪 | 🧪 |
| Bond stored (`SET_MAC` + LTK) | ✅ verified via advertised host field | 🧪 | 🧪 |
| Reconnect after wake | ✅ round 6, no SYNC needed; takes 9-12 dial rounds (~8-11 s) | 🧪 | 🧪 |
| Player LED pattern | ✅ (1 LED / 2 LEDs observed) | 🧪 | 🧪 |
| Digital buttons | ✅ all 11 per pad | 🧪 | 🧪 |
| Stick range | ✅ Left / ⚠️ Right (`0.74`, calibration fallback) | 🧪 | 🧪 |
| Analog triggers | — (Joy-Con 2 has none) | 🧪 (digital ZL/ZR) | 🧪 (analog branch, never run) |
| Battery mV / % | ✅ stable to ~7 mV | 🧪 | 🧪 |
| IMU decode (accel/gyro) | ✅ values stream and respond | 🧪 | 🧪 |
| IMU evdev node | 🧪 coded (`src/uinput/motion.rs`), not wired into the daemon yet | 🧪 | 🧪 |
| HD rumble (60 Hz stream) | 🧪 coded, not wired to force-feedback | 🧪 | 🧪 |
| Preset rumble | — | — | 🧪 coded, not wired |
| DSU motion output | 🧪 coded, not wired | 🧪 | 🧪 |
| Combined mode | 🧪 implemented, round 5 hardware validation pending | — | — |
| Split mode | 🧪 coded, not hardware-validated | — | — |

## What "speculative" means for you as a user

If you plug in a Pro Controller 2 or an NSO GameCube pad, the daemon may connect and may
even produce input. That is not the same as support: no part of those paths has been
observed working, and a bug report that begins "the GameCube pad does X" will be answered
with "we have no hardware evidence for that path".

## How the code keeps this honest

Each controller has a profile module under `src/profiles/` that owns every
model-specific fact: product id, display name, vibration characteristic UUID, rumble
layout, trigger style, calibration addresses, LED patterns and the `Support` status.
The generic connection, ATT, report-decoding and uinput code asks the profile instead
of branching on the PID.

That is deliberate: a contributor with a new pad can work inside its profile module and
tests, flip its status there and in this table, and cannot regress the validated
Joy-Con 2 path. `tests/support_matrix.rs` fails if the profile status and this table
disagree.

## Promoting a row from 🧪 to ✅

1. Run the round's plan (at minimum: pair, hold-session decode, wake reconnect).
2. Add a `TEST_RESULTS_*.md` with raw logs, as in rounds 1–4.
3. Update this table and the README hardware table in the same commit as the controller's
   status change (the relevant per-model constants/profile), so documentation and code
   cannot drift. `tests/support_matrix.rs` fails if this table drifts from the code.
