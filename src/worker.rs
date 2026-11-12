//! Per-pad session worker: one decoded input report -> virtual pad state,
//! battery bookkeeping and the idle-sleep decision.
//!
//! This module is deliberately free of sockets, Bluetooth, threads and dialing:
//! it is the piece the daemon (task 09B) assembles between the ATT reader
//! thread's bounded channel and the uinput node. All I/O goes through the
//! [`PadSink`] trait, so the whole state machine is testable without
//! `/dev/uinput`.
//!
//! Calibration is **not** redone here: the caller passes the
//! [`CalibratedInput`] produced by [`crate::controller::SwitchController::calibrated_input`]
//! (single source of truth, task 09D).

use std::io;
use std::time::{Duration, Instant};

use tracing::info;

use crate::controller::CalibratedInput;
use crate::protocol::report::InputReport;
use crate::status::battery_pct_from_mv;
use crate::uinput::gamepad::PadSink;

/// Static configuration for one pad session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerConfig {
    /// Controller MAC (upper-case, `AA:BB:CC:DD:EE:FF`).
    pub mac: String,
    /// Player slot 1..=8.
    pub player: u8,
    /// Idle-sleep timeout (analysis §5.13: 300 s on hardware).
    pub idle_timeout: Duration,
}

/// One pad's session state.
///
/// The idle timer follows analysis §7.3: `last_input_at` moves on every report
/// (used for "is this pad alive"), but `last_button_at` only moves when the
/// **button mask changes**. Stick motion, gyro and rumble therefore never keep
/// a pad awake, and holding a button does not postpone sleep either.
pub struct Worker<S: PadSink> {
    cfg: WorkerConfig,
    sink: S,
    /// Button mask of the previous report (`None` before the first report).
    last_buttons: Option<u32>,
    /// Last time the button mask changed. `None` = no report seen yet.
    last_button_at: Option<Instant>,
    /// When this worker was created; the idle baseline when no report has
    /// arrived yet (see [`Worker::should_sleep`]).
    created_at: Instant,
    /// Last report time (every report, regardless of content).
    last_input_at: Instant,
    /// Battery mV of the previous report, for change-only logging.
    last_battery_mv: u16,
    frames: u64,
}

impl<S: PadSink> Worker<S> {
    pub fn new(cfg: WorkerConfig, sink: S) -> Self {
        Self::new_at(cfg, sink, Instant::now())
    }

    /// Deterministic construction for tests: the idle baseline starts at `now`.
    fn new_at(cfg: WorkerConfig, sink: S, now: Instant) -> Self {
        Self {
            cfg,
            sink,
            last_buttons: None,
            last_button_at: None,
            created_at: now,
            last_input_at: now,
            last_battery_mv: 0,
            frames: 0,
        }
    }

    /// Handle one decoded input report.
    ///
    /// Every report is forwarded to the sink (a held stick keeps driving the
    /// pad); only the idle timer is selective. Battery is logged only when the
    /// value changes.
    pub fn on_report(
        &mut self,
        report: &InputReport,
        calibrated: &CalibratedInput,
        now: Instant,
    ) -> io::Result<()> {
        self.frames += 1;
        self.last_input_at = now;

        // Idle sleep keys off button-mask *changes*, not activity: a held
        // stick or button generates reports but must not postpone sleep.
        if self.last_buttons != Some(report.buttons) {
            self.last_buttons = Some(report.buttons);
            self.last_button_at = Some(now);
        }

        if report.battery_mv != self.last_battery_mv {
            self.last_battery_mv = report.battery_mv;
            info!(
                "{} battery: {} mV ({}%)",
                self.cfg.mac,
                report.battery_mv,
                battery_pct_from_mv(report.battery_mv)
                    .map(|pct| format!("{pct}"))
                    .unwrap_or_else(|| "?".to_string())
            );
        }

        self.sink.update(
            report.buttons,
            (
                calibrated.left.0,
                calibrated.left.1,
                calibrated.right.0,
                calibrated.right.1,
            ),
            (calibrated.lt, calibrated.rt),
        )
    }

    /// Should the session be put to sleep (`controller.sleep()` + drop link)?
    ///
    /// True once the button mask has not changed for `idle_timeout`,
    /// independent of stick activity.
    ///
    /// Before the first report there is no button timestamp; the documented
    /// fallback is "sleep `idle_timeout` after worker creation". That keeps a
    /// pad that never sends anything from being held awake forever, and is
    /// harmless in practice because a live pad reports ~every 5 ms.
    pub fn should_sleep(&self, now: Instant) -> bool {
        let since = match self.last_button_at {
            Some(at) => at,
            None => self.created_at,
        };
        now.saturating_duration_since(since) >= self.cfg.idle_timeout
    }

