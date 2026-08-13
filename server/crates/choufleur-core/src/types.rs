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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<String>,
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
}

impl TranscriptSegment {
    pub fn duration(&self) -> f64 {
        (self.t_end - self.t_start).max(0.0)
    }
}
