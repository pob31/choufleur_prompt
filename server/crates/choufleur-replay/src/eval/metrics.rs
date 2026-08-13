//! Scoring a tracker run against ground truth.
//!
//! Every quantity here is defined against the devplan's go/no-go gate, and every
//! one is computed **exactly** rather than by sampling: the position and confidence
//! are piecewise-constant step functions, so the integrals are sums over the merged
//! breakpoints of ground truth and trace. There is no grid to alias against and no
//! sample rate to argue about later.
//!
//! Two conventions worth stating once, because they must not silently change when
//! real recordings arrive:
//!
//! - **Speech-active time** is the union of ground-truth line intervals. Silence,
//!   applause and scene changes are excluded from every denominator: a tracker is
//!   not being tested while nobody is speaking.
//! - **Latest onset wins.** Where two ground-truth lines overlap in time — real
//!   dialogue does — the expected position at an instant is the line with the
//!   latest onset at or before it.

use std::collections::HashMap;

use choufleur_core::tracker::Confidence;
use serde::{Deserialize, Serialize};

use crate::formats::{GroundTruthLine, SegmentRecord, TraceKind, TraceRecord};

/// Gate thresholds from the devplan's GO/NO-GO table, in one place so the report
/// can state pass/fail rather than leaving arithmetic to the reader.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gate {
    pub coverage_min: f64,
    pub lag_median_max_s: f64,
    pub lag_p95_max_s: f64,
    pub confident_wrong_max: usize,
    pub recovery_max_speech_s: f64,
    pub latency_median_max_ms: f64,
    pub latency_p95_max_ms: f64,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            coverage_min: 0.90,
            lag_median_max_s: 2.0,
            lag_p95_max_s: 4.0,
            confident_wrong_max: 1,
            recovery_max_speech_s: 30.0,
            // PRD, ASR Engine and Latency Budget: ≤1.5 s typical, ≤3 s worst case.
            latency_median_max_ms: 1500.0,
            latency_p95_max_ms: 3000.0,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Distribution {
    pub count: usize,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
    pub mean: f64,
}

