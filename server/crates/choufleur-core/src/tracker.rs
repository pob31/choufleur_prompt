//! Position tracker v0.
//!
//! Maintains a single script position with an honest confidence level, updated
//! from transcript segments. The governing principle from the PRD is
//! *uncertain is better than wrong*: every rule here prefers admitting ignorance
//! to advancing on thin evidence, and every rejection is reported so the eval
//! harness can explain the tracker's silence as readily as its motion.
//!
//! Determinism is a hard requirement — the same segment sequence must produce a
//! byte-identical event sequence. Hence: no clock (time arrives on segments), no
//! RNG, no iteration over hash maps in any decision, and a total order on scores
//! with an explicit tie-break.

use serde::{Deserialize, Serialize};

use crate::lang::{LangCode, MatchText, NormalizerRegistry};
use crate::matcher::{token_coverage, token_dice, token_set_ratio};
use crate::normalize::{normalize_base, tokens};
use crate::script::{PreparedScript, Span};
use crate::types::TranscriptSegment;

/// Tracking levels, ordered by *precision* — see the PRD's tracking-levels table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Tracking lost; the position shown is stale and must be labelled as such.
    Lost,
    /// We know the scene but not the line — the state the tracker starts in.
    Scene,
    /// "Somewhere in this exchange": position held, waiting for a landmark.
    Block,
    /// Fuzzy semantic match on the line.
    Line,
    /// Near-exact transcript match.
    Word,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionCause {
    /// Ordinary advance to the matched line (gap of 0 or 1).
    Follow,
    /// Skip tolerance: material in between was never heard, but later material was.
    Skip,
    /// Re-anchored on a landmark after drifting.
    Reanchor,
    /// A large forward jump, confirmed by a second agreeing match.
    Jump,
    /// Set from outside the tracker (operator correction, run control, journal restore).
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// No usable text at all.
    Empty,
    /// A one- or two-word segment that did not confirm the very next line. Short
    /// utterances are weak evidence: they may confirm where we already think we
    /// are, but they may not move the position far and they are never divergence.
    WeakEvidence,
    /// Nothing in the candidate window scored above the acceptance threshold.
    BelowThreshold,
    /// Two candidates scored too close together — a repeated line, most likely.
    Ambiguous,
    /// A large jump seen once; held pending a second, agreeing match.
    JumpPending,
    /// Position is at the end of the script; nothing lies ahead.
    NoCandidates,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TrackerEvent {
    Position {
        line_index: usize,
        line_id: String,
        confidence: Confidence,
        score: f64,
        cause: PositionCause,
    },
    Rejected {
        reason: RejectReason,
        best_score: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        best_index: Option<usize>,
    },
    ConfidenceChanged {
        from: Confidence,
        to: Confidence,
        /// Seconds of *speech* seen without a match — silence never decays.
        unmatched_speech_s: f64,
    },
}

/// Tunable knobs. Everything marked **[sweep]** is a dial M0.4 turns; the defaults
/// are starting points chosen by reasoning, not by measurement, and are expected
/// to move once the eval runs against real recordings.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TrackerConfig {
    /// **[sweep]** How many lines ahead of the current position may start a span.
    pub window_ahead: usize,
    /// Maximum script lines one segment may cover.
    pub max_span: usize,
    /// **[sweep]** Minimum score to move the position at all, at `Block`
    /// confidence — "somewhere in this exchange".
    ///
    /// Deliberately lower than `accept_threshold`. What an operator needs is a
    /// page that keeps pace with the show, not a line number that is either
    /// perfect or frozen: they read the page, and a position drifting a few lines
    /// behind is far more useful than one that stopped two minutes ago. Freezing
    /// is not the safe option it looks like — a stale position is silently wrong,
    /// which is the failure the PRD actually warns about.
    ///
    /// Honesty is preserved by reporting `Block` rather than by refusing to move:
    /// the confidence says how much to trust the line number, and the position
    /// still says roughly where the show is.
    pub follow_threshold: f64,
    /// **[sweep]** Score at or above which the match is good enough to call `Line`.
    pub accept_threshold: f64,
    /// **[sweep]** Score at or above which confidence is `Word` rather than `Line`.
    pub word_threshold: f64,
    /// **[sweep]** How far the best candidate must beat the runner-up.
    pub margin: f64,
    /// **[sweep]** Multiplier when a segment's channel character is not the
    /// candidate line's speaker.
    pub char_mismatch_penalty: f64,
    /// Multiplier for a zone (identity-free) channel. 1.0 = no penalty; a zone
    /// mic is not *wrong* about who spoke, it simply says nothing.
    pub zone_factor: f64,
    /// **[sweep]** Lines of distance at which the distance prior halves.
    pub distance_prior_halflife: f64,
    /// Floor on the distance prior, so a distant-but-perfect match can still win.
    pub prior_floor: f64,
    /// **[sweep]** Exponent on the token-overlap (Dice) term.
    ///
    /// Token-set similarity alone scores a subset almost perfectly, so without an
    /// overlap term a three-line span would always beat the single line the
    /// segment actually covered, and the position would run ahead of the show.
    /// 0 disables the correction; 1 weights overlap as heavily as similarity.
    pub overlap_exp: f64,
    /// **[sweep]** Fraction of a line's own tokens that must appear in the segment
    /// before a multi-line span may claim that line was heard.
    pub member_coverage_min: f64,
    /// **[sweep]** Score multiplier per landmark weight 1, 2, 3.
    pub landmark_boost: [f64; 3],
    /// **[sweep]** How far ahead landmarks stay live as re-anchoring candidates.
    pub reanchor_horizon: usize,
    /// **[sweep]** Score a landmark span must reach to re-anchor across a big gap.
    pub reanchor_threshold: f64,
    /// Largest gap accepted without corroboration (skip tolerance).
    pub skip_max: usize,
    /// **[sweep]** Seconds of unmatched speech before confidence drops to `Block`.
    pub decay_to_block_s: f64,
    /// **[sweep]** Seconds of unmatched speech before tracking is declared `Lost`.
    pub decay_to_lost_s: f64,
    /// Segments with fewer tokens than this are treated as **weak evidence**:
    /// matched only against the immediately following line, at a higher bar, and
    /// never counted as divergence when they fail.
    ///
    /// Scripts are full of one-word lines ("Oui.", "No.") and discarding them
    /// outright costs real coverage — the tracker sits a line behind through the
    /// whole of the next speech. Believing them outright is worse: "yes" appears
    /// a dozen times in any act.
    pub min_segment_tokens: usize,
    /// **[sweep]** The higher bar a weak-evidence segment must clear.
    pub short_accept_threshold: f64,
    /// Seconds of unmatched speech after which an unconfirmed jump is forgotten.
    pub jump_pending_ttl_s: f64,
    /// **[sweep]** Seconds of speech during which the position never advanced
    /// before tracking is declared `Lost`, however much of it appeared to match.
    ///
    /// Matching something is not the same as making progress. In a scene of cross
    /// talk the tracker keeps finding weak support for lines near where it already
    /// is, which resets the unmatched-speech timer forever — so it can sit on line
    /// four for seventeen minutes at line confidence and never once admit it is
    /// lost. That is the worst available outcome: a frozen page with no warning,
    /// and no help request, in a scene where the operator most needs one.
    ///
    /// Generous, because a monologue legitimately holds one line for a long time —
    /// the La Reprise documentary monologue has 25-second lines.
    pub stall_to_lost_s: f64,
    /// When tracking is `Lost`, search the entire script rather than the window.
    ///
    /// Without this the tracker can only ever see `window_ahead` lines forward
    /// plus whatever landmarks exist — and a script converted from a rehearsal
    /// document has no landmarks at all, only the implicit one at line 0. A run
    /// that failed to catch the opening lines could then never reach line 9,
    /// however plainly the actors were speaking line 30. Being lost is exactly the
    /// situation in which looking everywhere is the right thing to do.
    pub lost_search_all: bool,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        TrackerConfig {
            window_ahead: 8,
            max_span: 3,
            follow_threshold: 0.45,
            accept_threshold: 0.62,
            word_threshold: 0.88,
            margin: 0.06,
            char_mismatch_penalty: 0.35,
            zone_factor: 1.0,
            // Gentle: a perfect match anywhere inside the window must still clear
            // `accept_threshold` on its own (1.0 × 0.70 > 0.62), or the window
            // would be decorative and skip tolerance unreachable.
            distance_prior_halflife: 12.0,
            prior_floor: 0.70,
            overlap_exp: 0.5,
            member_coverage_min: 0.5,
            landmark_boost: [1.05, 1.15, 1.30],
            reanchor_horizon: 40,
            reanchor_threshold: 0.80,
            skip_max: 2,
            decay_to_block_s: 8.0,
            decay_to_lost_s: 20.0,
            min_segment_tokens: 3,
            short_accept_threshold: 0.80,
            jump_pending_ttl_s: 8.0,
            stall_to_lost_s: 90.0,
            lost_search_all: true,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Candidate {
    span: Span,
    /// Raw similarity, before any weighting — the honest number for confidence.
    fuzzy: f64,
    /// Weighted score used for ranking and thresholds. Deliberately *not* clamped
    /// to 1: a landmark boost pushing a perfect match to 1.3 is what lets it win
    /// against a nearer rival, and clamping collapsed exactly the differences the
    /// ambiguity margin exists to read. It is a ranking score, not a probability.
    score: f64,
    is_landmark: bool,
}

struct PendingJump {
    index: usize,
    unmatched_at: f64,
}

/// The tracker. Borrows the prepared script for its lifetime; owns nothing else
/// that could vary between runs.
pub struct Tracker<'a> {
    script: &'a PreparedScript,
    cfg: TrackerConfig,
    reg: NormalizerRegistry,
    position: usize,
    confidence: Confidence,
    unmatched_speech_s: f64,
    /// Speech seen since the position last actually moved forward.
    stalled_speech_s: f64,
    last_match_t: f64,
    pending_jump: Option<PendingJump>,
    // Scratch buffers, reused across updates to keep the hot path allocation-free.
    spans: Vec<Span>,
    landmark_spans: Vec<Span>,
    seg_by_lang: Vec<(LangCode, MatchText)>,
}

impl<'a> Tracker<'a> {
    pub fn new(script: &'a PreparedScript, cfg: TrackerConfig) -> Self {
        Tracker {
            script,
            cfg,
            reg: NormalizerRegistry::with_defaults(),
            position: 0,
            confidence: Confidence::Scene,
            unmatched_speech_s: 0.0,
            stalled_speech_s: 0.0,
            last_match_t: 0.0,
            pending_jump: None,
            spans: Vec::new(),
            landmark_spans: Vec::new(),
            seg_by_lang: Vec::new(),
        }
    }

    pub fn position(&self) -> usize {
        self.position
    }
    pub fn confidence(&self) -> Confidence {
        self.confidence
    }
    pub fn config(&self) -> &TrackerConfig {
        &self.cfg
    }
    pub fn line_id(&self) -> Option<&str> {
        self.script.lines.get(self.position).map(|l| l.id.as_str())
    }

    /// Seconds of *new* speech a segment represents.
    ///
    /// Interim hypotheses are prefixes of the segment that follows them, so their
    /// durations overlap heavily — a 25-second line emitting a partial every 1.5 s
    /// sums to several times its own length. Counting them would run every decay
    /// timer at some multiple of real time and declare tracking lost, and raise a
    /// help request, in the middle of a passage that was tracking perfectly well.
    /// Final segments tile the speech exactly once, so only they are counted.
    /// Move the position from outside the matcher — an operator correction, a run
    /// control jump, or a journal restore. Confidence is not invented: the caller
    /// says what it knows.
    pub fn set_position(&mut self, index: usize, confidence: Confidence) -> Vec<TrackerEvent> {
        self.position = index.min(self.script.len().saturating_sub(1));
        self.confidence = confidence;
        self.unmatched_speech_s = 0.0;
        self.pending_jump = None;
        vec![self.position_event(1.0, PositionCause::Manual)]
    }

    /// Feed one transcript segment. Returns the events it caused, in order.
    pub fn update(&mut self, seg: &TranscriptSegment) -> Vec<TrackerEvent> {
        let mut events = Vec::new();

        let base = normalize_base(&seg.text);
        let token_count = tokens(&base).count();
        if token_count == 0 {
            events.push(TrackerEvent::Rejected {
                reason: RejectReason::Empty,
                best_score: 0.0,
                best_index: None,
            });
            return events;
        }
        let weak = token_count < self.cfg.min_segment_tokens;
        // Weak evidence keeps its higher bar; everything else may move the
        // position on `follow_threshold` and report the lower confidence honestly.
        let move_threshold = if weak {
            self.cfg.short_accept_threshold
        } else {
            self.cfg.follow_threshold
        };

        let Some(best) = self.best_candidate(seg, weak) else {
            events.push(TrackerEvent::Rejected {
                reason: if weak {
                    RejectReason::WeakEvidence
                } else {
                    RejectReason::NoCandidates
                },
                best_score: 0.0,
                best_index: None,
            });
            if !weak {
                self.on_unmatched(seg, &mut events);
            }
            return events;
        };
        let (best, runner_up) = best;

        if best.score < move_threshold {
            events.push(TrackerEvent::Rejected {
                reason: if weak {
                    RejectReason::WeakEvidence
                } else {
                    RejectReason::BelowThreshold
                },
                best_score: best.score,
                best_index: Some(best.span.first()),
            });
            if !weak {
                self.on_unmatched(seg, &mut events);
            }
            return events;
        }
        if best.score - runner_up < self.cfg.margin {
            // Two places in the script fit equally well. Saying nothing is the
            // whole point of the margin rule — see "Yes." twelve times over.
            events.push(TrackerEvent::Rejected {
                reason: if weak {
                    RejectReason::WeakEvidence
                } else {
                    RejectReason::Ambiguous
                },
                best_score: best.score,
                best_index: Some(best.span.first()),
            });
            if !weak {
                self.on_unmatched(seg, &mut events);
            }
            return events;
        }
        let gap = best.span.first() - self.position;
        let cause = if gap <= 1 {
            PositionCause::Follow
        } else if gap <= self.cfg.skip_max {
            PositionCause::Skip
        } else if best.score >= self.cfg.reanchor_threshold
            && (best.is_landmark || self.confidence == Confidence::Lost)
        {
            // A landmark anchors at any time. When already lost, any sufficiently
            // strong and unambiguous match anchors too — there is nothing left to
            // protect, and refusing to move is how a run stays lost for an act.
            PositionCause::Reanchor
        } else {
            // A big unanchored jump. Believe it only on the second sighting.
            match self.pending_jump.take() {
                Some(p) if best.span.first().abs_diff(p.index) <= 1 => PositionCause::Jump,
                _ => {
                    self.pending_jump = Some(PendingJump {
                        index: best.span.first(),
                        unmatched_at: self.unmatched_speech_s,
                    });
                    events.push(TrackerEvent::Rejected {
                        reason: RejectReason::JumpPending,
                        best_score: best.score,
                        best_index: Some(best.span.first()),
                    });
                    // Refusing to follow a single distant match is right; going on
                    // *asserting* the old position is not. Something convincing was
                    // heard somewhere else, so what we hold is now stale — say so
                    // immediately rather than after the decay timer. This is where
                    // confident-wrong events come from otherwise.
                    self.demote(Confidence::Block, &mut events);
                    self.on_unmatched(seg, &mut events);
                    return events;
                }
            }
        };

        self.accept(seg, &best, cause, weak, &mut events);
        events
    }

    fn accept(
        &mut self,
        seg: &TranscriptSegment,
        best: &Candidate,
        cause: PositionCause,
        weak: bool,
        events: &mut Vec<TrackerEvent>,
    ) {
        let from = self.confidence;
        let advanced = best.span.last() > self.position;
        self.position = best.span.last();
        if advanced {
            self.stalled_speech_s = 0.0;
        } else {
            self.stalled_speech_s += speech_seconds(seg);
        }
        self.confidence = if best.score < self.cfg.accept_threshold {
            // Moved on thin evidence: the page keeps pace, the operator is told
            // this is "somewhere around here" rather than a line number to trust.
            Confidence::Block
        } else if best.fuzzy >= self.cfg.word_threshold && !weak && !seg.interim {
            Confidence::Word
        } else {
            // Two ways to be placed without a word-level match in the PRD's sense:
            // a one-word confirmation, where there was almost nothing to match; and
            // an interim hypothesis, which is a *prefix* of the line rather than the
            // line. Both locate the show; neither is an exact transcript match.
            Confidence::Line
        };
        self.unmatched_speech_s = 0.0;
        self.last_match_t = seg.t_end;
        self.pending_jump = None;

        if self.confidence != from {
            events.push(TrackerEvent::ConfidenceChanged {
                from,
                to: self.confidence,
                unmatched_speech_s: 0.0,
            });
        }
        events.push(self.position_event(best.score, cause));
    }

    fn position_event(&self, score: f64, cause: PositionCause) -> TrackerEvent {
        TrackerEvent::Position {
            line_index: self.position,
            line_id: self
                .script
                .lines
                .get(self.position)
                .map(|l| l.id.clone())
                .unwrap_or_default(),
            confidence: self.confidence,
            score,
            cause,
        }
    }

    /// Confidence decays on *speech* the tracker could not place. A long pause is
    /// not evidence of anything, so silence never demotes.
    fn on_unmatched(&mut self, seg: &TranscriptSegment, events: &mut Vec<TrackerEvent>) {
        let dt = speech_seconds(seg);
        self.unmatched_speech_s += dt;
        self.stalled_speech_s += dt;
        if let Some(p) = &self.pending_jump {
            if self.unmatched_speech_s - p.unmatched_at > self.cfg.jump_pending_ttl_s {
                self.pending_jump = None;
            }
        }
        let target = if self.unmatched_speech_s >= self.cfg.decay_to_lost_s
            || self.stalled_speech_s >= self.cfg.stall_to_lost_s
        {
            Confidence::Lost
        } else if self.unmatched_speech_s >= self.cfg.decay_to_block_s {
            Confidence::Block
        } else {
            return;
        };
        self.demote(target, events);
    }

    /// Lower confidence to `target` if it is currently higher. Never raises —
    /// confidence is earned by a match, not by the absence of contradiction.
    fn demote(&mut self, target: Confidence, events: &mut Vec<TrackerEvent>) {
        if target < self.confidence {
            let from = self.confidence;
            self.confidence = target;
            events.push(TrackerEvent::ConfidenceChanged {
                from,
                to: target,
                unmatched_speech_s: self.unmatched_speech_s,
            });
        }
    }

    /// Score every candidate span; return the best and the best score belonging to
    /// a *different resulting position* — the runner-up the margin rule tests
    /// against. Grouping by resulting line and not by span start matters: the span
    /// `[4]` and the span `[2, 4]` both say "we are at line 4", so they corroborate
    /// each other rather than competing, and treating them as rivals would make
    /// the tracker fall silent exactly when it is most certain.
    fn best_candidate(&mut self, seg: &TranscriptSegment, weak: bool) -> Option<(Candidate, f64)> {
        // Weak evidence gets a narrow view of the world: the next line only, no
        // multi-line spans, no landmark re-anchoring. "Yes" is not allowed to
        // relocate the show.
        let (window, max_span) = if weak {
            (1, 1)
        } else {
            (self.cfg.window_ahead, self.cfg.max_span)
        };
        self.script.spans_into(
            self.position,
            window,
            max_span,
            seg.character.as_deref(),
            &mut self.spans,
        );
        if weak {
            self.landmark_spans.clear();
        } else {
            self.script.landmark_spans_into(
                self.position,
                self.cfg.reanchor_horizon,
                &mut self.landmark_spans,
            );
        }
        if self.spans.is_empty() && self.landmark_spans.is_empty() {
            return None;
        }
        self.seg_by_lang.clear();

        let mut best: Option<Candidate> = None;
        let mut runner_up = 0.0f64;
        // Index-based loops: the scratch buffers are borrowed from `self` and the
        // scorer needs `&mut self` for the normalizer cache.
        for k in 0..(self.spans.len() + self.landmark_spans.len()) {
            let span = if k < self.spans.len() {
                self.spans[k]
            } else {
                self.landmark_spans[k - self.spans.len()]
            };
            let Some(cand) = self.score_span(seg, span) else {
                continue;
            };
            match &best {
                None => best = Some(cand),
                Some(b) => {
                    if better(&cand, b) {
                        if cand.span.last() != b.span.last() {
                            runner_up = runner_up.max(b.score);
                        }
                        best = Some(cand);
                    } else if cand.span.last() != b.span.last() {
                        runner_up = runner_up.max(cand.score);
                    }
                }
            }
        }
        best.map(|b| (b, runner_up))
    }

    fn score_span(&mut self, seg: &TranscriptSegment, span: Span) -> Option<Candidate> {
        // Bind the script reference out of `self` so the token slices it hands
        // back live for `'a` and do not collide with the normalizer cache's
        // mutable borrow below.
        let script = self.script;
        let overlap_exp = self.cfg.overlap_exp;
        let member_coverage_min = self.cfg.member_coverage_min;
        let span_langs = script.span_langs(span);
        // Match in the language the script says this line is in. Where the decode
        // was forced to one of them, restrict to that; a mismatch means the
        // channel's default language differed from the line's, and comparing under
        // both normalizers is better than comparing under neither.
        let mut fuzzy = 0.0f64;
        for lang in &span_langs {
            if !seg.langs.is_empty() && !seg.langs.contains(lang) && span_langs.len() > 1 {
                continue;
            }
            let Some(line_tokens) = script.span_tokens(span, lang) else {
                continue;
            };
            let seg_tokens = self.prepared_segment(seg, lang);

            // Every line a multi-line span claims must be independently audible in
            // the segment. A span is a statement that all of this was just spoken;
            // it should not be believed on the strength of one member alone.
            if span.len() > 1 {
                let all_heard = span.iter().all(|i| {
                    script.line_tokens(i, lang).is_none_or(|member| {
                        token_coverage(&member, &seg_tokens) >= member_coverage_min
                    })
                });
                if !all_heard {
                    continue;
                }
            }

            let s = token_set_ratio(&seg_tokens, &line_tokens)
                * token_dice(&seg_tokens, &line_tokens).powf(overlap_exp);
            if s > fuzzy {
                fuzzy = s;
            }
        }
        if fuzzy <= 0.0 {
            return None;
        }

        let char_factor = match seg.character.as_deref() {
            None => self.cfg.zone_factor,
            Some(c) => {
                if span.iter().all(|i| script.lines[i].character == c) {
                    1.0
                } else {
                    self.cfg.char_mismatch_penalty
                }
            }
        };
        // Boost from the span's *first* line only: a landmark's value is "this
        // distinctive phrase identifies this position", which is a claim about
        // where the span starts, not about material it happens to run over.
        let weight = script.lines[span.first()].landmark_weight;
        let boost = if weight == 0 {
            1.0
        } else {
            self.cfg.landmark_boost[(weight - 1) as usize]
        };
        let gap = span.first().saturating_sub(self.position) as f64;
        let prior = (0.5f64)
            .powf(gap / self.cfg.distance_prior_halflife)
            .max(self.cfg.prior_floor);

        Some(Candidate {
            span,
            fuzzy,
            score: fuzzy * char_factor * boost * prior,
            // A span is a re-anchoring candidate because it *contains* a landmark,
            // not because of which list it was enumerated from.
            is_landmark: weight > 0,
        })
    }

    /// The segment's text prepared under one language's normalizer, memoized for
    /// the duration of this update (a segment is compared against dozens of spans).
    fn prepared_segment(&mut self, seg: &TranscriptSegment, lang: &LangCode) -> Vec<&str> {
        if !self.seg_by_lang.iter().any(|(l, _)| l == lang) {
            let mt = self.reg.prepare(&seg.text, lang);
            self.seg_by_lang.push((lang.clone(), mt));
        }
        let mt = &self.seg_by_lang.iter().find(|(l, _)| l == lang).unwrap().1;
        mt.tokens.iter().map(String::as_str).collect()
    }
}

/// Seconds of *new* speech a segment represents.
///
/// Interim hypotheses are prefixes of the segment that follows them, so their
/// durations overlap heavily — a 25-second line emitting a partial every 1.5 s sums
/// to several times its own length. Counting them would run every decay timer at a
/// multiple of real time, declaring tracking lost and raising a help request in the
/// middle of a passage that was tracking perfectly well. Final segments tile the
/// speech exactly once, so only they count.
fn speech_seconds(seg: &TranscriptSegment) -> f64 {
    if seg.interim {
        0.0
    } else {
        seg.duration()
    }
}

/// Total order on candidates: score, then the earliest start, then the shortest
/// span. Explicit so that equal scores never depend on iteration order.
fn better(a: &Candidate, b: &Candidate) -> bool {
    match a.score.partial_cmp(&b.score) {
        Some(std::cmp::Ordering::Greater) => true,
        Some(std::cmp::Ordering::Less) | None => false,
        Some(std::cmp::Ordering::Equal) => {
            (a.span.first(), a.span.len()) < (b.span.first(), b.span.len())
        }
    }
}
