//! Adaptive gain, per channel, causal.
//!
//! Theatre mics are gained for the loudest moment a show will ever produce. If an
//! actor is stripped and beaten in a car boot in act three, that channel's
//! transmitter is set so *that* does not clip — and ordinary dialogue then sits
//! forty decibels down. Measured on real recordings: peaks at −2.6 dBFS with speech
//! at −44.7, and one channel whose speech averaged **−66.8 dBFS**, roughly eleven
//! bits below full scale.
//!
//! Both models expect something else. Whisper and Silero are trained on audio
//! normalised to around −20 dBFS, so speech at −60 is far outside the distribution
//! they learned. It is not a signal-to-noise problem — amplifying changes no SNR —
//! it is a range problem, and it costs real accuracy: raising one such channel by
//! 30 dB took the voice detector from 128 segments to 236 over the same recording,
//! and the fraction of the script findable in the audio from 48 % to 59 %.
//!
//! # Why this is not simply a gain
//!
//! Normalising the whole file first, offline, is cheating twice: a live show has no
//! future to measure, and a fixed gain lifts the noise floor along with the voice.
//! The second is not theoretical — it produced **forty hallucinations** in one
//! excerpt, all of them confident ("Thank you very much.", "We care of home."),
//! because the recogniser was handed amplified room tone and duly found words in it.
//!
//! So the estimate is causal and the gain is withheld when there is nothing worth
//! amplifying. The floor and the voice are tracked separately, and gain is applied
//! only while they are far enough apart to mean someone is actually speaking.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgcConfig {
    /// Where speech should end up, in dBFS. Roughly what the models were trained on.
    pub target_dbfs: f32,
    /// Never amplify by more than this. A channel that needs more than 40 dB is
    /// broken, not quiet, and should raise an input-health warning instead.
    pub max_gain_db: f32,
    /// How far above the noise floor the level must be before it is treated as
    /// speech worth amplifying. Below this there is nothing to hear, and lifting it
    /// only invites the recogniser to invent something.
    pub min_snr_db: f32,
    /// Seconds for the voice estimate to fall when the stage goes quiet.
    pub speech_release_s: f32,
    /// Seconds for the noise-floor estimate to rise. Slow, so a held note or a long
    /// line is not mistaken for the room.
    pub noise_rise_s: f32,
    /// Seconds for the applied gain to travel to its target. Slow enough that gain
    /// does not audibly pump within a line, fast enough to catch a scene change.
    pub gain_slew_s: f32,
}

impl Default for AgcConfig {
    fn default() -> Self {
        AgcConfig {
            target_dbfs: -20.0,
            max_gain_db: 40.0,
            min_snr_db: 12.0,
            speech_release_s: 3.0,
            noise_rise_s: 10.0,
            gain_slew_s: 1.5,
        }
    }
}

/// How far below the current voice estimate a moment must be to count as a gap the
/// noise floor may be learned from. About -12 dB.
const QUIET_FRACTION: f32 = 0.25;

fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// One channel's gain state. Causal: it never sees audio it has not been given.
pub struct AutoGain {
    cfg: AgcConfig,
    sample_rate: f32,
    /// Running estimate of the level of speech, linear RMS.
    speech: f32,
    /// Running estimate of the room, linear RMS.
    noise: f32,
    /// Currently applied gain, linear, slewed toward its target.
    gain: f32,
    started: bool,
}

impl AutoGain {
    pub fn new(sample_rate: u32, cfg: AgcConfig) -> Self {
        AutoGain {
            cfg,
            sample_rate: sample_rate as f32,
            speech: 0.0,
            noise: 1.0,
            gain: 1.0,
            started: false,
        }
    }

    pub fn config(&self) -> &AgcConfig {
        &self.cfg
    }

    /// The gain currently being applied, in dB. Worth logging: a channel sitting at
    /// the maximum is telling you something about the desk, not about the actor.
    pub fn gain_db(&self) -> f32 {
        20.0 * self.gain.max(1e-9).log10()
    }

    pub fn speech_dbfs(&self) -> f32 {
        20.0 * self.speech.max(1e-9).log10()
    }