impl Distribution {
    pub fn of(mut values: Vec<f64>) -> Self {
        if values.is_empty() {
            return Distribution::default();
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = values.len();
        let at = |q: f64| {
            // Nearest-rank: with a handful of cues per act, interpolation would
            // invent precision the sample size does not support.
            let rank = ((q * n as f64).ceil() as usize).clamp(1, n);
            values[rank - 1]
        };
        Distribution {
            count: n,
            median: at(0.5),
            p95: at(0.95),
            max: values[n - 1],
            mean: values.iter().sum::<f64>() / n as f64,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfidentWrongEvent {
    pub start: f64,
    pub end: f64,
    pub duration_s: f64,
    /// Where the tracker said it was, and where it actually was, at the start.
    pub reported_index: usize,
    pub actual_index: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outage {
    pub start: f64,
    /// `None` if the run ended before recovery.
    pub end: Option<f64>,
    /// Seconds of *speech* between losing the position and regaining it — the gate
    /// is stated in subsequent speech, not in wall time, because a tracker cannot
    /// re-anchor on silence.
    pub recovery_speech_s: f64,
    pub closed_by_reanchor: bool,
    pub cause: OutageCause,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutageCause {
    /// The tracker admitted it was lost.
    Lost,
    /// Worse: it was confident and wrong.
    ConfidentWrong,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageMetrics {
    pub speech_active_s: f64,
    /// Fraction of speech-active time within ±0, ±1 and ±3 lines of ground truth.
    pub exact: f64,
    pub within_1: f64,
    pub within_3: f64,
    /// Fraction of speech-active time the tracker spent admitting it was lost.
    pub lost_fraction: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LagMetrics {
    /// Audio-domain seconds from a line's onset to the tracker reaching it.
    pub detection_lag: Distribution,
    /// Lines the tracker had already passed when they were spoken — skip-tolerance
    /// artefacts, counted separately so they cannot flatter the lag distribution.
    pub early_count: usize,
    /// Lines the tracker never reached at all.
    pub undetected_count: usize,
    pub total_lines: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineMetrics {
    /// Wall-clock ms from a segment's audio end to its match being applied.
    /// Only populated by `--realtime` runs; includes queue wait, deliberately.
    pub latency_ms: Distribution,
    pub decode_ms: Distribution,
    pub segments_total: usize,
    pub segments_filtered: usize,
    pub filtered_by_reason: HashMap<String, usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateResult {
    pub coverage: bool,
    pub lag: bool,
    pub honesty: bool,
    pub recovery: bool,
    /// `None` when the run carried no latency data (a batch run).
    pub latency: Option<bool>,
    pub passed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalReport {
    pub coverage: CoverageMetrics,
    pub lag: LagMetrics,
    pub pipeline: PipelineMetrics,
    pub confident_wrong: Vec<ConfidentWrongEvent>,
    pub outages: Vec<Outage>,
    pub gate: Gate,
    pub result: GateResult,
}

/// Merge overlapping intervals; the basis for speech-active time.
fn merge_intervals(mut spans: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    spans.retain(|(a, b)| b > a);
    spans.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (a, b) in spans {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// The tracker's state as reconstructed from the trace, plus the expected state
/// from ground truth, evaluated at instants.
struct Timeline {
    /// (time, line index) — position changes, ascending.
    positions: Vec<(f64, usize)>,
    /// (time, confidence) — every record that states a confidence, ascending.
    confidences: Vec<(f64, Confidence)>,
    /// (onset, line index) — ground truth, ascending by onset.
    onsets: Vec<(f64, usize)>,
    /// Times at which a re-anchor happened, for outage attribution.
    reanchors: Vec<f64>,
}

impl Timeline {
    fn position_at(&self, t: f64) -> usize {
        match self.positions.partition_point(|(x, _)| *x <= t) {
            0 => 0, // before the first update the tracker is at the top of the script
            i => self.positions[i - 1].1,
        }
    }
    fn confidence_at(&self, t: f64) -> Confidence {
        match self.confidences.partition_point(|(x, _)| *x <= t) {
            0 => Confidence::Scene, // the tracker's initial state
            i => self.confidences[i - 1].1,
        }
    }
    fn expected_at(&self, t: f64) -> Option<usize> {
        match self.onsets.partition_point(|(x, _)| *x <= t) {
            0 => None, // before the first line was spoken there is nothing to expect
            i => Some(self.onsets[i - 1].1),
        }
    }
}

/// Compute the full report.
///
/// `line_index_of` maps a ground-truth line id to its index in the script; lines
/// whose id is unknown are skipped, with the count returned so callers can shout
/// about a mismatched corpus rather than silently scoring against half of it.
pub fn evaluate(
    ground_truth: &[GroundTruthLine],
    trace: &[TraceRecord],
    segments: &[SegmentRecord],
    line_index_of: &HashMap<String, usize>,
    gate: Gate,
) -> (EvalReport, usize) {
    let mut unknown_lines = 0usize;
    let mut gt: Vec<(f64, f64, usize)> = Vec::new(); // onset, end, index
    for l in ground_truth {
        if l.omitted {
            continue;
        }
        match line_index_of.get(&l.line_id) {
            Some(&idx) => gt.push((l.onset, l.end.max(l.onset), idx)),
            None => unknown_lines += 1,
        }
    }
    gt.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let speech = merge_intervals(gt.iter().map(|(a, b, _)| (*a, *b)).collect());
    let speech_active_s: f64 = speech.iter().map(|(a, b)| b - a).sum();

    let mut positions: Vec<(f64, usize)> = Vec::new();
    let mut confidences: Vec<(f64, Confidence)> = Vec::new();
    let mut reanchors: Vec<f64> = Vec::new();
    for r in trace {
        if let (true, Some(idx)) = (r.is_position(), r.line_index) {
            positions.push((r.t, idx));
        }
        if let Some(c) = r.confidence {
            confidences.push((r.t, c));
        }
        if r.kind == TraceKind::Reanchor {
            reanchors.push(r.t);
        }
    }
    // Stable sort keeps same-timestamp events in trace order, which is the order
    // the tracker emitted them — the last one is the state that stands.
    positions.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    confidences.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let tl = Timeline {
        positions,
        confidences,
        onsets: gt.iter().map(|(o, _, i)| (*o, *i)).collect(),
        reanchors,
    };

    // --- Sweep -------------------------------------------------------------
    // Breakpoints must include every ground-truth onset, not merely the bounds of
    // the merged speech intervals: back-to-back dialogue merges into one interval,
    // and the line boundaries inside it are exactly where the expected position
    // changes. Losing them silently mis-integrates coverage.
    let mut breakpoints: Vec<f64> = Vec::new();
    for (a, b) in &speech {
        breakpoints.push(*a);
        breakpoints.push(*b);
    }
    for (onset, end, _) in &gt {
        breakpoints.push(*onset);
        breakpoints.push(*end);
    }
    for (t, _) in &tl.positions {
        breakpoints.push(*t);
    }
    for (t, _) in &tl.confidences {
        breakpoints.push(*t);
    }
    breakpoints.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    breakpoints.dedup_by(|a, b| (*a - *b).abs() < 1e-12);

    let mut cov = CoverageMetrics {
        speech_active_s,
        ..Default::default()
    };
    let mut slices: Vec<Slice> = Vec::new();

    for w in breakpoints.windows(2) {
        let (a, b) = (w[0], w[1]);
        let dt = b - a;
        if dt <= 0.0 {
            continue;
        }
        let m = a + dt / 2.0;
        if !in_spans(&speech, m) {
            continue;
        }
        let Some(expected) = tl.expected_at(m) else {
            continue;
        };
        let actual = tl.position_at(m);
        let conf = tl.confidence_at(m);
        let err = actual.abs_diff(expected);

        if err == 0 {
            cov.exact += dt;
        }
        if err <= 1 {
            cov.within_1 += dt;
        }
        if err <= 3 {
            cov.within_3 += dt;
        }
        if conf == Confidence::Lost {
            cov.lost_fraction += dt;
        }

        let confident = conf >= Confidence::Line;
        let state = if confident && err > 3 {
            SliceState::ConfidentWrong
        } else if conf == Confidence::Lost {
            SliceState::Lost
        } else if confident && err <= 1 {
            SliceState::Ok
        } else {
            SliceState::Neutral
        };
        slices.push(Slice {
            start: a,
            end: b,
            state,
            actual,
            expected,
        });
    }
    if speech_active_s > 0.0 {
        cov.exact /= speech_active_s;
        cov.within_1 /= speech_active_s;
        cov.within_3 /= speech_active_s;
        cov.lost_fraction /= speech_active_s;
    }

    // --- Confident-wrong events -------------------------------------------
    // Adjacent wrong slices are one event; a brief correct patch inside a long
    // wrong stretch should not be reported as two separate failures.
    const MERGE_GAP_S: f64 = 2.0;
    let mut confident_wrong: Vec<ConfidentWrongEvent> = Vec::new();
    for sl in slices
        .iter()
        .filter(|s| s.state == SliceState::ConfidentWrong)
    {
        match confident_wrong.last_mut() {
            Some(e) if sl.start - e.end <= MERGE_GAP_S => {
                e.end = sl.end;
                e.duration_s = e.end - e.start;
            }
            _ => confident_wrong.push(ConfidentWrongEvent {
                start: sl.start,
                end: sl.end,
                duration_s: sl.end - sl.start,
                reported_index: sl.actual,
                actual_index: sl.expected,
            }),
        }
    }

    // --- Outages and recovery ---------------------------------------------
    let outages = build_outages(&slices, &tl.reanchors);

    // --- Detection lag ------------------------------------------------------
    let mut lags: Vec<f64> = Vec::new();
    let mut early = 0usize;
    let mut undetected = 0usize;
    for (onset, _, idx) in &gt {
        if tl.position_at(*onset) >= *idx {
            early += 1;
            continue;
        }
        match tl
            .positions
            .iter()
            .find(|(t, p)| *t >= *onset && *p >= *idx)
        {
            Some((t, _)) => lags.push(t - onset),
            None => undetected += 1,
        }
    }

    // --- Pipeline -----------------------------------------------------------
    let mut filtered_by_reason: HashMap<String, usize> = HashMap::new();
    for s in segments.iter().filter(|s| !s.is_kept()) {
        *filtered_by_reason
            .entry(s.filtered.clone().unwrap_or_default())
            .or_default() += 1;
    }
    let pipeline = PipelineMetrics {
        latency_ms: Distribution::of(
            segments
                .iter()
                .filter_map(|s| s.latency_ms.map(|v| v as f64))
                .collect(),
        ),
        decode_ms: Distribution::of(
            segments
                .iter()
                .filter_map(|s| s.decode_ms.map(|v| v as f64))
                .collect(),
        ),
        segments_total: segments.len(),
        segments_filtered: segments.iter().filter(|s| !s.is_kept()).count(),
        filtered_by_reason,
    };

    let lag = LagMetrics {
        detection_lag: Distribution::of(lags),
        early_count: early,
        undetected_count: undetected,
        total_lines: gt.len(),
    };

    let latency_ok = if pipeline.latency_ms.count > 0 {
        Some(
            pipeline.latency_ms.median <= gate.latency_median_max_ms
                && pipeline.latency_ms.p95 <= gate.latency_p95_max_ms,
        )
    } else {
        // A batch run measured no latency; it must not be scored as if it had.
        None
    };
    let recovery_ok = outages
        .iter()
        .all(|o| o.end.is_some() && o.recovery_speech_s <= gate.recovery_max_speech_s);
    let result = GateResult {
        coverage: cov.within_1 >= gate.coverage_min,
        lag: lag.detection_lag.count > 0
            && lag.detection_lag.median <= gate.lag_median_max_s
            && lag.detection_lag.p95 <= gate.lag_p95_max_s,
        honesty: confident_wrong.len() < gate.confident_wrong_max.max(1),
        recovery: recovery_ok,
        latency: latency_ok,
        passed: false,
    };
    let result = GateResult {
        passed: result.coverage
            && result.lag
            && result.honesty
            && result.recovery
            && latency_ok.unwrap_or(true),
        ..result
    };

    (
        EvalReport {
            coverage: cov,
            lag,
            pipeline,
            confident_wrong,
            outages,
            gate,
            result,
        },
        unknown_lines,
    )
}

fn in_spans(spans: &[(f64, f64)], t: f64) -> bool {
    let i = spans.partition_point(|(a, _)| *a <= t);
    i > 0 && t < spans[i - 1].1
}

/// One breakpoint-to-breakpoint slice of speech-active time, classified.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Slice {
    start: f64,
    end: f64,
    state: SliceState,
    actual: usize,
    expected: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SliceState {
    /// Confident and within a line of the truth.
    Ok,
    /// Confident and badly wrong — the failure the PRD cares most about.
    ConfidentWrong,
    /// Honestly lost.
    Lost,
    /// Neither clearly right nor clearly wrong: uncertain, or off by 2–3 lines.
    Neutral,
}

/// Walk the classified slices and pair each loss with its recovery.
///
/// An outage opens on the first `Lost` or `ConfidentWrong` slice and closes on the
/// first `Ok` slice after it. `Neutral` neither opens nor closes one: drifting to
/// block-level confidence is not yet a loss, and it is not a recovery either.
fn build_outages(slices: &[Slice], reanchors: &[f64]) -> Vec<Outage> {
    let mut outages: Vec<Outage> = Vec::new();
    let mut open: Option<(f64, OutageCause, f64)> = None; // start, cause, speech so far

    for sl in slices {
        match (&mut open, sl.state) {
            (None, SliceState::Lost) => {
                open = Some((sl.start, OutageCause::Lost, sl.end - sl.start))
            }
            (None, SliceState::ConfidentWrong) => {
                open = Some((sl.start, OutageCause::ConfidentWrong, sl.end - sl.start))
            }
            (Some((start, cause, speech)), state) => match state {
                SliceState::Ok => {
                    outages.push(finish_outage(
                        *start,
                        Some(sl.start),
                        *cause,
                        *speech,
                        reanchors,
                    ));
                    open = None;
                }
                other => {
                    *speech += sl.end - sl.start;
                    // Being wrong is worse than admitting ignorance; if both happen
                    // during one outage, the report names the worse one.
                    if other == SliceState::ConfidentWrong {
                        *cause = OutageCause::ConfidentWrong;
                    }
                }
            },
            (None, _) => {}
        }
    }
    if let Some((start, cause, speech)) = open {
        outages.push(finish_outage(start, None, cause, speech, reanchors));
    }
    outages
}

fn finish_outage(
    start: f64,
    end: Option<f64>,
    cause: OutageCause,
    recovery_speech_s: f64,
    reanchors: &[f64],
) -> Outage {
    let closed_by_reanchor =
        end.is_some_and(|e| reanchors.iter().any(|&t| t > start && t <= e + 1e-9));
    Outage {
        start,
        end,
        recovery_speech_s,
        closed_by_reanchor,
        cause,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::TraceKind;

    fn gt(id: &str, onset: f64, end: f64) -> GroundTruthLine {
        GroundTruthLine {
            line_id: id.into(),
            onset,
            end,
            channel: None,
            omitted: false,
        }
    }

    fn pos(t: f64, idx: usize, conf: Confidence) -> TraceRecord {
        TraceRecord {
            t,
            kind: TraceKind::Position,
            line_index: Some(idx),
            line_id: Some(format!("L-{:04}", idx + 1)),
            confidence: Some(conf),
            score: Some(0.9),
            reason: None,
            best_index: None,
            channel: Some(1),
            match_us: None,
            latency_ms: None,
        }
    }

    fn index_map(n: usize) -> HashMap<String, usize> {
        (0..n).map(|i| (format!("L-{:04}", i + 1), i)).collect()
    }

    /// Four lines, one second each, spoken back to back.
    fn four_lines() -> Vec<GroundTruthLine> {
        vec![
            gt("L-0001", 0.0, 1.0),
            gt("L-0002", 1.0, 2.0),
            gt("L-0003", 2.0, 3.0),
            gt("L-0004", 3.0, 4.0),
        ]
    }

    /// A trace that reaches each line `delay` seconds after it was spoken —
    /// the shape of every real run, since the tracker learns at segment *end*.
    fn late_trace(n: usize, delay: f64) -> Vec<TraceRecord> {
        (0..n)
            .map(|i| pos(i as f64 + delay, i, Confidence::Word))
            .collect()
    }

    #[test]
    fn a_prompt_trace_covers_everything_and_lags_by_the_delay() {
        let (r, unknown) = evaluate(
            &four_lines(),
            &late_trace(4, 0.2),
            &[],
            &index_map(4),
            Gate::default(),
        );
        assert_eq!(unknown, 0);
        assert!((r.coverage.speech_active_s - 4.0).abs() < 1e-9);
        // Each line spends its first 0.2 s attributed to the previous one; line 1
        // is free because the tracker starts at the top of the script.
        assert!(
            (r.coverage.exact - 0.85).abs() < 1e-9,
            "exact {}",
            r.coverage.exact
        );
        assert!((r.coverage.within_1 - 1.0).abs() < 1e-9);
        assert_eq!(r.lag.detection_lag.count, 3);
        assert_eq!(
            r.lag.early_count, 1,
            "line 1 needs no detection: it is where we start"
        );
        assert!((r.lag.detection_lag.median - 0.2).abs() < 1e-9);
        assert!(r.confident_wrong.is_empty());
        assert!(r.outages.is_empty());
        assert!(r.result.passed);
    }

    #[test]
    fn a_half_second_lag_shows_up_as_half_a_second() {
        let (r, _) = evaluate(
            &four_lines(),
            &late_trace(4, 0.5),
            &[],
            &index_map(4),
            Gate::default(),
        );
        assert!(
            (r.lag.detection_lag.median - 0.5).abs() < 1e-9,
            "{:?}",
            r.lag.detection_lag
        );
        // Half of each line's second is spent one line behind; the first is exact.
        assert!(
            (r.coverage.exact - 0.625).abs() < 1e-9,
            "exact {}",
            r.coverage.exact
        );
        // ±1 line still counts as covered, so coverage stays perfect.
        assert!((r.coverage.within_1 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn arriving_exactly_on_the_onset_counts_as_early_not_as_zero_lag() {
        // A tracker cannot know a line at the instant it begins; if the trace says
        // so, the honest reading is "already there", not "detected instantly".
        let trace: Vec<TraceRecord> = (0..4).map(|i| pos(i as f64, i, Confidence::Word)).collect();
        let (r, _) = evaluate(&four_lines(), &trace, &[], &index_map(4), Gate::default());
        assert_eq!(r.lag.early_count, 4);
        assert_eq!(r.lag.detection_lag.count, 0);
        assert!((r.coverage.exact - 1.0).abs() < 1e-9);
    }

    #[test]
    fn being_confidently_wrong_is_counted_and_fails_the_honesty_gate() {
        // Ten lines; the tracker jumps to line 9 immediately and stays there.
        let gt_lines: Vec<GroundTruthLine> = (0..10)
            .map(|i| gt(&format!("L-{:04}", i + 1), i as f64, i as f64 + 1.0))
            .collect();
        let trace = vec![pos(0.0, 9, Confidence::Word)];
        let gate = Gate {
            confident_wrong_max: 1,
            ..Gate::default()
        };
        let (r, _) = evaluate(&gt_lines, &trace, &[], &index_map(10), gate);
        assert_eq!(
            r.confident_wrong.len(),
            1,
            "adjacent slices must merge into one event"
        );
        let e = &r.confident_wrong[0];
        assert!(e.duration_s > 4.0, "event lasted {}", e.duration_s);
        assert!(
            !r.result.honesty,
            "one confident-wrong event must fail the gate"
        );
        assert!(!r.result.passed);
    }

    #[test]
    fn admitting_loss_is_not_a_confident_wrong_event() {
        let gt_lines: Vec<GroundTruthLine> = (0..10)
            .map(|i| gt(&format!("L-{:04}", i + 1), i as f64, i as f64 + 1.0))
            .collect();
        // Stuck at line 0 but honest about it.
        let trace = vec![pos(0.0, 0, Confidence::Lost)];
        let (r, _) = evaluate(&gt_lines, &trace, &[], &index_map(10), Gate::default());
        assert!(
            r.confident_wrong.is_empty(),
            "honest loss is not a wrong answer"
        );
        assert!(r.result.honesty);
        assert!(r.coverage.lost_fraction > 0.9);
        // It is, however, an unrecovered outage.
        assert_eq!(r.outages.len(), 1);
        assert_eq!(r.outages[0].cause, OutageCause::Lost);
        assert!(r.outages[0].end.is_none());
        assert!(!r.result.recovery);
    }

    #[test]
    fn recovery_is_measured_in_speech_seconds_and_credits_the_reanchor() {
        let gt_lines: Vec<GroundTruthLine> = (0..10)
            .map(|i| gt(&format!("L-{:04}", i + 1), i as f64, i as f64 + 1.0))
            .collect();
        let mut trace = vec![pos(0.0, 0, Confidence::Lost)];
        let mut back = pos(5.0, 5, Confidence::Line);
        back.kind = TraceKind::Reanchor;
        trace.push(back);
        // ...and it keeps up from there, or the tail of the run would (rightly)
        // register as a second, confidently-wrong outage.
        trace.extend((6..10).map(|i| pos(i as f64, i, Confidence::Line)));
        let (r, _) = evaluate(&gt_lines, &trace, &[], &index_map(10), Gate::default());
        assert_eq!(r.outages.len(), 1);
        let o = &r.outages[0];
        assert_eq!(o.end, Some(5.0));
        assert!((o.recovery_speech_s - 5.0).abs() < 1e-9, "{o:?}");
        assert!(o.closed_by_reanchor);
        assert!(r.result.recovery, "5 s is well inside the 30 s gate");
    }

    #[test]
    fn a_line_the_tracker_never_reaches_is_counted_as_undetected() {
        let trace = vec![pos(0.0, 0, Confidence::Word), pos(1.0, 1, Confidence::Word)];
        let (r, _) = evaluate(&four_lines(), &trace, &[], &index_map(4), Gate::default());
        assert_eq!(r.lag.undetected_count, 2);
        assert_eq!(r.lag.total_lines, 4);
    }

    #[test]
    fn skipping_ahead_counts_as_early_not_as_negative_lag() {
        let trace = vec![pos(0.0, 3, Confidence::Line)];
        let (r, _) = evaluate(&four_lines(), &trace, &[], &index_map(4), Gate::default());
        assert_eq!(
            r.lag.early_count, 4,
            "every line was already passed when spoken"
        );
        assert_eq!(r.lag.detection_lag.count, 0);
    }

    #[test]
    fn silence_between_lines_is_excluded_from_the_denominator() {
        // Two lines a minute apart: only 2 s of speech-active time exists.
        let gt_lines = vec![gt("L-0001", 0.0, 1.0), gt("L-0002", 60.0, 61.0)];
        let trace = vec![
            pos(0.0, 0, Confidence::Word),
            pos(60.0, 1, Confidence::Word),
        ];
        let (r, _) = evaluate(&gt_lines, &trace, &[], &index_map(2), Gate::default());
        assert!((r.coverage.speech_active_s - 2.0).abs() < 1e-9);
        assert!(
            (r.coverage.exact - 1.0).abs() < 1e-9,
            "the minute of silence must not count"
        );
    }

    #[test]
    fn overlapping_lines_resolve_to_the_latest_onset() {
        // Two actors speaking over each other.
        let gt_lines = vec![gt("L-0001", 0.0, 3.0), gt("L-0002", 1.0, 4.0)];
        let trace = vec![pos(0.0, 0, Confidence::Word), pos(1.0, 1, Confidence::Word)];
        let (r, _) = evaluate(&gt_lines, &trace, &[], &index_map(2), Gate::default());
        assert!(
            (r.coverage.speech_active_s - 4.0).abs() < 1e-9,
            "union, not sum"
        );
        assert!((r.coverage.exact - 1.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_ground_truth_lines_are_reported_not_swallowed() {
        let gt_lines = vec![gt("L-0001", 0.0, 1.0), gt("L-9999", 1.0, 2.0)];
        let trace = vec![pos(0.0, 0, Confidence::Word)];
        let (r, unknown) = evaluate(&gt_lines, &trace, &[], &index_map(1), Gate::default());
        assert_eq!(unknown, 1);
        assert!((r.coverage.speech_active_s - 1.0).abs() < 1e-9);
    }

    #[test]
    fn distribution_percentiles_use_nearest_rank() {
        let d = Distribution::of(vec![1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(d.count, 5);
        assert_eq!(d.median, 3.0);
        assert_eq!(d.p95, 5.0);
        assert_eq!(d.max, 5.0);
        assert_eq!(d.mean, 3.0);
        assert_eq!(Distribution::of(vec![]).count, 0);
    }

    #[test]
    fn latency_gate_is_absent_rather_than_passing_when_unmeasured() {
        let (r, _) = evaluate(
            &four_lines(),
            &late_trace(4, 0.2),
            &[],
            &index_map(4),
            Gate::default(),
        );
        assert_eq!(
            r.result.latency, None,
            "a batch run has nothing to say about latency"
        );
        assert!(r.result.passed, "and must not be failed for it");
    }
}
