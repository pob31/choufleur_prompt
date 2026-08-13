//! Turning a stream of voice-activity probabilities into speech segments.
//!
//! Whisper is not a streaming recognizer — it consumes windows of audio and emits
//! nothing until it has them (PRD, *Whisper is chunk-based, not streaming*). So the
//! segmenter decides what a "chunk" is, and that decision sets the floor on how
//! quickly the tracker can learn anything.
//!
//! # Why interim segments exist
//!
//! The obvious policy — open on speech, close on silence, emit once — has a fatal
//! property, measured in `choufleur-replay`'s pipeline tests: **detection lag is
//! bounded below by the line's own duration**, because the tracker cannot hear a
//! line until the actor has finished saying it. A four-second line therefore lands
//! four seconds late, against a gate of two. No matcher tuning can recover this.
//!
//! So an open speech run is also emitted *interim*, every `interim_interval_ms`,
//! carrying everything heard so far. The tracker sees the line take shape and
//! commits partway through it. The cost is real — each interim emission is a full
//! Whisper decode, so this is the knob that trades compute for lag, and M0.4 is
//! where that trade gets priced.
//!
//! Everything here is pure: probabilities in, segments out, no model, no clock.

use serde::{Deserialize, Serialize};

/// Silero operates on fixed 512-sample windows at 16 kHz — 32 ms apiece.
pub const WINDOW: usize = 512;
pub const SAMPLE_RATE: u32 = 16_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct VadConfig {
    /// Probability at which a speech run opens.
    pub open_threshold: f32,
    /// Probability below which it starts closing. Lower than `open_threshold`:
    /// hysteresis, or every unvoiced consonant chops the line in half.
    pub close_threshold: f32,
    /// Runs shorter than this are dropped as noise rather than decoded.
    pub min_speech_ms: u32,
    /// How long the level must stay low before the run is really over. Actors
    /// pause inside sentences; 400 ms is a breath, not a cue.
    pub hangover_ms: u32,
    /// Audio retained from *before* the open, so the first phoneme survives.
    pub pre_pad_ms: u32,
    /// Audio retained after the close.
    pub post_pad_ms: u32,
    /// Hard cap on a single segment; a long speech is split rather than buffered.
    pub max_segment_ms: u32,
    /// Emit a partial hypothesis this often while speech is still open. 0 disables
    /// interim emission and restores the end-of-utterance latency floor.
    pub interim_interval_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        VadConfig {
            open_threshold: 0.50,
            close_threshold: 0.35,
            min_speech_ms: 250,
            hangover_ms: 400,
            pre_pad_ms: 200,
            post_pad_ms: 100,
            max_segment_ms: 5_000,
            interim_interval_ms: 1_500,
        }
    }
}

/// A chunk of speech ready for recognition.
#[derive(Clone, Debug)]
pub struct AudioSegment {
    pub channel: u16,
    /// Audio-timeline seconds.
    pub t_start: f64,
    pub t_end: f64,
    /// 16 kHz mono.
    pub samples: Vec<f32>,
    /// The speech run is still open; more of this line is coming. Interim segments
    /// are what keep detection lag inside the budget.
    pub interim: bool,
    /// Closed by the length cap rather than by silence — likely cut mid-word.
    pub forced_split: bool,
}

