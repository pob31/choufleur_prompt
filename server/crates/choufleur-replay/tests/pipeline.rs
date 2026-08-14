//! End-to-end: fixture → segments → track → eval, with no audio and no ASR.
//!
//! Transcription is simulated by turning ground truth back into transcript
//! segments, optionally damaged in the specific ways real ASR damages text. That
//! isolates the half of the pipeline that must be *exactly* right — timeline
//! arithmetic, matching, tracking, scoring — from the half that can only ever be
//! measured statistically. When the real corpus arrives, only the segment source
//! changes; every assertion here still holds.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use choufleur_core::lang::{LangCode, NormalizerRegistry};
use choufleur_core::script::{PreparedScript, Script};
use choufleur_core::tracker::TrackerConfig;
use choufleur_replay::cmd::make_fixture::{default_script, generate, Layout, Synth};
use choufleur_replay::cmd::track::track_segments;
use choufleur_replay::eval::metrics::{evaluate, Gate};
use choufleur_replay::formats::{read_jsonl, GroundTruthLine, SegmentRecord};

/// Stand-in for `say`: length proportional to the text, so timelines are realistic
/// without depending on which voices are installed.
struct MockSynth;
impl Synth for MockSynth {
    fn speak(&self, text: &str, _voice: &str, _wpm: u32) -> Result<Vec<f32>> {
        // ~14 characters per second of speech.
        let frames = (text.len() as f64 / 14.0 * 48_000.0) as usize;
        Ok((0..frames).map(|i| (i as f32 * 0.01).sin() * 0.4).collect())
    }
}

struct Fixture {
    script: Script,
    prepared: PreparedScript,
    ground_truth: Vec<GroundTruthLine>,
    index_of: HashMap<String, usize>,
    dir: std::path::PathBuf,
}