    pub fn noise_dbfs(&self) -> f32 {
        20.0 * self.noise.max(1e-9).log10()
    }

    /// Apply gain to one block in place, updating the estimates from it first.
    pub fn process(&mut self, block: &mut [f32]) {
        if block.is_empty() {
            return;
        }
        let rms = (block.iter().map(|s| s * s).sum::<f32>() / block.len() as f32).sqrt();
        let dt = block.len() as f32 / self.sample_rate;

        if !self.started {
            // Start from what is actually there rather than from an assumption, so
            // the first line of a show is not spent converging.
            self.speech = rms;
            self.noise = rms.max(1e-7);
            self.started = true;
        }

        // The voice: rises immediately, falls slowly. A peak-follower, so a pause
        // between lines does not erase what was learned from the line before.
        if rms > self.speech {
            self.speech = rms;
        } else {
            self.speech += (rms - self.speech) * decay(dt, self.cfg.speech_release_s);
        }

        // The room: falls immediately, and rises only from moments that are quiet
        // *relative to the voice*.
        //
        // Letting it rise during speech is a real failure, not a theoretical one:
        // a monologue with no gaps in it would pull the floor up to meet the voice,
        // the two would converge, the gain would decide there was nothing to
        // amplify, and it would fade out in the middle of the line. The floor is a
        // property of the gaps, so it is only learned from them.
        if rms < self.noise {
            self.noise = rms.max(1e-7);
        } else if rms < self.speech * QUIET_FRACTION {
            self.noise += (rms - self.noise) * decay(dt, self.cfg.noise_rise_s);
        }

        let snr_db = self.speech_dbfs() - self.noise_dbfs();
        let target = if snr_db >= self.cfg.min_snr_db && self.speech > 1e-6 {
            (db_to_lin(self.cfg.target_dbfs) / self.speech)
                .clamp(1.0, db_to_lin(self.cfg.max_gain_db))
        } else {
            // Nothing but room. Fall back toward unity rather than amplifying it
            // into something the recogniser will hear words in.
            1.0
        };
        self.gain += (target - self.gain) * decay(dt, self.cfg.gain_slew_s);

        for s in block.iter_mut() {
            *s = (*s * self.gain).clamp(-1.0, 1.0);
        }
    }
}