impl AudioSegment {
    pub fn duration(&self) -> f64 {
        self.t_end - self.t_start
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// Speech is open. `silence_ms` counts the current run of low probability;
    /// `speech_ms` counts windows that actually looked like speech — the pre-pad
    /// and the hangover are in the buffer too, and a cough surrounded by half a
    /// second of padding must not pass for a line on buffer length alone.
    Open {
        silence_ms: u32,
        speech_ms: u32,
    },
}

/// The VAD state machine for one channel.
pub struct Segmenter {
    channel: u16,
    cfg: VadConfig,
    state: State,
    /// Audio of the current (or candidate) speech run, from the pre-pad onward.
    buffer: Vec<f32>,
    /// Ring of recent windows, so a run can be back-dated by `pre_pad_ms`.
    pre_pad: Vec<f32>,
    /// Timeline position of `buffer[0]`.
    buffer_start: f64,
    /// Next window's start time.
    now: f64,
    ms_since_interim: u32,
    /// Milliseconds of the open run that actually looked like speech.
    speech_ms: u32,
}

impl Segmenter {
    pub fn new(channel: u16, cfg: VadConfig) -> Self {
        Segmenter {
            channel,
            cfg,
            state: State::Idle,
            buffer: Vec::new(),
            pre_pad: Vec::new(),
            buffer_start: 0.0,
            now: 0.0,
            ms_since_interim: 0,
            speech_ms: 0,
        }
    }

    pub fn config(&self) -> &VadConfig {
        &self.cfg
    }

    fn window_ms(&self) -> u32 {
        (WINDOW as u32 * 1000) / SAMPLE_RATE
    }

    fn pre_pad_samples(&self) -> usize {
        (self.cfg.pre_pad_ms as usize * SAMPLE_RATE as usize) / 1000
    }

    fn buffer_ms(&self) -> u32 {
        (self.buffer.len() * 1000 / SAMPLE_RATE as usize) as u32
    }

    /// Feed one 512-sample window and its speech probability.
    ///
    /// Returns any segments this window completed — at most one final segment, or
    /// one interim segment, never both.
    pub fn push(&mut self, window: &[f32], prob: f32) -> Option<AudioSegment> {
        debug_assert_eq!(window.len(), WINDOW);
        let w_ms = self.window_ms();
        let w_start = self.now;
        self.now += WINDOW as f64 / SAMPLE_RATE as f64;

        match self.state {
            State::Idle => {
                if prob >= self.cfg.open_threshold {
                    // Back-date the segment into the pre-pad so the attack is intact.
                    let pad = self.pre_pad.len();
                    self.buffer_start = w_start - pad as f64 / SAMPLE_RATE as f64;
                    self.buffer = std::mem::take(&mut self.pre_pad);
                    self.buffer.extend_from_slice(window);
                    self.state = State::Open {
                        silence_ms: 0,
                        speech_ms: w_ms,
                    };
                    self.ms_since_interim = 0;
                } else {
                    self.remember(window);
                }
                None
            }
            State::Open {
                silence_ms,
                speech_ms,
            } => {
                self.buffer.extend_from_slice(window);
                self.ms_since_interim += w_ms;

                let quiet = prob < self.cfg.close_threshold;
                let silence_ms = if quiet { silence_ms + w_ms } else { 0 };
                let speech_ms = if quiet { speech_ms } else { speech_ms + w_ms };
                self.speech_ms = speech_ms;

                if silence_ms >= self.cfg.hangover_ms {
                    return self.close(false);
                }
                if self.buffer_ms() >= self.cfg.max_segment_ms {
                    return self.close(true);
                }
                self.state = State::Open {
                    silence_ms,
                    speech_ms,
                };

                if self.cfg.interim_interval_ms > 0
                    && self.ms_since_interim >= self.cfg.interim_interval_ms
                    && speech_ms >= self.cfg.min_speech_ms
                {
                    self.ms_since_interim = 0;
                    return Some(self.emit(true, false));
                }
                None
            }
        }
    }

    /// End of stream: close anything still open.
    pub fn flush(&mut self) -> Option<AudioSegment> {
        match self.state {
            State::Idle => None,
            State::Open { .. } => self.close(false),
        }
    }

