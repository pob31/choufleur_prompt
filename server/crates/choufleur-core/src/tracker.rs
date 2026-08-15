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
use crate::matcher::{char_trigram_dice, token_coverage, token_dice, token_set_ratio};
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
    /// Score lines on **sound** as well, taking the best of the three.
    ///
    /// The deepest of the three measures and the one that matches what actually went
    /// wrong: every mishearing collected from real performances is a homophone. See
    /// `LangNormalizer::phonetic`.
    pub phonetic_similarity: bool,
    /// How far to trust a sound-only match. Below the character figure because the
    /// folding is coarse and boundary-free, which is permissive by construction.
    pub phonetic_similarity_trust: f64,
    /// Score lines on character trigrams as well as on words, taking the better.
    pub char_similarity: bool,
    /// How far to trust a character-only match, relative to a word match.
    ///
    /// Below 1.0 because characters are the more permissive measure: two unrelated
    /// French sentences share more trigrams than they share words, so an unscaled
    /// character score would raise the floor under every candidate and eat the
    /// ambiguity margin. This is the dial that decides whether the remedy costs more
    /// than the disease, and it is meant to be swept.
    pub char_similarity_trust: f64,
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
    /// How far ahead counts as a jump rather than an advance.
    ///
    /// Retained but **unused** by the threshold rule: charging extra for a long
    /// forward move was measured and rejected. See `jump_threshold`.
    pub jump_gap: usize,
    /// Score a forward move beyond `jump_gap` must reach — **0.0, disabled.**
    ///
    /// It sounded symmetric with `backward_threshold` and it is not. Measured at 0.78
    /// on both nights: night 16 unchanged, night 17 lost 110 lines of coverage and its
    /// time-lost went from 153 s to 1222 s. Skipping ahead is *routine* — a cut, a
    /// dropped exchange, a company running faster than the script — so a toll on
    /// forward travel stops the tracker keeping up with the show, while the same toll
    /// backwards costs nothing because going back is genuinely rare.
    ///
    /// The asymmetry is the finding: distance alone is not what makes a move
    /// suspicious, direction is.
    pub jump_threshold: f64,
    /// Score a match must reach to move the position *backwards*, while not lost.
    ///
    /// Deliberately far above `accept_threshold`. Not applied when lost, because then
    /// there is no position to preserve and every direction is equally unknown.
    pub backward_threshold: f64,
    /// How close two candidates must be before they stop counting as rivals.
    ///
    /// The ambiguity margin protects against landing in the wrong *place*. Within a
    /// few lines there is no wrong place — it is the same page — so the rule only
    /// costs the position it was meant to protect.
    pub rival_min_gap: usize,
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
    /// **[sweep]** Tokens a candidate must *offer* before it may relocate the show.
    ///
    /// The mirror of `min_segment_tokens`, and it was missing. A one- or two-word
    /// segment has long been treated as weak evidence that may confirm the next line
    /// and never move the position — but a one- or two-word *line* was allowed to be
    /// the destination of a jump of any size, and it is the same argument seen from
    /// the other end. "Là." has almost no content to disagree with, so it scores
    /// respectably against anything; Hécube has 213 lines of three tokens or fewer,
    /// sitting across the script like a field of magnets.
    ///
    /// Observed: `Excellences au milieu de la route` moved the show 178 lines onto
    /// `Là.` at exactly the follow threshold. Short lines remain perfectly good to
    /// *follow* onto — that is `skip_max`'s territory, and a show walking onto its own
    /// next line needs no protection from itself.
    pub min_relocate_tokens: usize,
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
    /// **[sweep]** Below this, the best explanation anywhere in the script is not an
    /// explanation, and the segment is treated as saying nothing about where we are.
    ///
    /// A show is not two hours of the script being spoken. It is also music, laughter,
    /// coughing, a held silence the recogniser fills in, a company improvising round a
    /// line, an actor grunting through a fight. All of it arrives as apparent speech
    /// and none of it is evidence about position — yet without this every second of it
    /// counted *against* the current position, because `on_unmatched` decays the
    /// incumbent's evidence and runs the timers towards `Lost`.
    ///
    /// That is what manufactures long jumps. Measured on both Hécube nights, a move of
    /// fifteen lines or more is preceded by twice the unmatched speech of a normal move
    /// (median 16 unplaceable segments in the previous 30 s against 7). The incumbent's
    /// evidence collapses, the whole-script challenger then only has to clear its
    /// absolute floor, and a coincidence four hundred lines away wins by default.
    ///
    /// So the existing rule — "silence never demotes", `on_unmatched` line 1 — looked
    /// right and too narrow: it exempts *quiet*, when what seemed to need exempting is
    /// **anything that isn't the script being spoken**.
    ///
    /// **Measured, and off by default.** On both nights it changes nothing that
    /// matters. At 0.25 and 0.35: identical window accuracy, identical jump counts,
    /// lost time within ten seconds of the baseline. At 0.45 lost time falls sharply
    /// (310 s → 208 s, 277 s → 184 s) with window accuracy *unchanged to the point* —
    /// which is the tell. Nothing was found sooner; the tracker merely stopped
    /// admitting it was lost. Buying a better-looking lost figure by suppressing the
    /// admission is the confidently-wrong failure the whole ladder exists to prevent.
    ///
    /// The diagnosis was right and the remedy was in the wrong place. The damage from
    /// unplaceable audio flows through the *challenger* — a collapsed incumbent lets a
    /// coincidence anywhere in the script clear the bar — so it is fixed there, by
    /// `challenger_extra_hit_lines`, which does work. Kept as a knob because the
    /// reasoning may yet hold on a corpus with more music than this one.
    ///
    /// 0 disables the rule.
    pub noise_floor: f64,
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
    /// **[sweep]** One extra confirming segment is required per this many lines of
    /// travel. 0 disables the scaling and every move costs `challenger_min_hits`.
    ///
    /// A relocation of twenty lines and a relocation of nine hundred are not the same
    /// claim, and until now they cost the same: three agreeing segments over four
    /// seconds. Twenty lines is an ordinary consequence of a cut or a dropped
    /// exchange; nine hundred says the show is somewhere else entirely, which in a
    /// continuous performance is close to impossible. Reported from the chair: "long
    /// jumps should take a few more matches to confirm rather than jump too soon."
    ///
    /// Note this charges *confirmations*, not score — the lesson of the rejected
    /// forward-jump toll (see `jump_threshold`) is that raising the bar on a single
    /// segment stops the tracker keeping up. Asking for more evidence over more time
    /// costs nothing when the move is real; the show keeps talking, and the
    /// challenger keeps being right. That distinction is the whole result: the same
    /// intuition failed as a score toll and succeeds as a confirmation count.
    ///
    /// Swept over 20/30/40/60/80/100/150/250 on both Hécube nights, and monotone
    /// down to 20. At 20 — so a 20-line move wants four agreeing segments, a 100-line
    /// move eight, and anything past 120 hits the ceiling:
    ///
    /// | | night 16 | night 17 |
    /// |---|---|---|
    /// | moves ≥ 100 lines | 14 → **0** | 15 → **0** |
    /// | moves ≥ 15 lines | 29 → 14 | 18 → 5 |
    /// | of those, backwards | 13 → 6 | 8 → 2 |
    /// | 6-line window accuracy | 90 % → 91 % | 92 % → 93 % |
    /// | p90 position error | 4 → 3 lines | 2 → 1 lines |
    /// | time lost | 310 s → 261 s | 277 s → 305 s |
    ///
    /// The long jump is not merely rarer, it is *gone*, and the position is more
    /// accurate rather than less — the cost is 28 s of extra lost time on one night,
    /// which is the honest trade: the tracker now says "recalculating" where it used
    /// to say, confidently, "act four".
    pub challenger_extra_hit_lines: usize,
    /// Ceiling on the scaled requirement, so a distant relocation stays *possible*.
    ///
    /// Swept at 4, 5, 6 and 9 with both doors guarded, and 9 is best on every count —
    /// lowest lost time (398 s against 475–495 s on night 16) *and* no jump over a
    /// hundred lines. Lowering it does not make the tracker more careful, it makes it
    /// relocate on less evidence, land wrong, and get lost again.
    pub challenger_max_hits: usize,
    /// **[sweep]** Lines per extra sighting on the *jump* path specifically.
    ///
    /// Separate from `challenger_extra_hit_lines` because the two doors carry very
    /// different traffic. The challenger only ever proposes somewhere outside the
    /// window, so charging it from the first line costs nothing. The jump path handles
    /// every ordinary overshoot as well — a cut, a dropped exchange, a company running
    /// fast — and charging those at the same rate buys a couple of avoided jumps for
    /// two extra minutes of "recalculating" across a two-hour show, which is the wrong
    /// way round for an operator who complained about being lost, not about drifting.
    ///
    /// So the jump path is charged coarsely: short overshoots stay at two sightings,
    /// and only a genuinely long relocation has to keep proving itself. 0 falls back
    /// to `challenger_extra_hit_lines`.
    pub jump_extra_sighting_lines: usize,
    /// **[sweep]** Exempt a lost tracker from the distance scaling.
    ///
    /// The reasoning was the backward-threshold exemption's: distance is expensive
    /// because it contradicts a position we believe, and while lost there is no such
    /// position to contradict, so charging for it would only lengthen the relocation
    /// the challenger exists to perform.
    ///
    /// **Measured false, and off.** It buys 34 s of lost time on night 16 and brings
    /// back two jumps of over a hundred lines on *each* night, with window accuracy
    /// falling back to 90 % and p90 error to 4 lines. The analogy does not hold: the
    /// backward threshold governs the ordinary path, which while lost has an
    /// alternative, whereas the challenger *is* the relocation mechanism — and being
    /// lost is exactly the state in which a coincidence four hundred lines away has
    /// nothing to beat. Distance is expensive because the show is continuous, and the
    /// show goes on being continuous while we are lost.
    pub challenger_scale_skips_lost: bool,
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
            jump_gap: 12,
            jump_threshold: 0.0,
            backward_threshold: 0.88,
            rival_min_gap: 4,
            phonetic_similarity: true,
            phonetic_similarity_trust: 0.78,
            char_similarity: true,
            char_similarity_trust: 0.85,
            lost_margin: 0.06,
            equivalent_text_competes: false,
            char_mismatch_penalty: 0.35,
            zone_factor: 1.0,
            // Gentle: a perfect match anywhere inside the window must still clear
            // `accept_threshold` on its own (1.0 × 0.70 > 0.62), or the window
            // would be decorative and skip tolerance unreachable.
            distance_prior_halflife: 12.0,
            prior_floor: 0.70,
            overlap_exp: 0.35,
            member_coverage_min: 0.5,
            landmark_boost: [1.05, 1.15, 1.30],
            reanchor_horizon: 40,
            reanchor_threshold: 0.80,
            skip_max: 2,
            decay_to_block_s: 8.0,
            decay_to_lost_s: 20.0,
            min_segment_tokens: 3,
            short_accept_threshold: 0.80,
            min_relocate_tokens: 4,
            jump_pending_ttl_s: 8.0,
            stall_to_lost_s: 90.0,
            noise_floor: 0.0,
            challenger_enabled: true,
            challenger_margin: 0.18,
            challenger_min_evidence: 0.62,
            challenger_min_hits: 3,
            challenger_extra_hit_lines: 20,
            challenger_max_hits: 9,
            jump_extra_sighting_lines: 60,
            challenger_scale_skips_lost: false,
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
    /// How many times this same distant place has now been seen.
    sightings: usize,
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
    /// The position currently sits on a line marked `hold`; see `ScriptLine::hold`.
    holding: bool,
    /// Best score anywhere in the script for the segment being handled, so that a
    /// segment nothing can explain is recognised as noise rather than as divergence.
    /// See `noise_floor`.
    best_anywhere: f64,
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
            holding: false,
            best_anywhere: 0.0,
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
    /// The hold the position is currently sitting on, if any. The display uses this
    /// to say *why* nothing is moving — "music", not a frozen page.
    pub fn hold(&self) -> Option<crate::script::Hold> {
        self.script.lines.get(self.position).and_then(|l| l.hold)
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
        self.best_anywhere = 0.0;
        // Sitting on a marked hold — music, a held silence, an improvised passage. The
        // script has said in advance that it cannot predict what comes out of the
        // speakers here, so nothing heard during it is evidence about position: it
        // must not erode the incumbent, must not run the timers towards `Lost`, and
        // above all must not let the whole-script challenger relocate the show on the
        // strength of a recogniser's opinion of a saxophone.
        //
        // The hold ends the moment something further on is heard — never on a timer.
        // See `ScriptLine::hold_seconds` for why.
        self.holding = self
            .script
            .lines
            .get(self.position)
            .and_then(|l| l.hold)
            .is_some();
        // Weak evidence keeps its higher bar; everything else may move the
        // position on `follow_threshold` and report the lower confidence honestly.
        let move_threshold = if weak {
            self.cfg.short_accept_threshold
        } else {
            self.cfg.follow_threshold
        };

        // Look at the whole script every time, not only once lost. Cheap, and it
        // is the only way to notice that somewhere else explains this better.
        if self.cfg.challenger_enabled && !weak && !self.holding {
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
                self.on_unmatched(seg, 0.0, &mut events);
            }
            return events;
        };
        let (best, runner_up) = best;

        // Going backwards costs much more evidence than going forwards.
        //
        // A show runs one way. Backwards happens — a restart, a retake, a company
        // going round again — but it is rare, while the *reasons to mistakenly think*
        // we should go back are everywhere: a repeated passage, a running gag, a
        // stock phrase. Judging both directions on one threshold treats the rare and
        // the routine as equally likely, and a wrong backward jump is the most
        // expensive error the tracker makes: it re-reads ground the show has left, so
        // every subsequent line disagrees and it stays wrong until it is lost.
        //
        // Forward mistakes self-correct — the show walks into them.
        // Distance costs evidence, in both directions.
        //
        // A show is continuous: the next line is overwhelmingly the most likely, and
        // any proposal to move a long way is claiming something rare happened. Rare
        // claims need more evidence than routine ones — and the sharpest case is an
        // exact repeat, which scores near 1.0 wherever it sits, so only distance can
        // tell the copy in front of us from the copy three hundred lines away.
        //
        // Backward stays steeper than forward: a wrong backward move re-reads ground
        // the show has left, so every later line disagrees and it cannot self-correct,
        // while a wrong forward move is walked into and fixed.
        //
        // Reported from a live run — the scene-4 performance of the Euripides text
        // that scene 2 only reads. Tonight's backward rule already blocked the copy
        // behind (0.70 after the distance prior, under a 0.88 bar); the copy *ahead*
        // had nothing to clear at all.
        let target = best.span.last();
        let going_back = target < self.position;
        let move_threshold = if matches!(self.confidence, Confidence::Lost) {
            // Nothing to preserve: every direction is equally unknown, and demanding
            // more here would only lengthen the relocation this exists to allow.
            move_threshold
        } else if going_back {
            move_threshold.max(self.cfg.backward_threshold)
        } else {
            move_threshold
        };

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
                self.on_unmatched(seg, best.score, &mut events);
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
                self.on_unmatched(seg, best.score, &mut events);
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
            // A big unanchored jump. Believe it only once it has been seen enough
            // times — and how many times *that* is depends on how far it is asking us
            // to go. See `challenger_extra_hit_lines`; this is the same rule guarding
            // the other door into a long move, and it has to be, because a tracker
            // that has decayed to `Lost` reaches this path with the whole script in
            // view and would otherwise relocate three hundred lines on two sightings.
            let distance = best.span.first().abs_diff(self.position);
            if self.script.span_token_count(best.span) < self.cfg.min_relocate_tokens {
                events.push(TrackerEvent::Rejected {
                    reason: RejectReason::WeakEvidence,
                    best_score: best.score,
                    best_index: Some(best.span.first()),
                });
                self.on_unmatched(seg, best.score, &mut events);
                return events;
            }
            let seen = match self.pending_jump.take() {
                Some(p) if best.span.first().abs_diff(p.index) <= 1 => p.sightings + 1,
                _ => 1,
            };
            if seen >= self.jump_sightings_needed(distance) {
                PositionCause::Jump
            } else {
                self.pending_jump = Some(PendingJump {
                    index: best.span.first(),
                    unmatched_at: self.unmatched_speech_s,
                    sightings: seen,
                });
                events.push(TrackerEvent::Rejected {
                    reason: RejectReason::JumpPending,
                    best_score: best.score,
                    best_index: Some(best.span.first()),
                });
                // Refusing to follow a single distant match is right; going on
                // *asserting* the old position is not. Something convincing was heard
                // somewhere else, so what we hold is now stale — say so immediately
                // rather than after the decay timer. This is where confident-wrong
                // events come from otherwise.
                self.demote(Confidence::Block, &mut events);
                self.on_unmatched(seg, best.score, &mut events);
                return events;
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
        // Step onto a hold the moment the line before it has been heard.
        //
        // A production stage direction is never matched — nobody says "Chorégraphie
        // sur la musique d'Otis Redding" out loud — so the position would step
        // straight over it and the hold would never fire at all. But the line before
        // it *was* just heard, which means the music is now. Parking on it is what
        // stops the next two minutes of transcribed soul record counting against the
        // position.
        if let Some(next) = self.script.lines.get(self.position + 1) {
            if next.hold.is_some() && !next.matchable {
                self.position += 1;
            }
        }
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
    fn on_unmatched(
        &mut self,
        seg: &TranscriptSegment,
        best_local: f64,
        events: &mut Vec<TrackerEvent>,
    ) {
        // Nothing in the script explains this at all: music, laughter, a cough, a
        // grunt, an improvised aside, or a stretch of hallucination over silence. It
        // is not the show diverging from the script — it is not the show speaking the
        // script at all, and the position we hold is no less likely than it was a
        // moment ago. See `noise_floor`.
        if self.holding {
            return;
        }
        if self.cfg.noise_floor > 0.0 && best_local.max(self.best_anywhere) < self.cfg.noise_floor {
            return;
        }
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

    /// How many agreeing sightings a move of `distance` lines must collect before it
    /// is believed, starting from `base` for a move of no distance at all.
    ///
    /// One rule, two doors. A long relocation can arrive either through the challenger
    /// or, once the tracker has decayed to `Lost` and the whole script comes into
    /// view, through the ordinary jump path — and charging only one of them just moves
    /// the traffic to the other. Measured: with the challenger alone guarded, a
    /// three-hundred-line move still went through in two sightings.
    fn confirmations_needed(&self, base: usize, distance: usize) -> usize {
        if self.cfg.challenger_extra_hit_lines == 0 {
            return base;
        }
        (base + distance / self.cfg.challenger_extra_hit_lines)
            .min(self.cfg.challenger_max_hits.max(base))
    }

    /// Agreeing sightings a jump of `distance` lines must collect. See
    /// `jump_extra_sighting_lines`.
    fn jump_sightings_needed(&self, distance: usize) -> usize {
        // While holding, every move is a relocation rather than an overshoot.
        //
        // The coarse jump rate exists because the jump path also carries ordinary
        // overshoots — a cut, a dropped exchange — and those deserve to be cheap. A
        // hold says there is no ordinary anything happening: the script has declared
        // that what is coming out of the speakers is music or noise, so a proposal to
        // move at all is a claim about somewhere else and is charged at the finer
        // rate. Observed on night 17, where pre-show music moved the position 57 lines
        // for two sightings and held it wrong for twenty seconds.
        let per = if self.holding {
            self.cfg.challenger_extra_hit_lines
        } else if self.cfg.jump_extra_sighting_lines > 0 {
            self.cfg.jump_extra_sighting_lines
        } else {
            self.cfg.challenger_extra_hit_lines
        };
        if per == 0 {
            return 2;
        }
        (2 + distance / per).min(self.cfg.challenger_max_hits.max(2))
    }

    /// Maintain the rival hypothesis, and adopt it if it has earned the position.
    ///
    /// Returns a `Position` event when the switch happens, in which case the
    /// caller must not also run the ordinary incumbent update for this segment.
    fn run_challenger(&mut self, seg: &TranscriptSegment) -> Option<TrackerEvent> {
        // Recorded even when no candidate survives, because "nothing anywhere explains
        // this" is exactly the fact `on_unmatched` needs in order to leave the
        // incumbent alone.
        let best = self.scan_whole_script(seg);
        self.best_anywhere = best.as_ref().map_or(0.0, |b| b.score);
        let best = best?;
        if self.script.span_token_count(best.span) < self.cfg.min_relocate_tokens {
            return None;
        }
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
        // The further the claim, the more of it we ask to see. See
        // `challenger_extra_hit_lines`.
        let need_hits = if self.cfg.challenger_scale_skips_lost
            && self.confidence == Confidence::Lost
        {
            self.cfg.challenger_min_hits
        } else {
            self.confirmations_needed(
                self.cfg.challenger_min_hits,
                c.position.abs_diff(self.position),
            )
        };
        let earned = c.hits >= need_hits
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
        // Two candidates a few lines apart are not competing answers, they are the
        // same answer at different precision — the operator reads a page, and both
        // land on it.
        //
        // This is what a company riffing costs. Reported from a live run: the tracker
        // struggles through
        //
        //     L0111  C'est très mystérieux…
        //     L0113  C'est un peu étrange.
        //     L0114  C'est étrange et symbolique.
        //     L0115  Mais plus étrange que symbolique.
        //     L0148  …Symbolique. Mais un peu étrange.
        //
        // Five near-identical lines in one scene. Each tie is rejected for ambiguity,
        // the rejections decay confidence to lost, and *then* the whole-script search
        // relocates to somewhere unrelated — which is why the jump landed in a section
        // containing none of these words. The ambiguity never chose wrongly; it
        // refused to choose, and being lost is what did the damage.
        //
        // Choosing the better of two neighbours risks a line or two. Refusing risks
        // the scene.
        if a.abs_diff(b) <= self.cfg.rival_min_gap {
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
        // Copied out before the scratch buffers are borrowed, as above.
        let char_similarity = self.cfg.char_similarity;
        let char_trust = self.cfg.char_similarity_trust;
        let phonetic_similarity = self.cfg.phonetic_similarity;
        let phonetic_trust = self.cfg.phonetic_similarity_trust;
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
            let span_sound = script.span_sound(span, lang);
            let (seg_tokens, seg_sound) = self.prepared_segment(seg, lang);

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
                let words = token_set_ratio(&seg_tokens, line_tokens)
                    * token_dice(&seg_tokens, line_tokens).powf(overlap_exp);
                // Take the better of words and characters rather than blending them.
                // A line whose words match is already scored well and gains nothing
                // from characters; a line the recogniser mis-split scores near zero on
                // words and should not be dragged down by that. The failure being
                // fixed is one-sided, so the remedy is too.
                let mut s = words;
                if char_similarity {
                    let chars =
                        char_trigram_dice(&seg_tokens.join(" "), &line_tokens.join(" "));
                    s = s.max(chars * char_trust);
                }
                if phonetic_similarity && !seg_sound.is_empty() && !span_sound.is_empty() {
                    // Sound is the last resort and the strongest evidence when the
                    // spelling has failed entirely: "Polyme Store" shares no token and
                    // few letters with "Polymestor", and is the same utterance.
                    s = s.max(char_trigram_dice(seg_sound, &span_sound) * phonetic_trust);
                }
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
    fn prepared_segment(&mut self, seg: &TranscriptSegment, lang: &LangCode) -> (Vec<&str>, &str) {
        if !self.seg_by_lang.iter().any(|(l, _)| l == lang) {
            let mt = self.reg.prepare(&seg.text, lang);
            self.seg_by_lang.push((lang.clone(), mt));
        }
        let mt = &self.seg_by_lang.iter().find(|(l, _)| l == lang).unwrap().1;
        (
            mt.tokens.iter().map(String::as_str).collect(),
            mt.sound.as_str(),
        )
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
