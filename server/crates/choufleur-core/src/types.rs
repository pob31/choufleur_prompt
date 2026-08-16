//! Types crossing the ASR → tracker boundary.

use serde::{Deserialize, Serialize};

use crate::lang::LangCode;

/// ASR-side quality signals, used by the hallucination filter and surfaced in the
/// trace so the eval can explain *why* a match was or was not made.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct AsrQuality {
    /// Mean token log-probability of the decode. More negative is worse.
    pub avg_logprob: f32,
    /// Whisper's own probability that the segment contains no speech.
    pub no_speech_prob: f32,
}

/// One transcribed speech segment on one channel.
///
/// `t_start`/`t_end` are seconds on the **audio timeline** — the recording's own
/// clock in replay, the capture clock live. The tracker reads no other time source.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSegment {
    pub channel: u16,
    /// Character id this channel carries, or `None` for a zone channel — an
    /// ambient mic with no speaker identity, matched against any expected speaker.
    ///
    /// Kept for the single-occupant case, which is most of them, and because every
    /// segment file on disk is written this way. Use [`Self::speakers`] rather than
    /// reading it directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<String>,
    /// Everyone this channel might be carrying, when a mic is shared.
    ///
    /// A position mic belongs to a place, not a person. The operator described the real
    /// arrangement on *Lovedoll*: *"I tried to keep people on the same mic as much as
    /// possible, not always possible. Veronica and Nicolas would share sometimes."*
    /// One name would be wrong half the time and no name throws away what is known, so
    /// a channel names the few people who could be on it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub characters: Vec<String>,
    pub t_start: f64,
    pub t_end: f64,
    pub text: String,
    /// Language(s) the decode was forced to (read from the script, never detected).
    pub langs: Vec<LangCode>,
    #[serde(default)]
    pub quality: AsrQuality,
    /// True when the segmenter closed this segment on its maximum-length rule
    /// rather than on silence — the text is likely cut mid-word at both ends.
    #[serde(default)]
    pub forced_split: bool,
    /// True when this is a *partial* hypothesis: the speaker is still talking and
    /// more of this line is coming. Interim segments are what keep detection lag
    /// inside the PRD's budget, but they carry a prefix of the line rather than
    /// the line, so they are believed more cautiously.
    #[serde(default)]
    pub interim: bool,
}

impl TranscriptSegment {
    pub fn duration(&self) -> f64 {
        (self.t_end - self.t_start).max(0.0)
    }

    /// Who this audio could be, in order of how it was written down.
    ///
    /// Empty means a zone channel: no identity, matched against any expected speaker.
    /// One name is the ordinary per-actor mic. Several is a shared position mic.
    pub fn speakers(&self) -> &[String] {
        if self.characters.is_empty() {
            self.character.as_slice()
        } else {
            &self.characters
        }
    }
}
