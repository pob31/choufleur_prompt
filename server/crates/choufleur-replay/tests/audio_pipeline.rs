//! The whole pipeline on real audio: WAVs → VAD → Whisper → tracker → score.
//!
//! Unlike `pipeline.rs`, which simulates transcription to isolate the parts that
//! must be exactly right, this runs the actual models. It is the test that would
//! catch a resampler feeding the VAD the wrong rate, a Silero window of the wrong
//! length, a language forced from the wrong place, or a segment queue that lost
//! its ordering — none of which any unit test can see.
//!
//! It skips itself unless both the models and the generated fixture are present:
//!
//! ```text
//! scripts/fetch-models.sh
//! cargo run -p choufleur-replay -- make-fixture corpus/fixture-smoke
//! ```
//!
//! The fixture is synthesized speech, so passing here says the plumbing works. It
//! says nothing about the go/no-go gate, which only real theatre audio can answer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use choufleur_core::lang::{LangCode, NormalizerRegistry};
use choufleur_core::script::PreparedScript;
use choufleur_core::tracker::TrackerConfig;
use choufleur_replay::cmd::track::track_segments;
use choufleur_replay::engine::{Consumer, DecodeHint, Engine, EngineConfig};
use choufleur_replay::eval::metrics::{evaluate, EvalReport, Gate};
use choufleur_replay::formats::{read_jsonl, GroundTruthLine, SegmentRecord};
use choufleur_replay::manifest::Corpus;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn prerequisites() -> Option<(PathBuf, PathBuf, PathBuf)> {
    let root = repo_root();
    let fixture = root.join("corpus/fixture-smoke/manifest.json");
    let whisper = root.join("server/models/ggml-small.bin");
    let vad = root.join("server/models/silero_vad.onnx");
    if fixture.exists() && whisper.exists() && vad.exists() {
        Some((fixture, whisper, vad))
    } else {
        eprintln!(
            "skipping: needs scripts/fetch-models.sh and \
             `choufleur-replay make-fixture corpus/fixture-smoke`"
        );
        None
    }
}

/// Decodes in each character's own language and collects what comes back.
struct Collector {
    records: Vec<SegmentRecord>,
    lang_of: HashMap<String, Vec<LangCode>>,
    default_lang: LangCode,
}

impl Consumer for Collector {
    fn decode_hint(&mut self, _channel: u16, character: Option<&str>) -> DecodeHint {
        let langs = character
            .and_then(|c| self.lang_of.get(c))
            .cloned()
            .unwrap_or_else(|| vec![self.default_lang.clone()]);
        DecodeHint {
            langs,
            prompt: None,
        }
    }
    fn on_segment(&mut self, record: &SegmentRecord) {
        self.records.push(record.clone());
    }
}

struct Run {
    segments: Vec<SegmentRecord>,
    report: EvalReport,
    realtime_factor: f64,
    prepared: PreparedScript,
}

fn run_pipeline(interim_ms: Option<u32>) -> Option<Run> {
    let (fixture, whisper, vad) = prerequisites()?;
    let corpus = Corpus::load(&fixture, None).expect("loading fixture corpus");
    let text = std::fs::read_to_string(corpus.script_path()).unwrap();
    let script: choufleur_core::script::Script = serde_json::from_str(&text).unwrap();
    let mut reg = NormalizerRegistry::with_defaults();
    let prepared = PreparedScript::build(&script, &mut reg);

    let mut cfg = EngineConfig::new(whisper, vad);
    if let Some(ms) = interim_ms {
        cfg.vad.interim_interval_ms = ms;
    }
    let mut collector = Collector {
        records: Vec::new(),
        lang_of: choufleur_replay::cmd::character_languages(&prepared),
        default_lang: script.default_lang[0].clone(),
    };
    let mut engine = Engine::load(cfg).expect("loading models");
    let stats = engine
        .run(&corpus, &mut collector)
        .expect("running the pipeline");

    let ground_truth: Vec<GroundTruthLine> =
        read_jsonl(&corpus.ground_truth_path().unwrap()).unwrap();
    let index_of: HashMap<String, usize> = prepared
        .lines
        .iter()
        .map(|l| (l.id.clone(), l.index))
        .collect();
    let trace = track_segments(
        &prepared,
        TrackerConfig::default(),
        &collector.records,
        false,
    );
    let (report, unknown) = evaluate(
        &ground_truth,
        &trace,
        &collector.records,
        &index_of,
        Gate::default(),
    );
    assert_eq!(unknown, 0);

    Some(Run {
        segments: collector.records,
        report,
        realtime_factor: stats.realtime_factor,
        prepared,
    })
}

