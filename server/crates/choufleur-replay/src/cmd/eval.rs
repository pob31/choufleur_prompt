//! `eval` — score a trace against ground truth and state the gate result.
//!
//! One command, one report. The devplan's go/no-go criteria are evaluated here
//! rather than by eye, so "did it pass" has an answer that does not depend on who
//! is reading the numbers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::eval::metrics::{evaluate, EvalReport, Gate};
use crate::formats::{read_jsonl, write_json, GroundTruthLine, SegmentRecord, TraceRecord};
use crate::manifest::Corpus;

pub fn run(
    corpus_path: &Path,
    trace_path: &Path,
    segments_path: Option<&Path>,
    out: Option<&Path>,
    pretty: bool,
    audio_root: Option<PathBuf>,
) -> Result<()> {
    let corpus = Corpus::load(corpus_path, audio_root)?;
    let (_script, prepared) = super::load_script(&corpus.script_path())?;

    let gt_path = corpus
        .ground_truth_path()
        .context("this corpus has no ground truth; label it before running eval")?;
    let ground_truth: Vec<GroundTruthLine> = read_jsonl(&gt_path)?;
    let trace: Vec<TraceRecord> = read_jsonl(trace_path)?;
    let segments: Vec<SegmentRecord> = match segments_path {
        Some(p) => read_jsonl(p)?,
        None => Vec::new(),
    };

    let index_of: HashMap<String, usize> = prepared
        .lines
        .iter()
        .map(|l| (l.id.clone(), l.index))
        .collect();

    let (report, unknown) = evaluate(&ground_truth, &trace, &segments, &index_of, Gate::default());
    if unknown > 0 {
        eprintln!(
            "warning: {unknown} ground-truth line(s) reference ids absent from the script — \
             the labels and the script have drifted apart"
        );
    }

    print_report(&corpus.manifest.show, &corpus.manifest.act, &report);

    if let Some(out) = out {
        write_json(out, &report, pretty)?;
        println!("\nwrote {}", out.display());
    }
    if !report.result.passed {
        // A non-zero exit makes this usable as a regression check without parsing
        // the report.
        bail!("gate not met");
    }
    Ok(())
}

fn print_report(show: &str, act: &str, r: &EvalReport) {
    let pass = |ok: bool| if ok { "PASS" } else { "FAIL" };
    println!("\n=== {show} / {act} ===");
    println!(
        "speech-active time      {:.1} s over {} labelled line(s)",
        r.coverage.speech_active_s, r.lag.total_lines
    );
    println!();
    println!(
        "Coverage   ±0 {:.1}%   ±1 {:.1}%   ±3 {:.1}%      [{}]",
        r.coverage.exact * 100.0,
        r.coverage.within_1 * 100.0,
        r.coverage.within_3 * 100.0,
        pass(r.result.coverage)
    );
    println!(
        "           lost {:.1}% of speech-active time",
        r.coverage.lost_fraction * 100.0
    );
    println!(
        "Lag        median {:.2} s   p95 {:.2} s   max {:.2} s   [{}]",
        r.lag.detection_lag.median,
        r.lag.detection_lag.p95,
        r.lag.detection_lag.max,
        pass(r.result.lag)
    );
    println!(
        "           {} detected, {} already passed, {} never reached",
        r.lag.detection_lag.count, r.lag.early_count, r.lag.undetected_count
    );
    println!(
        "Honesty    {} confident-wrong event(s)                    [{}]",
        r.confident_wrong.len(),
        pass(r.result.honesty)
    );
    for e in r.confident_wrong.iter().take(5) {
        println!(
            "           {:>7.1}s–{:<7.1} said line {}, actually {}",
            e.start, e.end, e.reported_index, e.actual_index
        );
    }
    println!(
        "Recovery   {} outage(s)                                   [{}]",
        r.outages.len(),
        pass(r.result.recovery)
    );
    for o in r.outages.iter().take(5) {
        match o.end {
            Some(e) => println!(
                "           {:>7.1}s–{:<7.1} {:?}, {:.1} s of speech{}",
                o.start,
                e,
                o.cause,
                o.recovery_speech_s,
                if o.closed_by_reanchor {
                    ", closed by re-anchor"
                } else {
                    ""
                }
            ),
            None => println!(
                "           {:>7.1}s–  never recovered ({:?})",
                o.start, o.cause
            ),
        }
    }
    match r.result.latency {
        Some(ok) => println!(
            "Latency    median {:.0} ms   p95 {:.0} ms   max {:.0} ms   [{}]",
            r.pipeline.latency_ms.median,
            r.pipeline.latency_ms.p95,
            r.pipeline.latency_ms.max,
            pass(ok)
        ),
        None => println!("Latency    not measured (batch run)"),
    }
    if r.pipeline.segments_total > 0 {
        println!(
            "ASR        {} segment(s), {} filtered ({})",
            r.pipeline.segments_total,
            r.pipeline.segments_filtered,
            if r.pipeline.filtered_by_reason.is_empty() {
                "—".to_string()
            } else {
                let mut v: Vec<String> = r
                    .pipeline
                    .filtered_by_reason
                    .iter()
                    .map(|(k, n)| format!("{k}×{n}"))
                    .collect();
                v.sort();
                v.join(", ")
            }
        );
        if r.pipeline.decode_ms.count > 0 {
            println!(
                "           decode median {:.0} ms, max {:.0} ms",
                r.pipeline.decode_ms.median, r.pipeline.decode_ms.max
            );
        }
    }
    println!(
        "\nGATE: {}",
        if r.result.passed { "PASS" } else { "NOT MET" }
    );
}
