//! `track` — run the tracker over an existing transcript.
//!
//! Splitting transcription from tracking is the single most useful decision in this
//! harness: transcribing an act takes minutes, tracking it takes milliseconds. Every
//! matcher and threshold experiment runs against a transcript produced once, and the
//! result is bit-identical on any machine — unlike the ASR stage, whose Metal float
//! arithmetic is only reproducible on the same hardware. This path is therefore the
//! one the pinned regression baseline is built from.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use choufleur_core::tracker::{Confidence, Tracker, TrackerConfig, TrackerEvent};

use crate::formats::{read_jsonl, write_jsonl, SegmentRecord, TraceRecord};
use crate::manifest::Corpus;

pub fn run(
    corpus_path: &Path,
    segments_path: &Path,
    out: &Path,
    tracker_config: Option<&Path>,
    audio_root: Option<PathBuf>,
    timings: bool,
) -> Result<()> {
    let corpus = Corpus::load(corpus_path, audio_root)?;
    let (_script, prepared) = super::load_script(&corpus.script_path())?;
    let cfg = load_config(tracker_config)?;
    let segments: Vec<SegmentRecord> = read_jsonl(segments_path)?;

    let trace = track_segments(&prepared, cfg, &segments, timings);
    write_jsonl(out, &trace)?;

    let positions = trace.iter().filter(|r| r.is_position()).count();
    let rejected = trace.len() - positions;
    println!(
        "tracked {} segment(s) ({} filtered upstream) → {} position update(s), {} other event(s)",
        segments.len(),
        segments.iter().filter(|s| !s.is_kept()).count(),
        positions,
        rejected
    );
    if let Some(last) = trace.iter().rev().find(|r| r.is_position()) {
        println!(
            "final position: line {} ({}) at {:.1} s",
            last.line_index.unwrap_or(0),
            last.line_id.as_deref().unwrap_or("?"),
            last.t
        );
    }
    println!("wrote {}", out.display());
    Ok(())
}

/// The pure part: segments in, trace out. Shared with the tests and, later, with
/// the coupled audio→track path.
/// `timings` adds per-segment match cost to the trace. It is **off by default**
/// and must stay that way for baseline runs: a trace carrying wall-clock
/// measurements is not byte-reproducible, and byte-reproducibility is the whole
/// reason this path — rather than the ASR path — is the pinned regression artifact.
pub fn track_segments(
    prepared: &choufleur_core::script::PreparedScript,
    cfg: TrackerConfig,
    segments: &[SegmentRecord],
    timings: bool,
) -> Vec<TraceRecord> {
    let mut tracker = Tracker::new(prepared, cfg);
    let mut trace = Vec::new();
    for rec in segments {
        if !rec.is_kept() {
            // Recorded, not tracked. The hallucination filter's decisions belong in
            // the trace — they are the raw material for the `channel_garbled`
            // warning family and the first thing to look at when coverage drops.
            trace.push(TraceRecord {
                t: rec.t_end,
                kind: crate::formats::TraceKind::SegmentFiltered,
                channel: Some(rec.channel),
                reason: None,
                ..empty_at(rec.t_end)
            });
            continue;
        }
        let seg = rec.to_segment();
        let started = std::time::Instant::now();
        let events: Vec<TrackerEvent> = tracker.update(&seg);
        let match_us = started.elapsed().as_micros() as u64;
        for (i, ev) in events.iter().enumerate() {
            let mut r = TraceRecord::from_event(rec.t_end, Some(rec.channel), ev);
            // Attribute the matching cost once per segment, not once per event.
            if i == 0 {
                if timings {
                    r.match_us = Some(match_us);
                }
                r.latency_ms = rec.latency_ms;
            }
            trace.push(r);
        }
    }
    trace
}

fn empty_at(t: f64) -> TraceRecord {
    TraceRecord {
        t,
        kind: crate::formats::TraceKind::SegmentFiltered,
        line_index: None,
        line_id: None,
        confidence: None,
        score: None,
        reason: None,
        best_index: None,
        channel: None,
        match_us: None,
        latency_ms: None,
    }
}

fn load_config(path: Option<&Path>) -> Result<TrackerConfig> {
    match path {
        None => Ok(TrackerConfig::default()),
        Some(p) => {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("reading tracker config {}", p.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("parsing tracker config {}", p.display()))
        }
    }
}

/// Summarize a trace for the console — used by `track` and `eval` alike.
pub fn describe_confidence(trace: &[TraceRecord]) -> String {
    let mut lost = 0usize;
    let mut word = 0usize;
    let mut line = 0usize;
    for r in trace {
        match r.confidence {
            Some(Confidence::Lost) => lost += 1,
            Some(Confidence::Word) => word += 1,
            Some(Confidence::Line) => line += 1,
            _ => {}
        }
    }
    format!("{word} word-level, {line} line-level, {lost} lost")
}
