//! Hallucination filter v0.
//!
//! Whisper invents fluent, confident text when fed something that is not speech —
//! room tone, a slammed door, a burst of applause (PRD, *Hallucination on silence
//! and noise*). VAD gating removes most of it; this catches the rest, before it
//! reaches the tracker and moves the position.
//!
//! The same signal has a second job: a channel whose levels look fine but whose
//! decodes are consistently garbage is the `channel_garbled` input-health warning
//! of Family B. So nothing is thrown away silently — every rejection is reported
//! with its reason, and the replay trace records it.

use choufleur_core::lang::LangCode;
use choufleur_core::normalize::{normalize_base, tokens};
use serde::{Deserialize, Serialize};

/// What the recognizer said, plus its own opinion of it.
#[derive(Clone, Debug)]
pub struct DecodeOutput {
    pub text: String,
    /// Mean token log-probability. More negative is less certain.
    pub avg_logprob: f32,
    /// Whisper's estimate that the audio contained no speech at all.
    pub no_speech_prob: f32,
    pub lang: LangCode,
    pub decode_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// Nothing was said.
    Empty,
    /// The model itself reports no speech, and is unsure of what it wrote anyway.
    NoSpeech,
    /// The classic Whisper failure: the same words over and over.
    RepetitionLoop,
    /// A known canned output — subtitle credits and sign-offs learned from
    /// training data, emitted verbatim over silence.
    KnownFiller,
    /// More words than a human mouth could produce in the time available.
    ImpossibleRate,
}

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Empty => "empty",
            DropReason::NoSpeech => "no_speech",
            DropReason::RepetitionLoop => "repetition_loop",
            DropReason::KnownFiller => "known_filler",
            DropReason::ImpossibleRate => "impossible_rate",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    Drop(DropReason),
}

