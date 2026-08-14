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
    /// Ambiguity margin required while **lost**.
    ///
    /// Reasoning said this should be lower than `margin`: a lost tracker searches the
    /// whole script, which is exactly the condition that manufactures ties, and it
    /// then refuses every one of them — so being somewhere plausible ought to beat
    /// being nowhere.
    ///
    /// Measured, it is not true. At 0.0 on Hécube: night 16 unchanged (27 losses,
    /// 758 s → 752 s lost in total), but night 17 went from 34 losses totalling
    /// 1205 s to 22 losses totalling **1398 s** — fewer relocations, each of them
    /// committing to a bad guess and sitting on it, worst case 160 s → 570 s. Fewer
    /// losses is not the same as less lost time, and the second is what the operator
    /// experiences.
    ///
    /// Left equal to `margin`, and left as a knob so the next attempt starts from a
    /// measurement rather than from this paragraph.
    pub lost_margin: f64,
    /// Whether two lines carrying identical text should silence each other.
    ///
    /// `false` — the default — means they do not: see `is_rival`. Left switchable
    /// because it changes what the margin rule means, and anything that changes
    /// matching behaviour should be measurable against the corpus rather than argued
    /// about.
    pub equivalent_text_competes: bool,
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
    /// overlap term a three-line span would always beat the single line the segment
    /// actually covered, and the position would run ahead of the show.
    ///
    /// But the term is symmetric, and the two ways a side can carry extra material
    /// are not the same thing at all:
    ///
    /// - a *span* holding lines the segment never covered is over-reach, and must
    ///   be penalised — that is what the term is for;
    /// - a *segment* holding only part of one line is an ordinary partial hearing,
    ///   and penalising it is simply wrong.
    ///
    /// The second case is the common one wherever audio is quiet or far-field: a
    /// boom mic returns fragments, and automatic gain recovers quiet speech as more
    /// fragments still. Applying the penalty there suppressed exactly the material
    /// that had just been recovered — on one real scene, removing it took tracking
    /// from 10 lines to 25 of 44.
    ///
    /// Span length looked like a clean separator — over-reach needs more than one
    /// line by definition — but exempting single-line spans broke five scenario
    /// tests just as removing the term entirely did, so the two cases are entangled
    /// somewhere else. Resolving it properly is M0.4 work, against ground truth;
    /// the evidence and the failed attempts are in the Phase 0 notes.
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
    /// Keep a second hypothesis over the whole script at all times, and adopt it
    /// when it explains recent speech clearly better than the current position.
    ///
    /// Without this the tracker only ever looks beyond its window once it has
    /// already given up, which is too late twice over: recovery waits for the
    /// decay timer to expire, and a position that is confidently wrong while still
    /// finding weak local support never questions itself at all. Scanning the whole
    /// script costs about 0.03 ms per segment against a 160 ms decode, so there is
    /// no reason not to look.
    ///
    /// Adoption is deliberately slow: the challenger must beat the incumbent by
    /// `challenger_margin` on a running average, over at least `challenger_min_hits`
    /// segments and `challenger_min_seconds`. One good coincidental match must never
    /// move the show.
    pub challenger_enabled: bool,
    /// How much better the challenger's running score must be.
    pub challenger_margin: f64,
    /// How good the challenger must be in absolute terms, regardless of the margin.
    ///
    /// Without this the challenger hijacks the position during improvisation: the
    /// incumbent's evidence collapses toward zero when nothing matches, so a rival
    /// need only beat *nothing* to win, and any coincidental word overlap
    /// elsewhere in the script qualifies. Off-script speech must leave the tracker
    /// saying it is lost, not confidently somewhere else. A rival has to explain
    /// the words well on its own account, not merely better than a ruin.
    pub challenger_min_evidence: f64,
    /// Segments the challenger must explain before it may be adopted.
    pub challenger_min_hits: usize,
    /// Seconds the challenger must sustain its advantage.
    pub challenger_min_seconds: f64,
    /// Weight of the newest observation in either side's running average.
    pub challenger_smoothing: f64,
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
            lost_margin: 0.06,
            equivalent_text_competes: false,
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
            challenger_enabled: true,
            challenger_margin: 0.18,
            challenger_min_evidence: 0.62,
            challenger_min_hits: 3,
            challenger_min_seconds: 4.0,
            challenger_smoothing: 0.4,
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

