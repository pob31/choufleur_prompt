# Phase 0 — Implementation Notes

Running log of decisions made and things learned while building the Phase 0 spike.
The devplan says *what* and *in what order*; this file records *what we found out
on the way*, especially where reality disagreed with the plan.

---

## Status

| Piece | State |
|---|---|
| Cargo workspace, `.gitignore`, READMEs | done |
| `choufleur-core` — normalization, languages, matcher, script index, tracker v0, prompt biasing | done |
| `choufleur-asr` — VAD segmentation policy, hallucination filter (both pure, no models) | done |
| `choufleur-asr` — resampling, Silero via `ort`, whisper-rs engine | **not started** |
| `choufleur-replay` — manifest, formats, WAV streaming, `verify`, `make-fixture`, `track`, `eval` | done |
| `choufleur-replay` — streaming engine, virtual clock, `transcribe`, `track --from-audio` | **not started** |
| `corpus/README.md`, `research/align.py`, `scripts/fetch-models.sh` | done |
| M0.1 corpus assembly, M0.4 sweep, the gate call | waiting on real recordings |

104 tests pass; clippy is clean. Everything so far runs without models and without
audio hardware, and the end-to-end path is exercised on a synthetic fixture.

**Nothing here has met real theatre audio yet.** Every threshold below is a
starting point reasoned from first principles, not a measured value. M0.4 turns
them against the real corpus and this file records the outcome.

---

## Verified third-party facts (checked, not remembered)

- `rapidfuzz` 0.5 exposes `fuzz::ratio` only — **no `token_set_ratio`**. Implemented
  ourselves in `matcher.rs`.
- `rapidfuzz::fuzz::ratio` returns **`[0, 1]`**, not the Python library's 0–100.
  Caught by a test that expected the 100 scale and got 0.01.
- macOS `say` writes 48 kHz WAV directly via `--data-format=LEI16@48000` on this
  machine; the `afconvert` path is kept as a probed fallback.
- Installed voices used by the fixture: Thomas (fr_FR), Samantha (en_US), Daniel
  (en_GB). French voices are a Settings download on some Macs, so `make-fixture`
  preflights and prints what is actually installed.
- Toolchain: rustc 1.96, cmake 4.2.3, macOS 15.7, arm64, full Xcode.

Still to verify when the model stages land (do not trust memory): `ort` 2.0.0-rc.13
tensor API, rubato 5 `audioadapter` buffers, whisper-rs 0.16 setter names and the
`no_speech_prob` accessor, and the Silero v5 asset URL at its pinned tag.

---

## The two findings that change the design

### A. Detection lag has a floor equal to the length of the line being spoken

Measured, not theorized: `crates/choufleur-replay/tests/pipeline.rs` runs a perfect
transcript through the real tracker and scores it with the real eval. Coverage is
essentially perfect and there are no confident-wrong events — **and the lag gate
fails**, at a median around the mean line duration.

The cause is structural. With one segment per utterance, the tracker cannot learn
a line until the actor has stopped saying it. A four-second line lands four seconds
late against a two-second gate, and no amount of matcher tuning recovers it.

The fix is in segmentation, not matching: an open speech run is emitted *interim*
every `interim_interval_ms` (default 1500), carrying everything heard so far. The
tracker watches the line take shape and commits partway through. A companion test
feeds the identical audio through interim segmentation and the lag gate passes.

This is the PRD's "sliding-window / local-agreement policy for stable partial
results" made concrete, and it is now the reason that policy is non-optional. The
cost is real and unpriced: **every interim emission is a full Whisper decode**, so
a 4 s line costs 3 decodes instead of 1. `interim_interval_ms` is therefore the
main compute-vs-latency dial for M0.4, and it interacts directly with the
"faster than real time on 3–4 concurrent channels" criterion. If compute turns out
tight, interim emission on the *most recently active* channel only is the obvious
degrade.

### B. A cut wider than skip tolerance costs one segment of staleness, unavoidably

When the director cuts four consecutive lines, nothing tells the tracker until the
*next* material is heard. Until then it holds a position that has become wrong.

Two things were done about it. What could be fixed: a single distant high-scoring
match now immediately demotes confidence to `Block` even though the position does
not move — something convincing was heard elsewhere, so what we hold is stale, and
saying so at once rather than after an 8-second decay timer is what stops it being
reported as *confident*-wrong. What cannot be fixed: the window between the cut and
the next audible line, which is bounded by segment length and is physics.

**This matters for the gate.** The devplan states honesty as "< 1 confident-wrong
event per act", i.e. zero. If an act contains cuts, brief stale windows are
unavoidable, and a strictly-zero criterion fails a tracker that is behaving
correctly. Suggested restatement once the real corpus exists: *no confident-wrong
event lasting longer than one segment*, with total confidently-wrong time reported
alongside. The eval already emits per-event durations, so either criterion can be
computed without changing code.

---

## What the tracker tests taught us about scoring

Five scoring bugs surfaced only because the scenarios were written from the PRD's
own failure table. Each would have shown up in the field as "the position runs
ahead of the show" — the exact failure the PRD calls most dangerous.

1. **Token-set similarity scores a subset ≈ 1.0.** That is deliberate (it is what
   lets a chunk spilling across a line boundary still match) but it means a
   three-line span always outscores the single line the segment covered.
   *Fix:* multiply by token-overlap (Dice), `overlap_exp = 0.5`. Similarity asks
   *is this the same words*; overlap asks *and nothing but those words*.