impl Verdict {
    pub fn is_keep(self) -> bool {
        matches!(self, Verdict::Keep)
    }
    pub fn reason(self) -> Option<DropReason> {
        match self {
            Verdict::Drop(r) => Some(r),
            Verdict::Keep => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FilterConfig {
    /// Above this, and with a poor `avg_logprob`, the decode is discarded.
    pub no_speech_prob_max: f32,
    /// Only combined with `no_speech_prob_max` — a confident decode is kept even
    /// if the model doubts there was speech, because on a mic'ed stage there was.
    pub avg_logprob_min: f32,
    /// A word n-gram repeated at least this many times consecutively is a loop.
    pub repeat_ngram: usize,
    pub repeat_ngram_min: usize,
    /// A single token repeated at least this many times consecutively is a loop.
    pub repeat_token_min: usize,
    /// Words per second beyond which the text cannot have been spoken.
    pub max_words_per_second: f64,
    /// Filler phrases are only suspicious on short segments; someone may really
    /// have said "thank you".
    pub filler_max_duration_s: f64,
}

impl Default for FilterConfig {
    fn default() -> Self {
        FilterConfig {
            no_speech_prob_max: 0.60,
            avg_logprob_min: -1.0,
            repeat_ngram: 4,
            repeat_ngram_min: 3,
            repeat_token_min: 6,
            // Rapid stage delivery reaches ~5 words/s; 9 is not a human being.
            max_words_per_second: 9.0,
            filler_max_duration_s: 2.5,
        }
    }
}

/// Canned outputs Whisper emits over non-speech, per language.
///
/// Matched against fully normalized text and only on short segments. The list is
/// deliberately short and exact — a fuzzy filler filter would eat real lines.
const FILLERS: &[(&str, &[&str])] = &[
    (
        "en",
        &[
            "thank you",
            "thanks for watching",
            "thank you for watching",
            "you",
            "bye",
            "subtitles by the amara org community",
            "please subscribe",
        ],
    ),
    (
        "fr",
        &[
            "sous titres réalisés par la communauté d amara org",
            "sous titres réalisés para la communauté d amara org",
            "merci d avoir regardé cette vidéo",
            "merci",
            "abonnez vous",
        ],
    ),
];

pub struct HallucinationFilter {
    cfg: FilterConfig,
}

impl Default for HallucinationFilter {
    fn default() -> Self {
        HallucinationFilter::new(FilterConfig::default())
    }
}

impl HallucinationFilter {
    pub fn new(cfg: FilterConfig) -> Self {
        HallucinationFilter { cfg }
    }

    pub fn config(&self) -> &FilterConfig {
        &self.cfg
    }

    pub fn check(&self, out: &DecodeOutput, duration_s: f64) -> Verdict {
        let norm = normalize_base(&out.text);
        let words: Vec<&str> = tokens(&norm).collect();
        if words.is_empty() {
            return Verdict::Drop(DropReason::Empty);
        }
        if out.no_speech_prob > self.cfg.no_speech_prob_max
            && out.avg_logprob < self.cfg.avg_logprob_min
        {
            return Verdict::Drop(DropReason::NoSpeech);
        }
        if duration_s > 0.0 && words.len() as f64 / duration_s > self.cfg.max_words_per_second {
            return Verdict::Drop(DropReason::ImpossibleRate);
        }
        if self.is_repetition(&words) {
            return Verdict::Drop(DropReason::RepetitionLoop);
        }
        if duration_s <= self.cfg.filler_max_duration_s && self.is_filler(&norm, &out.lang) {
            return Verdict::Drop(DropReason::KnownFiller);
        }
        Verdict::Keep
    }

    /// A run of one token, or an n-gram repeated back to back.
    ///
    /// Note this deliberately does *not* fire on a line that merely repeats a word
    /// twice ("no, no") — theatre is full of those.
    fn is_repetition(&self, words: &[&str]) -> bool {
        let mut run = 1usize;
        for w in words.windows(2) {
            run = if w[0] == w[1] { run + 1 } else { 1 };
            if run >= self.cfg.repeat_token_min {
                return true;
            }
        }
        for n in 2..=self.cfg.repeat_ngram {
            if words.len() < n * self.cfg.repeat_ngram_min {
                continue;
            }
            let mut reps = 1usize;
            let mut i = n;
            while i + n <= words.len() {
                if words[i - n..i] == words[i..i + n] {
                    reps += 1;
                    if reps >= self.cfg.repeat_ngram_min {
                        return true;
                    }
                } else {
                    reps = 1;
                }
                i += n;
            }
        }
        false
    }

    fn is_filler(&self, normalized: &str, lang: &LangCode) -> bool {
        FILLERS
            .iter()
            .filter(|(l, _)| *l == lang.primary())
            .any(|(_, list)| list.contains(&normalized))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(text: &str, lang: &str) -> DecodeOutput {
        DecodeOutput {
            text: text.into(),
            avg_logprob: -0.3,
            no_speech_prob: 0.02,
            lang: LangCode::new(lang),
            decode_ms: 120,
        }
    }

    fn check(text: &str, lang: &str, dur: f64) -> Verdict {
        HallucinationFilter::default().check(&out(text, lang), dur)
    }

    #[test]
    fn real_lines_pass() {
        for (text, lang, dur) in [
            ("Tu ne devrais pas être ici.", "fr", 2.0),
            ("I came as soon as I heard about the fire.", "en", 3.0),
            ("No, no — you misunderstand me entirely.", "en", 2.5),
        ] {
            assert_eq!(
                check(text, lang, dur),
                Verdict::Keep,
                "{text:?} was dropped"
            );
        }
    }

    #[test]
    fn empty_and_punctuation_only_decodes_are_dropped() {
        assert_eq!(check("", "en", 1.0), Verdict::Drop(DropReason::Empty));
        assert_eq!(
            check("  ...  ", "en", 1.0),
            Verdict::Drop(DropReason::Empty)
        );
    }

    #[test]
    fn the_model_doubting_itself_and_the_audio_is_enough_to_drop() {
        let mut o = out("Thank you very much indeed", "en");
        o.no_speech_prob = 0.9;
        o.avg_logprob = -1.6;
        assert_eq!(
            HallucinationFilter::default().check(&o, 2.0),
            Verdict::Drop(DropReason::NoSpeech)
        );
    }

    #[test]
    fn a_confident_decode_survives_a_high_no_speech_probability() {
        // Close-mic'ed stage audio often trips Whisper's no-speech estimate; if the
        // decode itself is confident, believe the decode.
        let mut o = out("Alors pars avant qu'il ne revienne", "fr");
        o.no_speech_prob = 0.8;
        o.avg_logprob = -0.25;
        assert_eq!(HallucinationFilter::default().check(&o, 2.5), Verdict::Keep);
    }

    #[test]
    fn repetition_loops_are_caught() {
        assert_eq!(
            check("Thank you. Thank you. Thank you. Thank you.", "en", 4.0),
            Verdict::Drop(DropReason::RepetitionLoop)
        );
        assert_eq!(
            check("la la la la la la la la", "fr", 4.0),
            Verdict::Drop(DropReason::RepetitionLoop)
        );
        assert_eq!(
            check("je ne sais pas je ne sais pas je ne sais pas", "fr", 5.0),
            Verdict::Drop(DropReason::RepetitionLoop)
        );
    }

    #[test]
    fn a_line_that_repeats_a_phrase_twice_is_not_a_loop() {
        // Deliberate repetition is a rhetorical device, not a failure mode.
        assert_eq!(check("No, no.", "en", 1.5), Verdict::Keep);
        assert_eq!(
            check("Never, never again will I ask you for anything.", "en", 3.0),
            Verdict::Keep
        );
        assert_eq!(check("Alors pars. Alors pars.", "fr", 2.0), Verdict::Keep);
    }

    #[test]
    fn known_fillers_are_dropped_only_on_short_segments() {
        assert_eq!(
            check("Thank you.", "en", 1.0),
            Verdict::Drop(DropReason::KnownFiller)
        );
        assert_eq!(
            check(
                "Sous-titres réalisés par la communauté d'Amara.org",
                "fr",
                2.0
            ),
            Verdict::Drop(DropReason::KnownFiller)
        );
        // The same words over four seconds of audio were probably really spoken.
        assert_eq!(check("Thank you.", "en", 4.0), Verdict::Keep);
        // ...and a filler in the wrong language is not a filler.
        assert_eq!(check("Merci", "en", 1.0), Verdict::Keep);
    }

    #[test]
    fn text_no_mouth_could_have_produced_is_dropped() {
        let text = "and then she said that the whole of the second act would have to be \
                    rewritten before anybody could possibly go home tonight";
        assert_eq!(
            check(text, "en", 1.0),
            Verdict::Drop(DropReason::ImpossibleRate)
        );
        assert_eq!(check(text, "en", 8.0), Verdict::Keep);
    }

    #[test]
    fn reasons_have_stable_names_for_the_trace() {
        assert_eq!(DropReason::RepetitionLoop.as_str(), "repetition_loop");
        assert_eq!(DropReason::KnownFiller.as_str(), "known_filler");
    }
}
