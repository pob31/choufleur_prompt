//! On-disk exchange formats between the replay subcommands.
//!
//! Everything is JSONL — one self-describing record per line, appendable, greppable,
//! and diffable. `transcribe` writes segments, `track` writes a trace, `eval` reads
//! both plus ground truth. Keeping the stages file-separated is what lets matcher
//! work iterate in seconds against a transcript produced once in minutes.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use choufleur_core::lang::LangCode;
use choufleur_core::tracker::{Confidence, PositionCause, RejectReason, TrackerEvent};
use choufleur_core::types::{AsrQuality, TranscriptSegment};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// One line of ground truth: when a scripted line was actually spoken.
///
/// Produced by forced alignment and then hand-corrected — see `corpus/README.md`.
/// Onsets are the labelling target (±200 ms); ends are coarse and used only to
/// define speech-active time.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroundTruthLine {
    pub line_id: String,
    pub onset: f64,
    pub end: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u16>,
    /// Set when the line was not performed in this run at all.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub omitted: bool,
}

/// One transcribed segment as written by `transcribe`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentRecord {
    pub channel: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<String>,
    pub t_start: f64,
    pub t_end: f64,
    pub text: String,
    pub langs: Vec<LangCode>,
    #[serde(default)]
    pub avg_logprob: f32,
    #[serde(default)]
    pub no_speech_prob: f32,
    #[serde(default)]
    pub forced_split: bool,
    /// Milliseconds spent inside Whisper for this segment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_ms: Option<u64>,
    /// Wall-clock seconds from the segment's audio end to the match being done.
    /// Only meaningful in `--realtime` runs; this is the number the ≤1.5 s / 3 s
    /// budget is measured against, queue wait included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// Set when the hallucination filter rejected this segment. Rejected segments
    /// are still written — they are the raw material for `channel_garbled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered: Option<String>,
}

impl SegmentRecord {
    pub fn to_segment(&self) -> TranscriptSegment {
        TranscriptSegment {
            channel: self.channel,
            character: self.character.clone(),
            t_start: self.t_start,
            t_end: self.t_end,
            text: self.text.clone(),
            langs: self.langs.clone(),
            quality: AsrQuality {
                avg_logprob: self.avg_logprob,
                no_speech_prob: self.no_speech_prob,
            },
            forced_split: self.forced_split,
        }
    }

    pub fn is_kept(&self) -> bool {
        self.filtered.is_none()
    }
}

/// What kind of thing happened, flattened for grep-ability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    Position,
    SkipAdvance,
    Reanchor,
    Jump,
    Manual,
    Decay,
    Lost,
    Rejected,
    SegmentFiltered,
}

/// One tracker decision, timestamped on the audio timeline.
///
/// Rejections are recorded as deliberately as advances: an eval that can only see
/// where the tracker went cannot explain why it stayed put, and "why did it lose
/// act 2 scene 3 every night" is the question the run log exists to answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceRecord {
    /// Audio-timeline seconds — the end of the segment that caused this.
    pub t: f64,
    pub kind: TraceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RejectReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

impl TraceRecord {
    /// Translate a tracker event into a trace record. The `cause` of a position
    /// change becomes the record kind, so a trace can be filtered by "show me every
    /// re-anchor" without decoding a nested enum.
    pub fn from_event(t: f64, channel: Option<u16>, event: &TrackerEvent) -> Self {
        let base = TraceRecord {
            t,
            kind: TraceKind::Position,
            line_index: None,
            line_id: None,
            confidence: None,
            score: None,
            reason: None,
            best_index: None,
            channel,
            match_us: None,
            latency_ms: None,
        };
        match event {
            TrackerEvent::Position {
                line_index,
                line_id,
                confidence,
                score,
                cause,
            } => TraceRecord {
                kind: match cause {
                    PositionCause::Follow => TraceKind::Position,
                    PositionCause::Skip => TraceKind::SkipAdvance,
                    PositionCause::Reanchor => TraceKind::Reanchor,
                    PositionCause::Jump => TraceKind::Jump,
                    PositionCause::Manual => TraceKind::Manual,
                },
                line_index: Some(*line_index),
                line_id: Some(line_id.clone()),
                confidence: Some(*confidence),
                score: Some(*score),
                ..base
            },
            TrackerEvent::Rejected {
                reason,
                best_score,
                best_index,
            } => TraceRecord {
                kind: TraceKind::Rejected,
                reason: Some(*reason),
                score: Some(*best_score),
                best_index: *best_index,
                ..base
            },
            TrackerEvent::ConfidenceChanged { to, .. } => TraceRecord {
                kind: if *to == Confidence::Lost {
                    TraceKind::Lost
                } else {
                    TraceKind::Decay
                },
                confidence: Some(*to),
                ..base
            },
        }
    }