2. **A length-balance factor rewards padding.** The first attempt weighted by
   token-count balance. Being symmetric, it meant that when ASR ran *longer* than
   the line (a paraphrase adds words), appending another script line **improved**
   the score. A paraphrase of line 1 matched span `[1,3]` and landed on line 3.
   *Lesson:* any aggregate over a concatenated span can be gamed by padding.

3. **Short lines are nearly free to append.** Even with Dice, adding `"Oui."` (one
   token) moved the score from 1.00 to 0.93. Scripts are full of one-word
   interjections, so this drifts the position forward constantly.
   *Fix:* `member_coverage_min` (0.5) — before a multi-line span may claim a line
   was heard, that line's own distinct tokens must be at least half present.

4. **The ambiguity margin fired on candidates that agreed.** Grouping runner-ups by
   span *start* made `[4]` and `[2,4]` rivals, though both say "we are at line 4".
   The tracker fell silent exactly when two readings corroborated each other.
   *Fix:* group by span *last* — the resulting position. Duplicate-line ambiguity
   ("Yes." × 12) still trips it, because those land on different lines.

5. **Clamping the score to 1.0 destroyed the margin.** With the landmark boost
   applied multiplicatively, several strong candidates all clamped to exactly 1.0,
   the margin became 0, and everything read as ambiguous. The score is a **ranking
   score, not a probability**, and is left unclamped. Confidence level still comes
   from unweighted similarity, so `Word` confidence cannot be bought with a boost.

And one bug in the eval itself, which would have quietly mis-scored everything:
**ground-truth onsets were not breakpoints** in the coverage integral. Back-to-back
dialogue merges into a single speech interval, and the line boundaries inside it
are exactly where the expected position changes. Losing them under-reported exact
coverage by more than half.

---

## Design decisions taken here (beyond the plan)

- **Punctuation normalizes to a space, not to nothing** (`"well-known"` → `"well
  known"`, `"j'suis"` → `"j suis"`). Deleting fuses tokens across hyphens and
  apostrophes. Notation §3.1's line-ID hash is defined over this output, so this is
  a format-level commitment, documented in `normalize.rs`.
- **Language folding is matching-only**, never part of the ID hash. French folds
  diacritics and expands elisions (`j` → `je`); English repairs the stems the
  apostrophe swallowed (`don` → `do`, `t` → `not`). Ambiguous fragments (`s`, `d`)
  are left alone — they are symmetric on both sides of the comparison anyway.
- **Short segments are weak evidence, not no evidence.** Discarding sub-threshold
  utterances outright cost real coverage: the tracker sat a line behind through
  every subsequent speech. Believing them outright is worse — "yes" appears a dozen
  times an act. They are now matched against the *next line only*, at a higher bar
  (`short_accept_threshold` 0.80), can never re-anchor or jump, never reach `Word`
  confidence, and never count as divergence.
- **Mic bleed needs no tracker rule.** A first implementation carried a
  recent-accept ring to detect the same line arriving twice. It was unreachable:
  per-character channels never receive another character's lines as candidates, and
  the one route that stays open (landmark spans) is already closed by
  `char_mismatch_penalty`. Deleted rather than kept as decorative safety.
  Level-thresholded attribution, which the PRD assigns this job, belongs upstream in
  the capture path.
- **Landmark boost comes from the span's first line only** — a landmark's claim is
  about where a span *starts*, not about material it happens to run over.
- **Distance prior is gentle** (halflife 12 lines, floor 0.70). The first defaults
  (halflife 4, floor 0.55) made a *perfect* match unacceptable beyond ~3 lines
  ahead, silently making `window_ahead: 8` decorative and skip tolerance
  unreachable. Constraint to preserve when tuning: `prior_floor > accept_threshold`.
- **Silence never decays confidence** — decay accumulates *speech* seen without a
  match. A ten-minute hold for director notes must not read as lost tracking.
- **Trace timings are opt-in** (`track --timings`). A trace carrying wall-clock
  measurements is not byte-reproducible, and byte-reproducibility is the entire
  reason the from-segments path — rather than the ASR path — is the pinned
  regression artifact. Metal float arithmetic is only reproducible on the same
  machine; matching is reproducible anywhere.
- **VAD minimum-length is measured on speech, not on buffer length.** A cough
  wrapped in 200 ms of pre-pad and 400 ms of hangover is a 700 ms buffer; only the
  ~100 ms that actually looked like speech should count.

---

## Open questions for the real corpus

- Is `member_coverage_min: 0.5` too strict for far-field zone channels, where ASR
  drops more words? It may need to be per-channel-class.
- The noise floor of `token_set_ratio` on unrelated French lines measures ~0.38;
  `accept_threshold` is 0.62. Comfortable on clean text, unknown under real WER.
- Should `char_mismatch_penalty` (0.35) be a hard exclusion? It currently makes
  landmark re-anchoring impossible on the wrong channel, which is right, but also
  means a landmark line delivered by an understudy on a different mic never
  re-anchors.
- What `interim_interval_ms` actually costs in decode time at 3–4 active channels —
  finding A's open question, and the one that decides whether the latency budget
  and the compute budget can be met at the same time.
- Whether the honesty criterion should be restated as "no confident-wrong event
  longer than one segment" (finding B).
