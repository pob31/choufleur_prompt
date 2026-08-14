//! One channel's front end: resample → VAD → segment.
//!
//! Blocks of 48 kHz audio go in, closed speech segments come out. The VAD model is
//! shared across channels and passed in per call; everything that is genuinely
//! per-channel — the resampler's filter memory, Silero's recurrent state, the
//! segmenter's position in its state machine — lives here.
//!
//! Timestamps are the subtle part. They are derived from the 16 kHz stream, which
//! lags the 48 kHz input by the resampler's constant group delay, so the delay is
//! subtracted once here. Get this wrong and every segment boundary is uniformly
//! late — which looks exactly like the tracker being slow, and would be tuned
//! against for a long time before anyone found it.

use crate::agc::{AgcConfig, AutoGain};
use crate::resample::Resampler48to16;
use crate::segmenter::{AudioSegment, Segmenter, VadConfig, WINDOW};
use crate::vad::{SileroChannelState, SileroSession};
use crate::AsrError;

pub struct ChannelFrontend {
    pub channel: u16,
    resampler: Resampler48to16,
    /// Runs between resampling and detection, so both the voice detector and the
    /// recogniser see audio in the range they were trained on. Theatre mics are
    /// gained for the loudest moment of the night, which leaves ordinary dialogue
    /// forty decibels down; see `agc`.
    agc: AutoGain,
    vad_state: SileroChannelState,
    segmenter: Segmenter,
    /// 16 kHz samples not yet consumed as a whole VAD window.
    pending: Vec<f32>,
    /// Subtracted from every emitted timestamp; constant for the run.
    delay_s: f64,
}

impl ChannelFrontend {
    pub fn new(channel: u16, cfg: VadConfig) -> Result<Self, AsrError> {
        Self::with_agc(channel, cfg, AgcConfig::default())
    }

    pub fn with_agc(channel: u16, cfg: VadConfig, agc: AgcConfig) -> Result<Self, AsrError> {
        let resampler = Resampler48to16::new()?;
        let delay_s = resampler.output_delay_seconds();
        Ok(ChannelFrontend {
            channel,
            resampler,
            agc: AutoGain::new(crate::segmenter::SAMPLE_RATE, agc),
            vad_state: SileroChannelState::new(),
            segmenter: Segmenter::new(channel, cfg),
            pending: Vec::with_capacity(WINDOW * 4),
            delay_s,
        })
    }

    pub fn resampler_delay_seconds(&self) -> f64 {
        self.delay_s
    }

    /// Gain currently applied to this channel, in dB. A channel pinned at the
    /// ceiling is reporting a problem with the desk, not with the actor.
    /// The level the gain stage believes speech on this channel sits at.
    ///
    /// Worth surfacing next to the gain: together they say whether a channel is quiet
    /// because the actor is far away or because the desk is set for a shout later.
    pub fn speech_dbfs(&self) -> f32 {
        self.agc.speech_dbfs()
    }

    pub fn gain_db(&self) -> f32 {
        self.agc.gain_db()
    }

    /// Feed one block of 48 kHz mono audio; return the segments it completed.
    pub fn push_block(
        &mut self,
        vad: &mut SileroSession,
        block_48k: &[f32],
        out: &mut Vec<AudioSegment>,
    ) -> Result<(), AsrError> {
        self.resampler.process(block_48k, &mut self.pending)?;
        self.drain_windows(vad, out)
    }

    /// End of stream: flush the resampler, run what is left, close any open run.
    pub fn finish(
        &mut self,
        vad: &mut SileroSession,
        out: &mut Vec<AudioSegment>,
    ) -> Result<(), AsrError> {
        self.resampler.flush(&mut self.pending)?;
        self.drain_windows(vad, out)?;
        // Whatever is left is shorter than a window; pad it rather than dropping
        // it, so a line ending at the very end of the recording is not truncated.
        if !self.pending.is_empty() {
            let mut window = [0.0f32; WINDOW];
            let n = self.pending.len().min(WINDOW);
            window[..n].copy_from_slice(&self.pending[..n]);
            self.pending.clear();
            let prob = vad.process_window(&mut self.vad_state, &window)?;
            if let Some(seg) = self.segmenter.push(&window, prob) {
                out.push(self.shift(seg));
            }
        }
        if let Some(seg) = self.segmenter.flush() {
            out.push(self.shift(seg));
        }
        Ok(())
    }

    fn drain_windows(
        &mut self,
        vad: &mut SileroSession,
        out: &mut Vec<AudioSegment>,
    ) -> Result<(), AsrError> {
        let mut consumed = 0usize;
        while consumed + WINDOW <= self.pending.len() {
            let mut window = [0.0f32; WINDOW];
            window.copy_from_slice(&self.pending[consumed..consumed + WINDOW]);
            consumed += WINDOW;
            // Before detection, so the voice detector sees a usable level too: at
            // -60 dBFS it missed nearly half the speech in a real recording.
            self.agc.process(&mut window);
            let prob = vad.process_window(&mut self.vad_state, &window)?;
            if let Some(seg) = self.segmenter.push(&window, prob) {
                out.push(self.shift(seg));
            }
        }
        self.pending.drain(..consumed);
        Ok(())
    }