    pub fn is_position(&self) -> bool {
        matches!(
            self.kind,
            TraceKind::Position
                | TraceKind::SkipAdvance
                | TraceKind::Reanchor
                | TraceKind::Jump
                | TraceKind::Manual
        )
    }
}

/// Read a JSONL file, reporting the offending line number on a parse error —
/// hand-written corpus files are the normal case here, and "expected `,` at line 1
/// column 4079" is not a usable diagnostic for a 3000-line ground truth.
pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() || line.trim_start().starts_with("//") {
            continue;
        }
        let rec = serde_json::from_str(&line)
            .with_context(|| format!("{}:{}: parsing record", path.display(), i + 1))?;
        out.push(rec);
    }
    Ok(out)
}

pub fn write_jsonl<T: Serialize>(path: &Path, records: &[T]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut w = BufWriter::new(file);
    for r in records {
        serde_json::to_writer(&mut w, r)?;
        w.write_all(b"\n")?;
    }
    w.flush()?;
    Ok(())
}

pub fn write_json<T: Serialize>(path: &Path, value: &T, pretty: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut w = BufWriter::new(file);
    if pretty {
        serde_json::to_writer_pretty(&mut w, value)?;
    } else {
        serde_json::to_writer(&mut w, value)?;
    }
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_records_round_trip_through_jsonl() {
        let dir = std::env::temp_dir().join("choufleur-formats-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("segments.jsonl");
        let recs = vec![SegmentRecord {
            channel: 1,
            character: Some("char-marie".into()),
            t_start: 1.25,
            t_end: 3.75,
            text: "Tu ne devrais pas être ici.".into(),
            langs: vec![LangCode::new("fr")],
            avg_logprob: -0.31,
            no_speech_prob: 0.02,
            forced_split: false,
            decode_ms: Some(180),
            latency_ms: None,
            filtered: None,
        }];
        write_jsonl(&path, &recs).unwrap();
        let back: Vec<SegmentRecord> = read_jsonl(&path).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].text, recs[0].text);
        assert!(back[0].is_kept());
        let seg = back[0].to_segment();
        assert_eq!(seg.character.as_deref(), Some("char-marie"));
        assert!((seg.duration() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn position_cause_becomes_the_trace_kind() {
        let ev = TrackerEvent::Position {
            line_index: 7,
            line_id: "L-0008".into(),
            confidence: Confidence::Line,
            score: 0.81,
            cause: PositionCause::Reanchor,
        };
        let rec = TraceRecord::from_event(12.5, Some(1), &ev);
        assert_eq!(rec.kind, TraceKind::Reanchor);
        assert!(rec.is_position());
        assert_eq!(rec.line_index, Some(7));
    }

    #[test]
    fn a_decay_to_lost_is_traced_as_lost() {
        let ev = TrackerEvent::ConfidenceChanged {
            from: Confidence::Line,
            to: Confidence::Lost,
            unmatched_speech_s: 21.0,
        };
        assert_eq!(
            TraceRecord::from_event(30.0, None, &ev).kind,
            TraceKind::Lost
        );
        let ev = TrackerEvent::ConfidenceChanged {
            from: Confidence::Word,
            to: Confidence::Block,
            unmatched_speech_s: 9.0,
        };
        assert_eq!(
            TraceRecord::from_event(30.0, None, &ev).kind,
            TraceKind::Decay
        );
    }
}
