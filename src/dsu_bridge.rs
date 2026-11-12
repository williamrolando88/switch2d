//! Glue between controller sessions and the four-slot DSU motion server.
//!
//! [`crate::dsu`] owns the UDP protocol and slot table. This module owns the
//! process lifetime of the server thread, maps players to ADR-3 slots, and
//! assembles the per-report [`MotionInput`] frame. The daemon only needs
//! [`DsuBridge::start`] once, then `session_started` / `update` /
//! `session_ended` per pad session.
//!
//! Motion is optional: a failed bind or thread spawn logs a warning and leaves
//! a no-op bridge instead of failing daemon startup.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use tracing::{debug, warn};

use crate::controller::CalibratedInput;
use crate::dsu::{self, DsuConfig, DsuServer, MotionInput};
use crate::protocol::report::InputReport;

/// DSU motion lifecycle plus session-to-slot glue.
pub struct DsuBridge {
    server: Option<Arc<DsuServer>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    started: Instant,
}

impl DsuBridge {
    /// Bind and start the DSU server thread without blocking.
    ///
    /// A bind/spawn failure is logged and returns a working no-op bridge:
    /// motion is optional, so it must never stop the daemon.
    pub fn start(bind: SocketAddr) -> io::Result<Self> {
        let started = Instant::now();
        let server = match DsuServer::bind(DsuConfig { bind }) {
            Ok(server) => Arc::new(server),
            Err(err) => {
                warn!("DSU motion disabled: failed to bind {bind}: {err}");
                return Ok(Self {
                    server: None,
                    stop: Arc::new(AtomicBool::new(false)),
                    thread: None,
                    started,
                });
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        let thread_server = Arc::clone(&server);
        let thread_stop = Arc::clone(&stop);
        let spawned = thread::Builder::new()
            .name("dsu-server".to_string())
            .spawn(move || {
                if let Err(err) = thread_server.run(thread_stop) {
                    warn!("DSU server stopped: {err}");
                }
            });

        match spawned {
            Ok(handle) => Ok(Self {
                server: Some(server),
                stop,
                thread: Some(handle),
                started,
            }),
            Err(err) => {
                warn!("DSU motion disabled: could not spawn server thread: {err}");
                Ok(Self {
                    server: None,
                    stop,
                    thread: None,
                    started,
                })
            }
        }
    }

    /// Mark a player's slot connected and return the ADR-3 slot it used.
    ///
    /// Players 5-8 clamp to slot 3; [`DsuServer::set_player_slot`] emits the
    /// once-per-`(mac, player)` warning.
    pub fn session_started(&self, player: u8, mac: &str, battery_mv: u16) -> u8 {
        let slot = dsu::slot_for_player(player);
        if let Some(server) = &self.server {
            server.set_player_slot(player, true, mac, battery_mv);
        }
        debug!(mac = %mac, player = player, slot = slot, "DSU session started");
        slot
    }

    /// Clear a slot when the session ends.
    pub fn session_ended(&self, slot: u8) {
        if let Some(server) = &self.server {
            server.drop_slot(slot);
        }
    }

    /// Build and publish one motion/input frame for `slot`.
    pub fn update(&self, slot: u8, report: &InputReport, cal: &CalibratedInput) {
        let Some(server) = &self.server else {
            return;
        };
        let timestamp_us = self.started.elapsed().as_micros() as u64;
        let frame = motion_input_from_report(report, cal, timestamp_us);
        server.update(slot, &frame);
    }

    /// Address the DSU server is bound to (port 0 resolves here).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.server
            .as_ref()
            .ok_or_else(|| io::Error::other("DSU motion server is not running"))?
            .local_addr()
    }
}

impl Drop for DsuBridge {
    fn drop(&mut self) {
        // The run loop polls this flag via the 500 ms socket read timeout.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// DSU `0..255` stick conversion from `bridge.py:_stick_to_dsu`
/// (line 40-42): `round(128 + value * 127)`, clamped to `0..=255`.
///
/// Python's `round` is ties-to-even; `f32::round_ties_even` preserves that
/// exact behaviour (`-0.5 -> 64`, not `65`).
pub fn stick_to_dsu(value: f32) -> u8 {
    let scaled = (128.0 + value * 127.0).round_ties_even();
    scaled.clamp(0.0, 255.0) as u8
}

/// Build one [`MotionInput`] from a report and its calibrated values.
///
/// IMU samples stay as raw `i16`-backed `f32`s (the DSU payload builder applies
/// the legacy scaling/signs). Sticks use [`stick_to_dsu`]; triggers pass
/// through as the already-calibrated `0..=255` values.
pub fn motion_input_from_report(
    report: &InputReport,
    cal: &CalibratedInput,
    timestamp_us: u64,
) -> MotionInput {
    MotionInput {
        accel: [
            f32::from(report.accel.0),
            f32::from(report.accel.1),
            f32::from(report.accel.2),
        ],
        gyro: [
            f32::from(report.gyro.0),
            f32::from(report.gyro.1),
            f32::from(report.gyro.2),
        ],
        sticks: [
            stick_to_dsu(cal.left.0),
            stick_to_dsu(cal.left.1),
            stick_to_dsu(cal.right.0),
            stick_to_dsu(cal.right.1),
        ],
        triggers: [cal.lt, cal.rt],
        buttons: report.buttons,
        timestamp_us,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsu::{battery_enum, info_payload, slot_for_player, STATE_CONNECTED};

    #[test]
    fn bridge_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DsuBridge>();
    }

    #[test]
    fn slot_clamps_players_five_to_eight() {
        assert_eq!(slot_for_player(1), 0);
        assert_eq!(slot_for_player(2), 1);
        assert_eq!(slot_for_player(3), 2);
        assert_eq!(slot_for_player(4), 3);
        assert_eq!(slot_for_player(5), 3);
        assert_eq!(slot_for_player(6), 3);
        assert_eq!(slot_for_player(7), 3);
        assert_eq!(slot_for_player(8), 3);
        assert_eq!(slot_for_player(0), 0);
    }

    #[test]
    fn stick_scaling_matches_legacy() {
        assert_eq!(stick_to_dsu(0.0), 128);
        assert_eq!(stick_to_dsu(1.0), 255);
        assert_eq!(stick_to_dsu(-1.0), 1);
        assert_eq!(stick_to_dsu(0.5), 192);
        // Python round is ties-to-even: 128 - 63.5 rounds to 64, not 65.
        assert_eq!(stick_to_dsu(-0.5), 64);
        assert_eq!(stick_to_dsu(0.25), 160);
        assert_eq!(stick_to_dsu(2.0), 255, "clamped high");
        assert_eq!(stick_to_dsu(-2.0), 0, "clamped low");
    }

    #[test]
    fn motion_input_from_report_uses_raw_i16() {
        let report = InputReport {
            timestamp: 123,
            buttons: crate::protocol::constants::button_mask::A
                | crate::protocol::constants::button_mask::ZL,
            left_stick_raw: (0, 0),
            right_stick_raw: (0, 0),
            battery_mv: 0,
            accel: (1, -2, 3),
            gyro: (-4, 5, -6),
            left_trigger_raw: 0,
            right_trigger_raw: 0,
        };
        let cal = CalibratedInput {
            left: (1.0, 0.0),
            right: (-0.5, 0.25),
            lt: 12,
            rt: 200,
        };

        let frame = motion_input_from_report(&report, &cal, 12345);

        assert_eq!(frame.accel, [1.0, -2.0, 3.0], "raw accel, no scaling/sign");
        assert_eq!(frame.gyro, [-4.0, 5.0, -6.0], "raw gyro, no scaling/sign");
        assert_eq!(frame.sticks, [255, 128, 64, 160]);
        assert_eq!(frame.triggers, [12, 200]);
        assert_eq!(frame.buttons, report.buttons);
        assert_eq!(frame.timestamp_us, 12345);
        assert!((crate::dsu::ACCEL_G_PER_LSB - 1.0 / 4096.0).abs() < f32::EPSILON);
    }

    #[test]
    fn battery_and_mac_reach_the_slot_header() {
        let slot = 2;
        let mac = "BC:89:A6:45:83:CC";
        let battery_mv = 3700;
        let payload = info_payload(slot, true, mac, battery_enum(battery_mv));

        assert_eq!(payload.len(), 12);
        assert_eq!(payload[0], slot);
        assert_eq!(payload[1], STATE_CONNECTED);
        assert_eq!(
            &payload[4..10],
            &[0xBC, 0x89, 0xA6, 0x45, 0x83, 0xCC],
            "MAC bytes are in the shared slot header"
        );
        assert_eq!(payload[10], 3, "3700 mV -> battery enum 0x03");
    }
}