    /// Move a segment from resampler time back onto the recording's timeline.
    fn shift(&self, mut seg: AudioSegment) -> AudioSegment {
        seg.t_start = (seg.t_start - self.delay_s).max(0.0);
        seg.t_end = (seg.t_end - self.delay_s).max(0.0);
        seg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/silero_vad.onnx")
    }

    /// A channel of the generated fixture, if it has been built.
    ///
    /// These tests deliberately use *real synthesized speech* rather than a
    /// synthetic tone. An earlier version fed Silero a sum of sine waves and got no
    /// segments at all — which was Silero being right, not a bug. Voice activity
    /// detection is trained on voices; anything else is a test of nothing.
    fn fixture_channel() -> Option<std::path::PathBuf> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../corpus/fixture-smoke/ch01-marie.wav");
        p.exists().then_some(p)
    }

    fn read_wav_48k(path: &std::path::Path) -> Vec<f32> {
        let mut r = hound::WavReader::open(path).expect("opening fixture wav");
        assert_eq!(r.spec().sample_rate, 48_000);
        r.samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32_768.0)
            .collect()
    }

    fn run_fixture(cfg: VadConfig) -> Option<(Vec<AudioSegment>, f64)> {
        let (Some(wav), true) = (fixture_channel(), model_path().exists()) else {
            eprintln!("skipping: needs models/ and corpus/fixture-smoke (make-fixture)");
            return None;
        };
        let audio = read_wav_48k(&wav);
        let mut vad = SileroSession::load(&model_path()).unwrap();
        let mut fe = ChannelFrontend::new(1, cfg).unwrap();
        let mut out = Vec::new();
        // 0.25 s blocks, the same shape the replay engine feeds.
        for block in audio.chunks(12_000) {
            fe.push_block(&mut vad, block, &mut out).unwrap();
        }
        fe.finish(&mut vad, &mut out).unwrap();
        Some((out, audio.len() as f64 / 48_000.0))
    }

    #[test]
    fn a_block_of_silence_produces_no_segments() {
        if !model_path().exists() {
            eprintln!("skipping: no VAD model");
            return;
        }
        let mut vad = SileroSession::load(&model_path()).unwrap();
        let mut fe = ChannelFrontend::new(1, VadConfig::default()).unwrap();
        let mut out = Vec::new();
        for _ in 0..10 {
            fe.push_block(&mut vad, &vec![0.0; 12_000], &mut out)
                .unwrap();
        }
        fe.finish(&mut vad, &mut out).unwrap();
        assert!(out.is_empty(), "silence produced {} segment(s)", out.len());
    }

    #[test]
    fn real_speech_becomes_segments_on_the_recordings_timeline() {
        let Some((out, total_s)) = run_fixture(VadConfig::default()) else {
            return;
        };

        let finals: Vec<&AudioSegment> = out.iter().filter(|s| !s.interim).collect();
        assert!(
            !finals.is_empty(),
            "no segments from a channel of spoken lines"
        );

        for s in &out {
            assert!(s.t_start >= 0.0, "negative timestamp {:.3}", s.t_start);
            assert!(s.t_end > s.t_start, "empty segment at {:.3}", s.t_start);
            // Nothing may claim to happen after the recording ends. This is the
            // assertion that catches an uncompensated resampler delay, a dropped
            // carry buffer, or a timeline computed from the 16 kHz stream.
            assert!(
                s.t_end <= total_s + 0.1,
                "segment ends at {:.3} s but the audio is {total_s:.3} s long",
                s.t_end
            );
        }

        // The fixture starts with one second of lead-in silence before MARIE speaks.
        let first = finals[0];
        assert!(
            first.t_start > 0.5,
            "first segment started at {:.3} s",
            first.t_start
        );

        // Final segments must not overlap: each is one closed speech run.
        for w in finals.windows(2) {
            assert!(
                w[1].t_start >= w[0].t_end - 1e-6,
                "segments overlap: {:.3}-{:.3} then {:.3}",
                w[0].t_start,
                w[0].t_end,
                w[1].t_start
            );
        }
    }

    #[test]
    fn interim_segments_precede_the_line_they_belong_to() {
        let Some((out, _)) = run_fixture(VadConfig::default()) else {
            return;
        };
        let interims = out.iter().filter(|s| s.interim).count();
        assert!(
            interims > 0,
            "no partial hypotheses over a whole channel of speech"
        );

        // Every interim must share a start with the final segment that follows it
        // and end earlier — it is a prefix of that line, not a separate utterance.
        let mut pending: Vec<&AudioSegment> = Vec::new();
        for s in &out {
            if s.interim {
                pending.push(s);
                continue;
            }
            for p in pending.drain(..) {
                assert!(
                    (p.t_start - s.t_start).abs() < 1e-6,
                    "interim at {:.3} does not share a start with its final at {:.3}",
                    p.t_start,
                    s.t_start
                );
                assert!(p.t_end < s.t_end, "interim is not shorter than its final");
                assert!(p.samples.len() < s.samples.len());
            }
        }
    }

    #[test]
    fn switching_off_interim_emission_leaves_only_closed_runs() {
        let cfg = VadConfig {
            interim_interval_ms: 0,
            ..Default::default()
        };
        let Some((out, _)) = run_fixture(cfg) else {
            return;
        };
        assert!(!out.is_empty(), "no segments at all");
        assert!(
            out.iter().all(|s| !s.interim),
            "interim emission was switched off"
        );
    }
}
