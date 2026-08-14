//! `track` — run the tracker over an existing transcript.
//!
//! Splitting transcription from tracking is the single most useful decision in this
//! harness: transcribing an act takes minutes, tracking it takes milliseconds. Every
//! matcher and threshold experiment runs against a transcript produced once, and the
//! result is bit-identical on any machine — unlike the ASR stage, whose Metal float
//! arithmetic is only reproducible on the same hardware. This path is therefore the
//! one the pinned regression baseline is built from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use choufleur_core::lang::LangCode;
use choufleur_core::prompt::{static_prompt, tracker_prompt, BiasMode, PromptConfig};
use choufleur_core::script::PreparedScript;
use choufleur_core::tracker::{Confidence, Tracker, TrackerConfig, TrackerEvent};

use crate::engine::{Consumer, DecodeHint, Engine, EngineConfig};
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

/// Tracks while transcribing, and answers each decode hint from where the tracker
/// currently believes the show to be.
///
/// This is the feedback loop the devplan calls tracker-fed biasing: the decoder is
/// primed with the lines we expect next, which is the cheapest accuracy win
/// available when aligning against a known script. It is also the one that can
/// bite — a prompt full of expected text makes Whisper likelier to *produce* that
/// text on noise — which is why the hallucination filter runs regardless of bias
/// mode and why `confident-wrong` is the metric that decides whether it earns its
/// place. Run the sweep with `--bias none` as the control.
struct TrackingConsumer<'a> {
    tracker: Tracker<'a>,
    script: &'a PreparedScript,
    trace: Vec<TraceRecord>,
    bias: BiasMode,
    prompt_cfg: PromptConfig,
    lang_of: HashMap<String, LangCode>,
    default_lang: LangCode,
    static_prompt: String,
    timings: bool,
}

impl Consumer for TrackingConsumer<'_> {
    fn decode_hint(&mut self, _channel: u16, character: Option<&str>) -> DecodeHint {
        // Language comes from the script, never from the audio. Prefer the language
        // of the line we expect *here*, falling back to the character's usual one:
        // that is what makes a bilingual line decode correctly.
        let position = self.tracker.position();
        let expected = self
            .script
            .lines
            .get(position)
            .filter(|l| character.is_none_or(|c| l.character == c))
            .and_then(|l| l.langs().next().cloned());
        let lang = expected
            .or_else(|| character.and_then(|c| self.lang_of.get(c).cloned()))
            .unwrap_or_else(|| self.default_lang.clone());

        let prompt = match self.bias {
            BiasMode::None => None,
            BiasMode::Static => Some(self.static_prompt.clone()),
            BiasMode::Tracker => {
                let p = tracker_prompt(self.script, position, &lang, character, &self.prompt_cfg);
                (!p.is_empty()).then_some(p)
            }
        };
        DecodeHint { lang, prompt }
    }

    fn on_segment(&mut self, record: &SegmentRecord) {
        if !record.is_kept() {
            self.trace.push(TraceRecord {
                t: record.t_end,
                kind: crate::formats::TraceKind::SegmentFiltered,
                channel: Some(record.channel),
                ..empty_at(record.t_end)
            });
            return;
        }
        let seg = record.to_segment();
        let started = std::time::Instant::now();
        let events = self.tracker.update(&seg);
        let match_us = started.elapsed().as_micros() as u64;
        for (i, ev) in events.iter().enumerate() {
            let mut r = TraceRecord::from_event(record.t_end, Some(record.channel), ev);
            if i == 0 {
                if self.timings {
                    r.match_us = Some(match_us);
                }
                r.latency_ms = record.latency_ms;
            }
            self.trace.push(r);
        }
    }
}

/// Track straight from audio, with the tracker able to bias its own recognition.
#[allow(clippy::too_many_arguments)]
pub fn run_from_audio(
    corpus_path: &Path,
    out: &Path,
    segments_out: Option<&Path>,
    tracker_config: Option<&Path>,
    whisper_model: Option<&Path>,
    vad_model: Option<&Path>,
    realtime: bool,
    bias: BiasMode,
    mixdown: bool,
    channels: Option<Vec<u16>>,
    interim_ms: Option<u32>,
    audio_root: Option<PathBuf>,
    timings: bool,
) -> Result<()> {
    let corpus = Corpus::load(corpus_path, audio_root)?;
    let (script, prepared) = super::load_script(&corpus.script_path())?;
    let cfg = load_config(tracker_config)?;

    let mut ecfg = EngineConfig::new(
        super::resolve_model(whisper_model, super::DEFAULT_WHISPER_MODEL)?,
        super::resolve_model(vad_model, super::DEFAULT_VAD_MODEL)?,
    );
    ecfg.realtime = realtime;
    ecfg.mixdown = mixdown;
    ecfg.channels = channels;
    if let Some(ms) = interim_ms {
        ecfg.vad.interim_interval_ms = ms;
    }

    println!("model:   {}", ecfg.whisper_model.display());
    println!("bias:    {bias:?}");
    println!("mode:    {}", if realtime { "realtime" } else { "batch" });

    let names: Vec<String> = script.characters.iter().map(|c| c.name.clone()).collect();
    let mut consumer = TrackingConsumer {
        tracker: Tracker::new(&prepared, cfg),
        script: &prepared,
        trace: Vec::new(),
        bias,
        prompt_cfg: PromptConfig::default(),
        lang_of: super::character_languages(&prepared),
        default_lang: script
            .default_lang
            .first()
            .cloned()
            .unwrap_or(LangCode::new("en")),
        static_prompt: static_prompt(script.title.as_deref(), &names),
        timings,
    };

    // Segments are captured as well as tracked, so the exact same run can be
    // replayed through `track --segments` without touching audio again.
    let mut captured: Vec<SegmentRecord> = Vec::new();
    {
        struct Tee<'t> {
            inner: &'t mut dyn Consumer,
            captured: &'t mut Vec<SegmentRecord>,
        }
        impl Consumer for Tee<'_> {
            fn decode_hint(&mut self, channel: u16, character: Option<&str>) -> DecodeHint {
                self.inner.decode_hint(channel, character)
            }
            fn on_segment(&mut self, record: &SegmentRecord) {
                self.captured.push(record.clone());
                self.inner.on_segment(record);
            }
        }
        let mut engine = Engine::load(ecfg)?;
        let mut tee = Tee {
            inner: &mut consumer,
            captured: &mut captured,
        };
        let stats = engine.run(&corpus, &mut tee)?;
        println!("\n{stats}");
        if stats.fell_behind > 0 {
            println!(
                "note:    processing was bursty — behind on {} occasion(s), worst {:.2} s",
                stats.fell_behind, stats.max_backlog_s
            );
        }
    }

    write_jsonl(out, &consumer.trace)?;
    if let Some(p) = segments_out {
        write_jsonl(p, &captured)?;
        println!("wrote {}", p.display());
    }
    report(&consumer.trace, &captured);
    println!("wrote {}", out.display());
    Ok(())
}

fn report(trace: &[TraceRecord], segments: &[SegmentRecord]) {
    let positions = trace.iter().filter(|r| r.is_position()).count();
    let kept = segments.iter().filter(|s| s.is_kept()).count();
    println!(
        "{} segment(s), {kept} kept -> {positions} position update(s)",
        segments.len()
    );
    if let Some(last) = trace.iter().rev().find(|r| r.is_position()) {
        println!(
            "final position: line {} ({}) at {:.1} s",
            last.line_index.unwrap_or(0),
            last.line_id.as_deref().unwrap_or("?"),
            last.t
        );
    }
}

/// Summarize a trace's confidence mix for the console.
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