    /// Release every key and centre every axis. The uinput node stays alive;
    /// closing it is the daemon owner's job (deviation 6 / analysis §5.9).
    pub fn teardown(&mut self) -> io::Result<()> {
        self.sink.release_all()
    }

    /// Select the player slot, forwarding to the sink.
    pub fn set_player(&mut self, player: u8) -> io::Result<()> {
        self.cfg.player = player;
        self.sink.set_player(player)
    }

    /// Reports handled so far (including ones whose sink update failed).
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Battery mV of the latest report (`0` before the first one).
    pub fn battery_mv(&self) -> u16 {
        self.last_battery_mv
    }

    pub fn mac(&self) -> &str {
        &self.cfg.mac
    }

    pub fn player(&self) -> u8 {
        self.cfg.player
    }

    /// Time of the most recent report (worker creation time before the first).
    pub fn last_input_at(&self) -> Instant {
        self.last_input_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::constants::button_mask;

    /// Records what the worker asked for; `release_all` neutralises the state
    /// so tests can assert the teardown contract rather than just a call count.
    #[derive(Debug, Default)]
    struct FakeSink {
        updates: usize,
        buttons: u32,
        sticks: (f32, f32, f32, f32),
        triggers: (u8, u8),
        releases: usize,
        players: Vec<u8>,
    }

    impl PadSink for FakeSink {
        fn update(
            &mut self,
            buttons: u32,
            sticks: (f32, f32, f32, f32),
            triggers: (u8, u8),
        ) -> io::Result<()> {
            self.updates += 1;
            self.buttons = buttons;
            self.sticks = sticks;
            self.triggers = triggers;
            Ok(())
        }

        fn release_all(&mut self) -> io::Result<()> {
            self.releases += 1;
            self.buttons = 0;
            self.sticks = (0.0, 0.0, 0.0, 0.0);
            self.triggers = (0, 0);
            Ok(())
        }

        fn set_player(&mut self, player: u8) -> io::Result<()> {
            self.players.push(player);
            Ok(())
        }
    }

    const TIMEOUT: Duration = Duration::from_secs(300);

    fn cfg() -> WorkerConfig {
        WorkerConfig {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            player: 1,
            idle_timeout: TIMEOUT,
        }
    }

    fn report(buttons: u32) -> InputReport {
        InputReport {
            timestamp: 0,
            buttons,
            left_stick_raw: (2048, 2048),
            right_stick_raw: (2048, 2048),
            battery_mv: 4000,
            accel: (0, 0, 0),
            gyro: (0, 0, 0),
            left_trigger_raw: 0,
            right_trigger_raw: 0,
        }
    }

    fn cal() -> CalibratedInput {
        CalibratedInput {
            left: (0.5, -0.25),
            right: (-0.75, 1.0),
            lt: 12,
            rt: 200,
        }
    }

    #[test]
    fn worker_forwards_buttons_and_sticks() {
        let t0 = Instant::now();
        let mut worker = Worker::new_at(cfg(), FakeSink::default(), t0);
        let buttons = button_mask::A | button_mask::ZL | button_mask::C_BTN;

        worker
            .on_report(&report(buttons), &cal(), t0)
            .expect("sink accepts the report");

        assert_eq!(worker.sink.updates, 1);
        assert_eq!(worker.sink.buttons, buttons);
        assert_eq!(worker.sink.sticks, (0.5, -0.25, -0.75, 1.0));
        assert_eq!(worker.sink.triggers, (12, 200));
        assert_eq!(worker.frames(), 1);
    }

    #[test]
    fn idle_timer_resets_only_on_button_mask_change() {
        let t0 = Instant::now();
        let mut worker = Worker::new_at(cfg(), FakeSink::default(), t0);

        worker.on_report(&report(0), &cal(), t0).unwrap();
        // Stick-only movement keeps forwarding but must not reset the timer.
        worker
            .on_report(&report(0), &cal(), t0 + Duration::from_secs(200))
            .unwrap();
        assert!(!worker.should_sleep(t0 + TIMEOUT - Duration::from_millis(1)));
        assert!(worker.should_sleep(t0 + TIMEOUT));

        // A real mask change (here: at +280 s) restarts the idle window.
        let pressed = t0 + Duration::from_secs(280);
        worker
            .on_report(&report(button_mask::A), &cal(), pressed)
            .unwrap();
        assert!(!worker.should_sleep(pressed + TIMEOUT - Duration::from_millis(1)));
        assert!(worker.should_sleep(pressed + TIMEOUT));

        // Releasing the button is another mask change.
        let released = pressed + Duration::from_secs(290);
        worker.on_report(&report(0), &cal(), released).unwrap();
        assert!(!worker.should_sleep(released + TIMEOUT - Duration::from_millis(1)));
        assert!(worker.should_sleep(released + TIMEOUT));
    }

    #[test]
    fn idle_timer_ignores_stick_activity() {
        let t0 = Instant::now();
        let mut worker = Worker::new_at(cfg(), FakeSink::default(), t0);

        // 60 s of reports with an unchanged (empty) button mask, 16 ms apart.
        let mut now = t0;
        for _ in 0..(60_000 / 16) {
            worker.on_report(&report(0), &cal(), now).unwrap();
            now += Duration::from_millis(16);
        }
        assert_eq!(worker.frames(), 60_000 / 16);

        assert!(worker.should_sleep(t0 + TIMEOUT));
        assert!(!worker.should_sleep(t0 + TIMEOUT - Duration::from_millis(1)));
    }

    #[test]
    fn worker_does_not_sleep_before_idle_timeout() {
        let t0 = Instant::now();
        let mut worker = Worker::new_at(cfg(), FakeSink::default(), t0);
        worker
            .on_report(&report(button_mask::B), &cal(), t0)
            .unwrap();

        assert!(!worker.should_sleep(t0));
        assert!(!worker.should_sleep(t0 + Duration::from_secs(299)));
        assert!(worker.should_sleep(t0 + TIMEOUT));

        // A worker that has never seen a report sleeps one timeout after it was
        // created (documented fallback).
        let idle = Worker::new_at(cfg(), FakeSink::default(), t0);
        assert!(!idle.should_sleep(t0 + TIMEOUT - Duration::from_millis(1)));
        assert!(idle.should_sleep(t0 + TIMEOUT));
    }

    #[test]
    fn teardown_centres_axes_and_releases_keys() {
        let t0 = Instant::now();
        let mut worker = Worker::new_at(cfg(), FakeSink::default(), t0);

        worker
            .on_report(&report(button_mask::A | button_mask::ZR), &cal(), t0)
            .unwrap();
        assert_ne!(worker.sink.buttons, 0);
        assert_ne!(worker.sink.sticks, (0.0, 0.0, 0.0, 0.0));

        worker.teardown().expect("release_all succeeds");

        assert_eq!(worker.sink.releases, 1);
        assert_eq!(worker.sink.buttons, 0, "every key released");
        assert_eq!(worker.sink.sticks, (0.0, 0.0, 0.0, 0.0), "sticks centred");
        assert_eq!(worker.sink.triggers, (0, 0), "triggers zeroed");
        // The worker itself is untouched by teardown: the node stays alive and
        // the daemon decides whether to reuse this worker.
        assert_eq!(worker.frames(), 1);
    }

    #[test]
    fn battery_change_is_observable() {
        let t0 = Instant::now();
        let mut worker = Worker::new_at(cfg(), FakeSink::default(), t0);
        assert_eq!(worker.battery_mv(), 0);

        let mut first = report(0);
        first.battery_mv = 4000;
        worker.on_report(&first, &cal(), t0).unwrap();
        assert_eq!(worker.battery_mv(), 4000);

        // Same battery: still forwarded, value unchanged.
        worker
            .on_report(&first, &cal(), t0 + Duration::from_millis(16))
            .unwrap();
        assert_eq!(worker.battery_mv(), 4000);

        let mut low = report(0);
        low.battery_mv = 3350;
        worker
            .on_report(&low, &cal(), t0 + Duration::from_millis(32))
            .unwrap();
        assert_eq!(worker.battery_mv(), 3350);
        assert_eq!(worker.sink.updates, 3);
    }

    #[test]
    fn frames_counts_every_report() {
        let t0 = Instant::now();
        let mut worker = Worker::new_at(cfg(), FakeSink::default(), t0);
        assert_eq!(worker.frames(), 0);

        for i in 0..5 {
            // Identical content on purpose: forwarded, counted.
            worker
                .on_report(&report(0), &cal(), t0 + Duration::from_millis(16 * i))
                .unwrap();
            assert_eq!(worker.frames(), i + 1);
        }
        assert_eq!(worker.sink.updates, 5);
    }

    #[test]
    fn set_player_reaches_the_sink() {
        let t0 = Instant::now();
        let mut worker = Worker::new_at(cfg(), FakeSink::default(), t0);
        worker.set_player(3).expect("sink accepts the slot");
        assert_eq!(worker.player(), 3);
        assert_eq!(worker.sink.players, vec![3]);
    }
}