fn build_fixture(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("choufleur-pipeline-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let script = default_script();
    let voices: BTreeMap<String, String> = script
        .characters
        .iter()
        .map(|c| (c.id.clone(), "Mock".into()))
        .collect();
    generate(
        &dir,
        &script,
        &MockSynth,
        &voices,
        42,
        180,
        -60.0,
        Layout::Sequential,
    )
    .expect("fixture generation");

    let ground_truth: Vec<GroundTruthLine> =
        read_jsonl(&dir.join("ground-truth.jsonl")).expect("ground truth");
    let mut reg = NormalizerRegistry::with_defaults();
    let prepared = PreparedScript::build(&script, &mut reg);
    let index_of = prepared
        .lines
        .iter()
        .map(|l| (l.id.clone(), l.index))
        .collect();
    Fixture {
        script,
        prepared,
        ground_truth,
        index_of,
        dir,
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Turn ground truth into transcript segments, as a perfect ASR would produce
/// them: one segment per line, on the speaker's channel, ending when they stop.
fn perfect_segments(f: &Fixture) -> Vec<SegmentRecord> {
    f.ground_truth
        .iter()
        .filter(|g| !g.omitted)
        .map(|g| {
            let idx = f.index_of[&g.line_id];
            let line = &f.prepared.lines[idx];
            SegmentRecord {
                gain_db: None,
                speech_dbfs: None,
                channel: g.channel.unwrap_or(1),
                character: Some(line.character.clone()),
                t_start: g.onset,
                t_end: g.end,
                text: line.text.clone(),
                langs: line.langs().cloned().collect(),
                avg_logprob: -0.2,
                no_speech_prob: 0.01,
                forced_split: false,
                interim: false,
                decode_ms: Some(150),
                latency_ms: None,
                filtered: None,
            }
        })
        .collect()
}

fn run_eval(
    f: &Fixture,
    segments: &[SegmentRecord],
) -> choufleur_replay::eval::metrics::EvalReport {
    run_eval_against(f, segments, &f.ground_truth)
}

fn run_eval_against(
    f: &Fixture,
    segments: &[SegmentRecord],
    ground_truth: &[GroundTruthLine],
) -> choufleur_replay::eval::metrics::EvalReport {
    let trace = track_segments(&f.prepared, TrackerConfig::default(), segments, false);
    let (report, unknown) = evaluate(ground_truth, &trace, segments, &f.index_of, Gate::default());
    assert_eq!(unknown, 0, "fixture ground truth must match its own script");
    report
}

/// Split each line into partial segments emitted every `interval` seconds while
/// the actor is still speaking, each carrying the words spoken so far.
///
/// This is the "sliding-window / local-agreement policy for stable partial
/// results" the PRD asks for, simulated at the transcript level.
fn interim_segments(f: &Fixture, interval: f64) -> Vec<SegmentRecord> {
    let mut out = Vec::new();
    for base in perfect_segments(f) {
        let words: Vec<&str> = base.text.split_whitespace().collect();
        let dur = base.t_end - base.t_start;
        let mut t = base.t_start + interval;
        while t < base.t_end - 0.2 {
            let frac = (t - base.t_start) / dur;
            let n = ((words.len() as f64 * frac).round() as usize).clamp(1, words.len());
            out.push(SegmentRecord {
                gain_db: None,
                speech_dbfs: None,
                t_end: t,
                text: words[..n].join(" "),
                interim: true,
                ..base.clone()
            });
            t += interval;
        }
        out.push(base);
    }
    out.sort_by(|a, b| a.t_end.partial_cmp(&b.t_end).unwrap());
    out
}

#[test]
fn a_perfect_transcript_tracks_the_whole_fixture() {
    let f = build_fixture("perfect");
    let segments = perfect_segments(&f);
    assert_eq!(segments.len(), f.script.lines.len());

    let report = run_eval(&f, &segments);
    assert!(
        report.coverage.within_1 > 0.95,
        "coverage {:.3} on a perfect transcript",
        report.coverage.within_1
    );
    assert!(
        report.confident_wrong.is_empty(),
        "{:?}",
        report.confident_wrong
    );
    assert!(report.outages.is_empty(), "{:?}", report.outages);
    assert_eq!(report.lag.undetected_count, 0, "{:?}", report.lag);
    assert!(report.result.coverage && report.result.honesty && report.result.recovery);

    // ...but the lag gate fails, and it is *supposed* to on this input. One
    // segment per line means the tracker cannot learn a line until the actor has
    // finished saying it, so detection lag is bounded below by the line's own
    // duration. No amount of matcher tuning fixes that; it is a segmentation
    // property. See `interim_segments_bring_detection_lag_under_the_gate`.
    let mean_line_s = report.coverage.speech_active_s / report.lag.total_lines as f64;
    assert!(
        report.lag.detection_lag.median >= mean_line_s * 0.6,
        "lag {:.2}s should track line duration {:.2}s",
        report.lag.detection_lag.median,
        mean_line_s
    );
    assert!(
        !report.result.lag,
        "the end-of-utterance floor must show up as a failed lag gate"
    );
}

#[test]
fn interim_segments_bring_detection_lag_under_the_gate() {
    let f = build_fixture("interim");
    // Same audio, same tracker — the only change is that the ASR stage emits a
    // partial hypothesis every 1.5 s instead of waiting for the actor to stop.
    let report = run_eval(&f, &interim_segments(&f, 1.5));

    assert!(
        report.lag.detection_lag.median <= 2.0,
        "median lag {:.2}s should now meet the 2.0 s gate",
        report.lag.detection_lag.median
    );
    assert!(
        report.confident_wrong.is_empty(),
        "{:?}",
        report.confident_wrong
    );
    assert!(
        report.coverage.within_1 > 0.95,
        "coverage {:.3}",
        report.coverage.within_1
    );
    assert!(report.result.passed, "{:?}", report.result);
}

#[test]
fn tracking_survives_a_realistically_damaged_transcript() {
    let f = build_fixture("damaged");
    // The three things Whisper actually does to theatre audio: drops a word,
    // mangles a rare one, and loses accents entirely.
    let damage = |text: &str, n: usize| -> String {
        let mut words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
        if words.len() > 4 {
            words.remove(n % words.len());
        }
        words
            .join(" ")
            .replace(['é', 'è'], "e")
            .replace('à', "a")
            .replace('û', "u")
            .to_lowercase()
    };
    let segments: Vec<SegmentRecord> = perfect_segments(&f)
        .into_iter()
        .enumerate()
        .map(|(i, mut s)| {
            s.text = damage(&s.text, i + 1);
            s.avg_logprob = -0.7;
            s
        })
        .collect();

    let report = run_eval(&f, &segments);
    assert!(
        report.coverage.within_1 > 0.85,
        "coverage {:.3} on a damaged transcript",
        report.coverage.within_1
    );
    assert!(
        report.confident_wrong.is_empty(),
        "damage must cost coverage, never produce confident-wrong: {:?}",
        report.confident_wrong
    );
}

#[test]
fn a_cut_costs_one_segment_of_staleness_and_then_recovers() {
    let f = build_fixture("cut");
    // The director cuts four consecutive lines: the audio never contains them, so
    // ground truth must not claim them either.
    const CUT: std::ops::Range<usize> = 6..10;
    let segments: Vec<SegmentRecord> = interim_segments(&f, 1.5)
        .into_iter()
        .filter(|s| !CUT.contains(&f.index_of[&line_id_at(&f, s)]))
        .collect();
    let ground_truth: Vec<GroundTruthLine> = f
        .ground_truth
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, mut g)| {
            g.omitted = CUT.contains(&i);
            g
        })
        .collect();

    let report = run_eval_against(&f, &segments, &ground_truth);
    // A cut wider than skip tolerance cannot be noticed until the *next* material
    // is heard, so a brief stale window is physically unavoidable. What must not
    // happen is a sustained one.
    let worst = report
        .confident_wrong
        .iter()
        .map(|e| e.duration_s)
        .fold(0.0, f64::max);
    assert!(
        worst < 3.0,
        "staleness after a cut must be brief, was {worst:.1}s: {:?}",
        report.confident_wrong
    );
    assert!(
        report.coverage.within_1 > 0.85,
        "coverage {:?}",
        report.coverage
    );
    assert!(
        report.outages.iter().all(|o| o.end.is_some()),
        "the tracker must find its way back: {:?}",
        report.outages
    );
}

/// Which script line a synthesized segment came from — the fixture's segments are
/// generated from the script, so the text identifies the line exactly.
fn line_id_at(f: &Fixture, s: &SegmentRecord) -> String {
    f.ground_truth
        .iter()
        .min_by(|a, b| {
            (a.onset - s.t_start)
                .abs()
                .partial_cmp(&(b.onset - s.t_start).abs())
                .unwrap()
        })
        .map(|g| g.line_id.clone())
        .unwrap()
}

#[test]
fn improvised_material_is_reported_as_uncertainty_not_as_motion() {
    let f = build_fixture("improv");
    let mut segments = perfect_segments(&f);
    // Ten seconds of off-script talk in the middle of the act.
    let at = segments[8].t_end;
    for k in 0..4 {
        segments.insert(
            9 + k,
            SegmentRecord {
                gain_db: None,
                speech_dbfs: None,
                channel: 2,
                character: Some("char-john".into()),
                t_start: at + k as f64 * 2.5,
                t_end: at + k as f64 * 2.5 + 2.4,
                text: "sorry could we take that again from the top of the page please".into(),
                langs: vec![LangCode::new("en")],
                avg_logprob: -0.4,
                no_speech_prob: 0.02,
                forced_split: false,
                interim: false,
                decode_ms: Some(150),
                latency_ms: None,
                filtered: None,
            },
        );
    }
    // Ground truth still describes the scripted performance, so those ten seconds
    // are not speech-active time and do not enter the denominator.
    let report = run_eval(&f, &segments);
    assert!(
        report.confident_wrong.is_empty(),
        "improvisation must not move the position: {:?}",
        report.confident_wrong
    );
    assert!(
        report.coverage.within_1 > 0.9,
        "coverage {:?}",
        report.coverage
    );
}

#[test]
fn the_pipeline_is_deterministic() {
    let f = build_fixture("determinism");
    let segments = perfect_segments(&f);
    let a = track_segments(&f.prepared, TrackerConfig::default(), &segments, false);
    let b = track_segments(&f.prepared, TrackerConfig::default(), &segments, false);
    let ja: Vec<String> = a
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    let jb: Vec<String> = b
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    assert_eq!(
        ja, jb,
        "the same segments must produce a byte-identical trace"
    );
}

#[test]
fn filtered_segments_are_recorded_but_never_tracked() {
    let f = build_fixture("filtered");
    let mut segments = perfect_segments(&f);
    // A hallucination on a dead channel, which the ASR filter caught.
    segments.insert(
        4,
        SegmentRecord {
            gain_db: None,
            speech_dbfs: None,
            channel: 3,
            character: Some("char-sarah".into()),
            t_start: segments[3].t_end + 0.1,
            t_end: segments[3].t_end + 1.6,
            text: "Thank you. Thank you. Thank you.".into(),
            langs: vec![LangCode::new("en")],
            avg_logprob: -1.4,
            no_speech_prob: 0.87,
            forced_split: false,
            interim: false,
            decode_ms: Some(90),
            latency_ms: None,
            filtered: Some("repetition_loop".into()),
        },
    );
    let trace = track_segments(&f.prepared, TrackerConfig::default(), &segments, false);
    let filtered = trace
        .iter()
        .filter(|r| r.kind == choufleur_replay::formats::TraceKind::SegmentFiltered)
        .count();
    assert_eq!(filtered, 1, "the drop must be visible in the trace");

    let (report, _) = evaluate(
        &f.ground_truth,
        &trace,
        &segments,
        &f.index_of,
        Gate::default(),
    );
    assert_eq!(report.pipeline.segments_filtered, 1);
    assert_eq!(
        report.pipeline.filtered_by_reason.get("repetition_loop"),
        Some(&1)
    );
    assert!(report.confident_wrong.is_empty());
}