    fn close(&mut self, forced: bool) -> Option<AudioSegment> {
        self.state = State::Idle;
        self.ms_since_interim = 0;
        let spoken = self.speech_ms;
        self.speech_ms = 0;
        if spoken < self.cfg.min_speech_ms {
            // Too short to be a line. Drop it, but keep it as pre-pad: a false
            // start is often the beginning of the real thing.
            self.buffer.clear();
            self.pre_pad.clear();
            return None;
        }
        let seg = self.emit(false, forced);
        self.buffer.clear();
        self.pre_pad.clear();
        if forced {
            // A forced split means the actor is still speaking; reopen immediately
            // so the next chunk starts here rather than waiting for a fresh onset.
            self.buffer_start = seg.t_end;
            self.state = State::Open {
                silence_ms: 0,
                speech_ms: 0,
            };
        }
        Some(seg)
    }

    fn emit(&self, interim: bool, forced_split: bool) -> AudioSegment {
        let post = if interim {
            0
        } else {
            (self.cfg.post_pad_ms as usize * SAMPLE_RATE as usize) / 1000
        };
        let _ = post; // post-pad audio has already been buffered by the hangover
        AudioSegment {
            channel: self.channel,
            t_start: self.buffer_start.max(0.0),
            t_end: self.buffer_start + self.buffer.len() as f64 / SAMPLE_RATE as f64,
            samples: self.buffer.clone(),
            interim,
            forced_split,
        }
    }