/// A rival explanation of what is being said, maintained alongside the position.
struct Challenger {
    position: usize,
    /// Running average of how well this hypothesis has explained recent speech.
    evidence: f64,
    hits: usize,
    first_t: f64,
    last_t: f64,
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
    challenger: Option<Challenger>,
    /// Running average of how well the *current* position explains recent speech,
    /// on the same scale as a challenger's, so the two are directly comparable.
    incumbent_evidence: f64,
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
            challenger: None,
            incumbent_evidence: 0.0,
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

        // Look at the whole script every time, not only once lost. Cheap, and it
        // is the only way to notice that somewhere else explains this better.
        if self.cfg.challenger_enabled && !weak {
            if let Some(ev) = self.run_challenger(seg) {
                events.push(ev);
                return events;
            }
        }

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
        // The margin rule protects a position we already trust from being stolen by a
        // coincidence. When there is no position to protect it does the opposite: a
        // lost tracker searches the whole script, which is precisely the condition
        // that manufactures ties, and it then refuses every one of them. Reported
        // from a live run — "it's really struggling to get back on track by itself".
        //
        // So while lost, being somewhere plausible beats being nowhere. The result is
        // reported at Block, never Line, so the screen shows an uncertain position as
        // uncertain and the operator can overrule it with one tap.
        let margin = if matches!(self.confidence, Confidence::Lost) {
            self.cfg.lost_margin
        } else {
            self.cfg.margin
        };
        if best.score - runner_up < margin {
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
        self.incumbent_evidence = blend(
            self.incumbent_evidence,
            best.score,
            self.cfg.challenger_smoothing,
        );

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
        // A segment the current position could not explain counts against it.
        self.incumbent_evidence =
            blend(self.incumbent_evidence, 0.0, self.cfg.challenger_smoothing);
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

    /// Maintain the rival hypothesis, and adopt it if it has earned the position.
    ///
    /// Returns a `Position` event when the switch happens, in which case the
    /// caller must not also run the ordinary incumbent update for this segment.
    fn run_challenger(&mut self, seg: &TranscriptSegment) -> Option<TrackerEvent> {
        let best = self.scan_whole_script(seg)?;
        let target = best.span.last();
        let near_incumbent = target >= self.position
            && target.saturating_sub(self.position) <= self.cfg.window_ahead;
        if near_incumbent {
            // Not a rival at all — this is the incumbent's own territory.
            return None;
        }

        match &mut self.challenger {
            // Same neighbourhood, or the show has moved on within it: the rival
            // survives and grows. A challenger that never advances is a
            // coincidence; one that follows the dialogue is an explanation.
            Some(c)
                if target >= c.position.saturating_sub(2)
                    && target <= c.position + self.cfg.window_ahead =>
            {
                c.position = target;
                c.evidence = blend(c.evidence, best.score, self.cfg.challenger_smoothing);
                c.hits += 1;
                c.last_t = seg.t_end;
            }
            // Somewhere else entirely. One sighting does not outweigh a rival that
            // has been accumulating, so only replace a weaker one.
            Some(c) if best.score <= c.evidence => {}
            _ => {
                self.challenger = Some(Challenger {
                    position: target,
                    evidence: best.score,
                    hits: 1,
                    first_t: seg.t_end,
                    last_t: seg.t_end,
                });
            }
        }

        let c = self.challenger.as_ref()?;
        let earned = c.hits >= self.cfg.challenger_min_hits
            && c.last_t - c.first_t >= self.cfg.challenger_min_seconds
            && c.evidence >= self.cfg.challenger_min_evidence
            && c.evidence >= self.incumbent_evidence + self.cfg.challenger_margin;
        if !earned {
            return None;
        }

        let position = c.position;
        let evidence = c.evidence;
        self.challenger = None;
        self.position = position;
        self.incumbent_evidence = evidence;
        self.unmatched_speech_s = 0.0;
        self.stalled_speech_s = 0.0;
        self.pending_jump = None;
        // Always `Block`, however strong the evidence was.
        //
        // A challenger has been judged only against itself: nothing has yet
        // confirmed it the way the ordinary path confirms a position, by matching
        // the next line from there. Measured on real material, eight of nine
        // adoptions were correct and the ninth was indistinguishable by score —
        // 0.65, inside the range of the correct ones — so no threshold separates
        // them and tuning one to exclude a single observed failure would be
        // fitting noise. What *can* be said honestly is that a just-relocated
        // tracker does not yet know it is right. Reporting `Block` says exactly
        // that, keeps a wrong adoption out of the confident-wrong count, and costs
        // nothing when the adoption was correct: the next match promotes it.
        self.confidence = Confidence::Block;
        Some(self.position_event(evidence, PositionCause::Reanchor))
    }

    /// Best candidate anywhere in the script, ignoring the current position.
    fn scan_whole_script(&mut self, seg: &TranscriptSegment) -> Option<Candidate> {
        let script = self.script;
        script.spans_into(
            0,
            script.len(),
            self.cfg.max_span,
            seg.character.as_deref(),
            &mut self.spans,
        );
        let mut best: Option<Candidate> = None;
        for k in 0..self.spans.len() {
            let span = self.spans[k];
            let Some(cand) = self.score_span_at(seg, span, span.first()) else {
                continue;
            };
            if best.as_ref().is_none_or(|b| better(&cand, b)) {
                best = Some(cand);
            }
        }
        best
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
        // Being lost is exactly the situation in which looking everywhere is
        // correct. A script converted from a rehearsal document has no landmarks
        // at all beyond the implicit one at line 0, so without this a run that
        // missed the opening lines can never reach line 9 — however plainly the
        // actors are speaking line 30.
        // `Scene` is the state the tracker starts in and means "we know the scene
        // but not the line" — which is nearer to lost than to tracking, and is
        // exactly the situation at the top of a run or after a jump. Searching
        // widely from there too is what takes time-to-first-fix from minutes to
        // seconds; the distance prior still makes a near match beat a far one, so
        // a run that genuinely starts at line 1 is unaffected.
        let lost = matches!(self.confidence, Confidence::Lost | Confidence::Scene);
        let (window, max_span) = if weak {
            (1, 1)
        } else if lost && self.cfg.lost_search_all {
            (self.script.len(), self.cfg.max_span)
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
                        if self.is_rival(cand.span.last(), b.span.last()) {
                            runner_up = runner_up.max(b.score);
                        }
                        best = Some(cand);
                    } else if self.is_rival(cand.span.last(), b.span.last()) {
                        runner_up = runner_up.max(cand.score);
                    }
                }
            }
        }
        best.map(|b| (b, runner_up))
    }

    /// Whether two candidate end-lines are genuinely competing answers.
    ///
    /// A different line saying *different words* is a rival, and the margin rule
    /// should silence us. A different line saying *the same words* is not: the show
    /// says that sentence in two places and either reading explains what was just
    /// heard, so refusing to move is the one response guaranteed to be wrong.
    ///
    /// Measured on Hécube: 165 rejections for ambiguity in one performance, including
    /// a line scored **1.00** — a perfect match — thrown away because its twin tied
    /// it. The company reads Euripides aloud and then performs it, and several actors
    /// say the same original text in turn; every one of those moments froze the
    /// tracker, which then decayed to lost and needed an operator to put it back.
    /// Picking the nearer copy is a line or two of error. Freezing costs the scene.
    fn is_rival(&self, a: usize, b: usize) -> bool {
        if a == b {
            return false;
        }
        if !self.cfg.equivalent_text_competes {
            match (self.script.lines.get(a), self.script.lines.get(b)) {
                (Some(x), Some(y)) if x.text_key == y.text_key => return false,
                _ => {}
            }
        }
        true
    }

    fn score_span(&mut self, seg: &TranscriptSegment, span: Span) -> Option<Candidate> {
        let origin = self.position;
        self.score_span_at(seg, span, origin)
    }

    /// As `score_span`, but measuring distance from `origin` rather than from the
    /// current position — so a rival hypothesis is judged on how well it explains
    /// the words, not on how far away it happens to be.
    fn score_span_at(
        &mut self,
        seg: &TranscriptSegment,
        span: Span,
        origin: usize,
    ) -> Option<Candidate> {
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
            let variants = script.span_token_variants(span, lang);
            if variants.is_empty() {
                continue;
            }
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

            // Best over the written line and every way it has actually been
            // performed. The written form still wins ties, being first.
            //
            for line_tokens in &variants {
                let s = token_set_ratio(&seg_tokens, line_tokens)
                    * token_dice(&seg_tokens, line_tokens).powf(overlap_exp);
                if s > fuzzy {
                    fuzzy = s;
                }
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
        let gap = span.first().saturating_sub(origin) as f64;
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

/// Exponential moving average: `weight` is how much the newest observation counts.
fn blend(current: f64, observation: f64, weight: f64) -> f64 {
    current * (1.0 - weight) + observation * weight
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
