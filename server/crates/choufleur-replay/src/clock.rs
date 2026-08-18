//! Pacing a replay against the wall clock.
//!
//! Batch mode answers "how fast can this machine go"; realtime mode answers the
//! question the PRD actually cares about — "does the pipeline keep up, and by how
//! much does a warning arrive late". Those are different numbers and conflating
//! them hides the interesting failure: a run that is faster than real time *on
//! average* can still be seconds behind at the moment three actors speak at once.
//!
//! The clock also records **backlog**: how far behind the audio timeline the
//! processing has fallen at each point. A run that never falls behind and a run
//! that falls two seconds behind twice per act both look fine in an average.

use std::time::{Duration, Instant};

/// Maps audio-timeline seconds onto wall-clock instants.
pub struct VirtualClock {
    started: Instant,
    /// `None` in batch mode: audio time and wall time are unrelated and no
    /// sleeping happens.
    realtime: bool,
    max_backlog_s: f64,
    backlog_sum: f64,
    backlog_samples: u64,
    fell_behind: u64,
    /// Backlog beyond which a block counts as "fell behind" rather than jitter.
    tolerance_s: f64,
}

impl VirtualClock {
    pub fn batch() -> Self {
        VirtualClock::new(false)
    }

    pub fn realtime() -> Self {
        VirtualClock::new(true)
    }

    fn new(realtime: bool) -> Self {
        VirtualClock {
            started: Instant::now(),
            realtime,
            max_backlog_s: 0.0,
            backlog_sum: 0.0,
            backlog_samples: 0,
            fell_behind: 0,
            tolerance_s: 0.25,
        }
    }

    pub fn is_realtime(&self) -> bool {
        self.realtime
    }