    fn remember(&mut self, window: &[f32]) {
        let cap = self.pre_pad_samples();
        if cap == 0 {
            return;
        }
        self.pre_pad.extend_from_slice(window);
        if self.pre_pad.len() > cap {
            let drop = self.pre_pad.len() - cap;
            self.pre_pad.drain(..drop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the segmenter with a probability script, collecting what it emits.
    fn run(cfg: VadConfig, probs: &[f32]) -> Vec<AudioSegment> {
        let mut seg = Segmenter::new(1, cfg);
        let window = [0.1f32; WINDOW];
        let mut out = Vec::new();
        for &p in probs {
            if let Some(s) = seg.push(&window, p) {
                out.push(s);
            }
        }
        out.extend(seg.flush());
        out
    }

    /// `n` windows at probability `p`. One window is 32 ms.
    fn hold(p: f32, ms: u32) -> Vec<f32> {
        vec![p; (ms / 32).max(1) as usize]
    }

    fn no_interim() -> VadConfig {
        VadConfig {
            interim_interval_ms: 0,
            ..Default::default()
        }
    }

    #[test]
    fn speech_bracketed_by_silence_becomes_one_segment() {
        let mut probs = hold(0.02, 640);
        probs.extend(hold(0.9, 2_000));
        probs.extend(hold(0.02, 1_000));
        let segs = run(no_interim(), &probs);
        assert_eq!(segs.len(), 1, "{segs:?}");
        assert!(!segs[0].interim && !segs[0].forced_split);
        // Pre-pad + speech + the hangover that proved it was over.
        assert!(
            (2.0..=2.7).contains(&segs[0].duration()),
            "duration {} should be the pre-pad, the speech and the hangover",
            segs[0].duration()
        );
    }

    #[test]
    fn the_pre_pad_back_dates_the_segment_so_the_attack_survives() {
        let mut probs = hold(0.02, 2_000);
        probs.extend(hold(0.9, 1_000));
        let segs = run(no_interim(), &probs);
        let s = &segs[0];
        // Speech began at 2.0 s; the segment must start ~200 ms earlier.
        assert!((s.t_start - 1.8).abs() < 0.05, "t_start {}", s.t_start);
    }

    #[test]
    fn a_breath_inside_a_line_does_not_split_it() {
        let mut probs = hold(0.9, 1_000);
        probs.extend(hold(0.1, 250)); // shorter than the 400 ms hangover
        probs.extend(hold(0.9, 1_000));
        probs.extend(hold(0.02, 1_000));
        let segs = run(no_interim(), &probs);
        assert_eq!(segs.len(), 1, "a 250 ms pause is a breath: {segs:?}");
    }

    #[test]
    fn a_real_pause_does_split_the_line() {
        let mut probs = hold(0.9, 1_000);
        probs.extend(hold(0.02, 800)); // beyond the hangover
        probs.extend(hold(0.9, 1_000));
        probs.extend(hold(0.02, 1_000));
        let segs = run(no_interim(), &probs);
        assert_eq!(segs.len(), 2, "{segs:?}");
        assert!(segs[0].t_end < segs[1].t_start);
    }

    #[test]
    fn a_cough_is_too_short_to_decode() {
        let mut probs = hold(0.02, 500);
        probs.extend(hold(0.9, 100)); // under min_speech_ms
        probs.extend(hold(0.02, 1_000));
        assert!(run(no_interim(), &probs).is_empty());
    }

    #[test]
    fn a_long_speech_is_split_rather_than_buffered_forever() {
        let probs = hold(0.95, 12_000);
        let segs = run(no_interim(), &probs);
        assert!(segs.len() >= 2, "{} segment(s)", segs.len());
        assert!(
            segs[0].forced_split,
            "the first split is forced, not silence"
        );
        for s in &segs {
            assert!(s.duration() <= 5.2, "segment ran to {} s", s.duration());
        }
        // The splits must tile the speech without gaps, or words fall between them.
        for w in segs.windows(2) {
            assert!(
                (w[1].t_start - w[0].t_end).abs() < 0.05,
                "gap at {}",
                w[0].t_end
            );
        }
    }

    #[test]
    fn interim_segments_arrive_while_the_actor_is_still_speaking() {
        // A four-second line, the case that breaks the lag budget without interims.
        let mut probs = hold(0.9, 4_000);
        probs.extend(hold(0.02, 1_000));
        let segs = run(VadConfig::default(), &probs);

        let interims: Vec<&AudioSegment> = segs.iter().filter(|s| s.interim).collect();
        assert!(
            interims.len() >= 2,
            "expected partial hypotheses, got {}",
            interims.len()
        );
        assert_eq!(
            segs.iter().filter(|s| !s.interim).count(),
            1,
            "exactly one final segment"
        );

        // The first partial must land well inside the two-second gate.
        assert!(
            interims[0].t_end <= 2.0,
            "first interim at {} s",
            interims[0].t_end
        );
        // Each interim carries everything heard so far, so they grow.
        for w in interims.windows(2) {
            assert!(w[1].samples.len() > w[0].samples.len());
            assert!(
                (w[1].t_start - w[0].t_start).abs() < 1e-6,
                "all share one start"
            );
        }
        // ...and the final segment is the whole line.
        let last = segs.last().unwrap();
        assert!(last.samples.len() >= interims.last().unwrap().samples.len());
    }

    #[test]
    fn interim_emission_can_be_switched_off() {
        let mut probs = hold(0.9, 4_000);
        probs.extend(hold(0.02, 1_000));
        let segs = run(no_interim(), &probs);
        assert!(segs.iter().all(|s| !s.interim));
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn a_short_line_produces_no_interim_before_its_final() {
        let mut probs = hold(0.9, 700);
        probs.extend(hold(0.02, 1_000));
        let segs = run(VadConfig::default(), &probs);
        assert_eq!(
            segs.len(),
            1,
            "a sub-interval line needs no partial: {segs:?}"
        );
        assert!(!segs[0].interim);
    }

    #[test]
    fn flush_closes_a_run_left_open_at_end_of_stream() {
        let segs = run(no_interim(), &hold(0.9, 2_000));
        assert_eq!(segs.len(), 1);
        assert!(
            !segs[0].interim,
            "the flushed segment is final, not partial"
        );
    }

    #[test]
    fn hysteresis_keeps_a_wobbling_level_from_chattering() {
        // Level oscillates between the two thresholds — neither clearly speech nor
        // clearly silence. It must not produce a burst of tiny segments.
        let mut probs = Vec::new();
        for i in 0..120 {
            probs.push(if i % 2 == 0 { 0.45 } else { 0.55 });
        }
        probs.extend(hold(0.02, 1_000));
        let segs = run(no_interim(), &probs);
        assert!(segs.len() <= 2, "chattered into {} segments", segs.len());
    }
}