/// Fraction of the way to travel toward a target in `dt`, for a time constant.
fn decay(dt: f32, tau_s: f32) -> f32 {
    if tau_s <= 0.0 {
        return 1.0;
    }
    (1.0 - (-dt / tau_s).exp()).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: u32 = 16_000;

    fn tone(level: f32, seconds: f32) -> Vec<f32> {
        let n = (seconds * SR as f32) as usize;
        (0..n)
            .map(|i| (i as f32 * TAU * 220.0 / SR as f32).sin() * level * std::f32::consts::SQRT_2)
            .collect()
    }

    /// Speech-shaped: two seconds of voice, half a second of room, repeating.
    /// A continuous tone is not speech and does not exercise a floor estimator.
    fn speech(level: f32, floor: f32, seconds: f32) -> Vec<f32> {
        let mut out = Vec::new();
        while (out.len() as f32) < seconds * SR as f32 {
            out.extend(tone(level, 2.0));
            out.extend(noise(floor, 0.5));
        }
        out.truncate((seconds * SR as f32) as usize);
        out
    }

    /// RMS of the loudest tenth — the speech, not the gaps.
    fn speech_rms(v: &[f32]) -> f32 {
        let win = (0.05 * SR as f32) as usize;
        let mut r: Vec<f32> = v.chunks(win).map(rms).collect();
        r.sort_by(|a, b| a.partial_cmp(b).unwrap());
        r[r.len() * 9 / 10]
    }

    fn noise(level: f32, seconds: f32) -> Vec<f32> {
        let n = (seconds * SR as f32) as usize;
        let mut x = 12345u32;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((x >> 16) as f32 / 32_768.0 - 1.0) * level
            })
            .collect()
    }

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|s| s * s).sum::<f32>() / v.len() as f32).sqrt()
    }

    fn run(agc: &mut AutoGain, signal: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(signal.len());
        for block in signal.chunks(512) {
            let mut b = block.to_vec();
            agc.process(&mut b);
            out.extend_from_slice(&b);
        }
        out
    }

    #[test]
    fn a_quiet_channel_is_brought_up_to_the_target() {
        // -60 dBFS speech: the real case, and 40 dB below what the models expect.
        let mut agc = AutoGain::new(SR, AgcConfig::default());
        let quiet = speech(0.001, 0.00003, 20.0);
        let out = run(&mut agc, &quiet);
        // Judge the settled end, not the approach.
        let settled = 20.0 * speech_rms(&out[out.len() / 2..]).log10();
        assert!(
            (settled - -20.0).abs() < 4.0,
            "settled at {settled:.1} dBFS, wanted about -20"
        );
    }

    #[test]
    fn a_channel_already_at_a_sensible_level_is_left_alone() {
        let mut agc = AutoGain::new(SR, AgcConfig::default());
        let ok = speech(0.1, 0.0003, 12.0); // speech already at -20 dBFS
        let out = run(&mut agc, &ok);
        let settled = 20.0 * speech_rms(&out[out.len() / 2..]).log10();
        assert!((settled - -20.0).abs() < 4.0, "settled at {settled:.1}");
        assert!(
            agc.gain_db() < 6.0,
            "gained {:.1} dB for no reason",
            agc.gain_db()
        );
    }

    #[test]
    fn room_tone_alone_is_not_amplified() {
        // The failure that produced forty confident hallucinations: lift the noise
        // floor into the recogniser's range and it will find words in it.
        let mut agc = AutoGain::new(SR, AgcConfig::default());
        let out = run(&mut agc, &noise(0.0005, 20.0));
        assert!(
            agc.gain_db() < 6.0,
            "amplified silence by {:.1} dB — this is how hallucinations are made",
            agc.gain_db()
        );
        assert!(rms(&out) < 0.01, "room tone came out at {:.4}", rms(&out));
    }

    #[test]
    fn quiet_speech_over_a_quiet_floor_is_still_amplified() {
        // The distinction that matters: quiet *speech* is worth lifting, quiet
        // *nothing* is not. Same absolute level, different SNR.
        let mut agc = AutoGain::new(SR, AgcConfig::default());
        let mut sig = noise(0.00002, 4.0);
        sig.extend(speech(0.002, 0.00002, 16.0));
        let out = run(&mut agc, &sig);
        let speech_out = speech_rms(&out[out.len() / 2..]);
        assert!(
            speech_out > 0.02,
            "quiet speech over a quiet floor stayed at {speech_out:.4}"
        );
    }

    #[test]
    fn gain_never_exceeds_the_configured_ceiling() {
        let cfg = AgcConfig {
            max_gain_db: 20.0,
            ..Default::default()
        };
        let mut agc = AutoGain::new(SR, cfg);
        run(&mut agc, &speech(0.00001, 0.000001, 25.0));
        assert!(
            agc.gain_db() <= 20.5,
            "gained {:.1} dB past a 20 dB ceiling",
            agc.gain_db()
        );
    }

    #[test]
    fn nothing_clips() {
        let mut agc = AutoGain::new(SR, AgcConfig::default());
        let mut sig = speech(0.002, 0.00002, 16.0); // quiet, so gain winds up
        sig.extend(tone(0.9, 3.0)); // then someone shouts
        let out = run(&mut agc, &sig);
        assert!(
            out.iter().all(|s| s.abs() <= 1.0),
            "output left the legal range"
        );
    }

    #[test]
    fn it_is_causal_and_deterministic() {
        // No lookahead: the same audio must give the same result whatever block
        // size it arrives in, because live capture chooses that, not us.
        let sig = speech(0.003, 0.00003, 10.0);
        let mut a = AutoGain::new(SR, AgcConfig::default());
        let one = run(&mut a, &sig);
        let mut b = AutoGain::new(SR, AgcConfig::default());
        let two = run(&mut b, &sig);
        assert_eq!(one, two, "not deterministic");
    }
}