    /// Wall-clock seconds since the run began.
    pub fn elapsed_s(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// Declare that some wall-clock time did not happen.
    ///
    /// A paused run does not advance audio time, so every second of the pause becomes a
    /// second of backlog — and `wait_until` only ever sleeps when it is *ahead*, never
    /// when behind. The run therefore raced to make the pause up: eight seconds of
    /// standing still cost sixty-eight seconds of the recording, consumed as fast as the
    /// machine could manage. Moving the anchor forward is what makes a pause a pause
    /// rather than a debt.
    pub fn skip(&mut self, elapsed: std::time::Duration) {
        self.started += elapsed;
    }

    /// Block until wall time reaches `audio_t`, recording how late we already are.
    ///
    /// In batch mode this only records; it never sleeps. Note that being *late*
    /// is the interesting case and is never "caught up" by sleeping less — the
    /// deadline for audio time t is always `start + t`, so a run that falls
    /// behind stays behind unless it processes faster than real time.
    pub fn wait_until(&mut self, audio_t: f64) {
        let elapsed = self.elapsed_s();
        let backlog = elapsed - audio_t;
        if backlog > 0.0 {
            self.max_backlog_s = self.max_backlog_s.max(backlog);
            if backlog > self.tolerance_s {
                self.fell_behind += 1;
            }
        }
        self.backlog_sum += backlog.max(0.0);
        self.backlog_samples += 1;

        if self.realtime && backlog < 0.0 {
            std::thread::sleep(Duration::from_secs_f64(-backlog));
        }
    }

    /// Wall-clock milliseconds between a segment's audio ending and *now*.
    ///
    /// This is the number the PRD's ≤1.5 s / ≤3 s budget is measured against, and
    /// it deliberately includes time the segment spent queued behind other
    /// channels: an operator does not care why their warning was late.
    pub fn latency_ms_for(&self, segment_end_t: f64) -> u64 {
        let late = self.elapsed_s() - segment_end_t;
        (late.max(0.0) * 1000.0).round() as u64
    }

    pub fn stats(&self, audio_duration_s: f64) -> ClockStats {
        let wall = self.elapsed_s();
        ClockStats {
            audio_s: audio_duration_s,
            wall_s: wall,
            realtime_factor: if wall > 0.0 {
                audio_duration_s / wall
            } else {
                f64::INFINITY
            },
            max_backlog_s: self.max_backlog_s,
            mean_backlog_s: if self.backlog_samples > 0 {
                self.backlog_sum / self.backlog_samples as f64
            } else {
                0.0
            },
            fell_behind: self.fell_behind,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ClockStats {
    pub audio_s: f64,
    pub wall_s: f64,
    /// Audio seconds processed per wall second. Above 1.0 is faster than real
    /// time; the devplan's compute criterion is this staying above 1.0 with 3–4
    /// channels active.
    pub realtime_factor: f64,
    pub max_backlog_s: f64,
    pub mean_backlog_s: f64,
    /// Blocks whose processing started more than the tolerance late.
    pub fell_behind: u64,
}

impl std::fmt::Display for ClockStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.1} s of audio in {:.1} s wall ({:.2}× real time)",
            self.audio_s, self.wall_s, self.realtime_factor
        )?;
        if self.fell_behind > 0 {
            write!(
                f,
                "; fell behind {} time(s), worst {:.2} s",
                self.fell_behind, self.max_backlog_s
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_mode_never_sleeps() {
        let mut clock = VirtualClock::batch();
        let before = Instant::now();
        // Ten minutes of audio time, consumed instantly.
        for i in 0..600 {
            clock.wait_until(i as f64);
        }
        assert!(
            before.elapsed() < Duration::from_millis(500),
            "batch mode slept"
        );
        assert!(!clock.is_realtime());
    }

    #[test]
    fn realtime_mode_paces_to_the_audio_timeline() {
        let mut clock = VirtualClock::realtime();
        let before = Instant::now();
        clock.wait_until(0.15);
        let waited = before.elapsed();
        assert!(
            waited >= Duration::from_millis(140),
            "did not wait: {waited:?}"
        );
        assert!(
            waited < Duration::from_millis(400),
            "waited too long: {waited:?}"
        );
    }

    #[test]
    fn backlog_is_recorded_rather_than_smoothed_away() {
        let mut clock = VirtualClock::batch();
        std::thread::sleep(Duration::from_millis(60));
        // Audio time 0 while 60 ms of wall time has passed: 60 ms behind.
        clock.wait_until(0.0);
        let stats = clock.stats(1.0);
        assert!(stats.max_backlog_s >= 0.05, "{stats:?}");
        // Under the 250 ms tolerance, so it is jitter, not falling behind.
        assert_eq!(stats.fell_behind, 0);
    }

    #[test]
    fn sustained_lateness_is_counted() {
        let mut clock = VirtualClock::batch();
        std::thread::sleep(Duration::from_millis(300));
        clock.wait_until(0.0);
        assert_eq!(clock.stats(1.0).fell_behind, 1);
    }

    #[test]
    fn latency_is_measured_from_the_segments_audio_end() {
        let mut clock = VirtualClock::batch();
        std::thread::sleep(Duration::from_millis(120));
        // A segment whose audio ended at t=0 is now ~120 ms old.
        let late = clock.latency_ms_for(0.0);
        assert!((100..400).contains(&late), "latency {late} ms");
        // A segment whose audio ends in the future is not negative-latency.
        assert_eq!(clock.latency_ms_for(999.0), 0);
        clock.wait_until(0.0);
    }

    #[test]
    fn the_realtime_factor_says_whether_this_machine_keeps_up() {
        let clock = VirtualClock::batch();
        std::thread::sleep(Duration::from_millis(100));
        // Ten seconds of audio processed in ~0.1 s wall.
        let stats = clock.stats(10.0);
        assert!(stats.realtime_factor > 20.0, "{stats:?}");
        assert!(format!("{stats}").contains("× real time"));
    }

#[cfg(test)]
mod pause_tests {
    use super::*;

    #[test]
    fn skipping_time_removes_the_backlog_a_pause_creates() {
        let mut c = VirtualClock::realtime();
        std::thread::sleep(std::time::Duration::from_millis(120));
        // Audio time has not moved, so the run is 120 ms behind — and `wait_until` only
        // sleeps when it is *ahead*, so that backlog would be made up by racing through
        // the recording.
        let behind = c.elapsed_s();
        assert!(behind >= 0.1, "{behind}");
        // Skip what is actually owed, not the 120 ms that were asked for. `sleep`
        // promises a floor and not a duration: on a loaded machine it overshoots, and a
        // CI runner slept 181 ms — leaving 61 ms of backlog that this test read as a
        // failure to skip rather than as the scheduler being busy. The property worth
        // asserting is that skipping the backlog clears it, whatever the backlog is.
        c.skip(std::time::Duration::from_secs_f64(behind));
        assert!(c.elapsed_s() < 0.05, "the pause is no longer owed: {}", c.elapsed_s());
    }
}
}