#[test]
fn the_whole_pipeline_tracks_the_fixture_from_audio() {
    let Some(run) = run_pipeline(None) else {
        return;
    };
    let r = &run.report;

    assert!(
        r.coverage.within_1 > 0.9,
        "coverage {:.3} — the pipeline heard the act but could not follow it",
        r.coverage.within_1
    );
    assert!(r.confident_wrong.is_empty(), "{:?}", r.confident_wrong);
    assert_eq!(
        r.lag.undetected_count, 0,
        "lines never reached: {:?}",
        r.lag
    );
    assert!(r.result.passed, "gate not met: {:?}", r.result);
}

#[test]
fn each_channel_is_decoded_in_its_own_language() {
    // The single most expensive ASR mistake available is forcing the wrong
    // language, and on a bilingual show it is one line of code away at all times.
    let Some(run) = run_pipeline(None) else {
        return;
    };
    let finals: Vec<&SegmentRecord> = run.segments.iter().filter(|s| !s.interim).collect();
    assert!(finals.len() >= 15, "only {} final segments", finals.len());

    for s in &finals {
        let Some(ch) = s.character.as_deref() else {
            continue;
        };
        let expected = run
            .prepared
            .lines
            .iter()
            .find(|l| l.character == ch)
            .and_then(|l| l.langs().next())
            .expect("every character speaks something");
        assert_eq!(
            &s.langs[0], expected,
            "channel {} ({ch}) was decoded as {:?}, not {expected:?}",
            s.channel, s.langs[0]
        );
    }
    // Both languages must actually appear, or the assertion above is vacuous.
    let langs: std::collections::HashSet<&str> =
        finals.iter().map(|s| s.langs[0].as_str()).collect();
    assert!(
        langs.contains("fr") && langs.contains("en"),
        "languages seen: {langs:?}"
    );
}

#[test]
fn recognition_is_close_enough_that_matching_is_not_doing_all_the_work() {
    let Some(run) = run_pipeline(None) else {
        return;
    };
    // Every final segment should resemble some line of the script. This is a check
    // on the audio path, not the matcher: if the resampler or VAD were wrong, the
    // text would be plausible French or English but not *this* script's.
    let mut reg = NormalizerRegistry::with_defaults();
    let mut matched = 0usize;
    let finals: Vec<&SegmentRecord> = run.segments.iter().filter(|s| !s.interim).collect();
    for s in &finals {
        let heard = reg.prepare(&s.text, &s.langs[0]);
        let best = run
            .prepared
            .lines
            .iter()
            .filter_map(|l| l.match_text(&s.langs[0]))
            .map(|mt| {
                choufleur_core::matcher::token_set_ratio(&heard.token_refs(), &mt.token_refs())
            })
            .fold(0.0f64, f64::max);
        if best > 0.8 {
            matched += 1;
        }
    }
    let ratio = matched as f64 / finals.len() as f64;
    assert!(
        ratio > 0.8,
        "only {matched}/{} segments resembled any script line",
        finals.len()
    );
}

#[test]
fn interim_emission_is_what_buys_the_lag_budget() {
    // The finding this whole segmentation policy exists for, asserted end to end
    // on real audio rather than argued: with one segment per utterance the tracker
    // cannot learn a line until the actor stops, so detection lag is bounded below
    // by the line's own duration and the gate is unreachable.
    let Some(without) = run_pipeline(Some(0)) else {
        return;
    };
    let Some(with) = run_pipeline(None) else {
        return;
    };

    assert!(without.segments.iter().all(|s| !s.interim));
    assert!(with.segments.iter().any(|s| s.interim));

    assert!(
        with.report.lag.detection_lag.median < without.report.lag.detection_lag.median,
        "interim {:.2}s vs final-only {:.2}s — interim emission should reduce lag",
        with.report.lag.detection_lag.median,
        without.report.lag.detection_lag.median
    );
    assert!(
        with.report.lag.detection_lag.median <= 2.0,
        "median lag {:.2}s still misses the gate",
        with.report.lag.detection_lag.median
    );

    // ...and it is paid for in decodes. Both configurations must still run faster
    // than real time, which is the devplan's compute criterion.
    assert!(with.segments.len() > without.segments.len());
    assert!(
        with.realtime_factor > 1.0 && without.realtime_factor > 1.0,
        "faster-than-real-time: with interim {:.1}×, without {:.1}×",
        with.realtime_factor,
        without.realtime_factor
    );
}
