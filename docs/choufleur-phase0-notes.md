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
| `choufleur-asr` — resampling, Silero VAD via `ort`, whisper-rs engine | done |
| `choufleur-replay` — manifest, formats, WAV streaming, `verify`, `make-fixture`, `track`, `eval` | done |
| `choufleur-replay` — streaming engine, virtual clock, `transcribe`, `track --from-audio` | done |
| `corpus/README.md`, `research/align.py`, `scripts/fetch-models.sh` | done |
| M0.1 corpus assembly, M0.4 sweep, the gate call | waiting on real recordings |

139 tests pass; fmt and clippy are clean. The pipeline runs end to end on audio:
WAVs in, Whisper transcript, tracked position, scored report.

**Nothing here has met real theatre audio yet.** The numbers below are measured,
but they are measured on *synthesized* speech — perfect diction, no reverb, no
bleed, no overlap, no audience. They say the plumbing works and they price the
compute honestly. They say nothing about the go/no-go gate, and no threshold
should be tuned against them. M0.4 turns these dials against the real corpus.

---

## Measured on the fixture (synthesized speech, 3 channels, 52.7 s, `small` + Metal, M-series)

The whole point of building the harness first: these are outputs of
`transcribe → track → eval`, not estimates.

| Configuration | Speed | Detection lag (median / p95) | Coverage ±1 | Gate |
|---|---|---|---|---|
| Batch, interim every 1.5 s | **8.3× real time** | 1.56 s / 1.67 s | 100 % | PASS |
| Batch, interim disabled | 15.2× real time | 2.67 s / 3.29 s | 99.7 % | **FAIL** (lag) |
| Realtime, coupled, tracker-biased | 1.00× (paced) | 1.56 s / 1.67 s | 100 % | PASS |
| Mixed feed (degraded mode) | 7.8× real time | 5.09 s / 19.9 s | 42.7 % | **FAIL** |

That fixture has **one speaker at a time** — ordinary dialogue takes turns. The
devplan's compute criterion is stated in *concurrent* active channels, so it needs
its own fixture; see finding D.

End-to-end latency, realtime coupled run: **median 351 ms, p95 510 ms, max 535 ms**
against a budget of 1.5 s typical / 3 s worst case. That is the number an operator
actually feels, queue wait included, and it has four times the headroom the PRD asks
for — on this material, at this channel count.

Per-segment decode cost is **~160 ms and near-constant regardless of segment
length**, because whisper.cpp zero-pads every input to a 30-second mel window. A
1-second segment costs almost exactly what a 5-second one does. This single fact
drives the whole latency/compute trade below.

## Verified third-party facts (checked, not remembered)

The model-bound stages were written against the vendored crate sources rather than
from memory, by agents that compiled and ran what they reported. Six of these would
have been **silent** bugs — wrong answers with no error anywhere — and are the
reason that verification was worth doing before writing a line of the integration:

- **Silero's output state tensor is named `stateN`, not `state`.** Indexing
  `SessionOutputs` by an unknown name *panics* rather than erroring, so this is
  reached through `.get()`, which doubles as the v4-vs-v5 model check.
- **A wrong VAD window length is silently accepted.** The model declares its input
  shape as fully dynamic, so feeding 512 samples where 576 are required returns a
  plausible probability computed from the wrong thing. `process_window` takes
  `&[f32; 512]` so the compiler enforces what the model will not.
- **`ort::inputs!` is not fallible in rc.13** — `inputs!{...}?` is a compile error,
  though earlier release candidates required it.
- **`set_suppress_non_speech_tokens` does not exist**; the real name is
  `set_suppress_nst`.
- **`set_initial_prompt` leaks a `CString` on every call** — `FullParams` has no
  `Drop`. Prompts are pre-tokenized with `ctx.tokenize` and passed via `set_tokens`,
  which allocates nothing. Over a show-length run with a fresh prompt per segment,
  the leak would be steady.
- **An invalid language code fails silently** and decodes with an out-of-range
  token; validated up front with `get_lang_id`.
- `temperature_inc` defaults to 0.2, which silently re-decodes poor segments at
  rising temperatures and multiplies worst-case latency; set to 0 for a predictable
  budget. `print_progress` defaults to *true* and will spam stdout mid-show.
- `avg_logprob` is not exposed by the C API at all; it is computed here from
  per-token `plog`, skipping special and timestamp tokens.
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

### A. Detection lag has a floor equal to the length of the line being spoken — and interim emission is what buys it back

First predicted from a simulated transcript, now **confirmed on real audio and
priced**. With one segment per utterance the tracker cannot learn a line until the
actor has stopped saying it, so detection lag is bounded below by the line's own
duration. Measured: median 2.67 s, p95 3.29 s, against a 2.0 s / 4.0 s gate — a
clean failure with a perfect transcript and 99.7 % coverage. No matcher tuning
recovers it, because nothing is wrong with the matching.

The fix is in segmentation: an open speech run is also emitted *interim* every
`interim_interval_ms` (default 1500), carrying everything heard so far. Same audio,
same tracker, same eval: **median 1.56 s, p95 1.67 s, gate met**. Exact coverage
(±0 lines) also jumps from 6.7 % to 36.4 % — the tracker is not merely close more
often, it is *right* more often.

**The price, now measured.** Interim emission roughly doubles the decode count
(18 segments → 37) and costs a little over half the throughput headroom: 15.2×
real time falls to 8.3×. Because each decode is a near-constant ~160 ms, the
arithmetic is simple and worth writing down for the venue:

> decodes per second ≈ (active channels) ÷ (interim interval in seconds)
> compute load ≈ decodes per second × 0.16 s

Four channels all speaking continuously at a 1.5 s interval is ~2.7 decodes/s,
about 0.43 s of decode per second of audio — roughly 2.3× real time, still inside
budget with `small` on Apple Silicon. `medium` is roughly 3× the decode cost and
would land near 0.8× — i.e. **`medium` with interim emission on four simultaneous
channels does not fit**, which matters because the PRD recommends `medium` for
non-English shows. The degrade path, if the field proves this tight: interim
emission only on the most recently active channel, or a longer interval, both of
which trade lag back for compute along the curve above.

`interim_interval_ms` is exposed as `--interim-ms` precisely so M0.4 can walk that
curve rather than argue about it.

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

### C. On a mixed feed, the thing that breaks first is *language*, not diarization

The PRD offers a single mixed feed as a graceful degrade. Measured on a bilingual
fixture, it is not graceful: coverage ±1 falls from 100 % to 42.7 %, with a
confident-wrong event.

The interesting part is the cause. It is not primarily lost speaker identity — it
is that **a mixed feed has no per-line language to force**. Language is a property
of the line (notation §8), and without knowing whose line is being spoken there is
nothing to look it up by. The engine falls back to the show default and decodes the
whole act as French, so English lines come back as French mishearings: *"I saw the
smoke from the station platform"* becomes *"Je vois le smok de la plateforme
station"*. That text is not recoverable by any amount of fuzzy matching.

Letting the tracker supply the expected language (`track --from-audio --mixdown
--bias tracker`) helps and removes the confident-wrong event — coverage ±3 rises
from 73.7 % to 88 % — but it is chicken-and-egg: the tracker cannot advance on a
mistranscribed line, so it keeps supplying the language of the line it is stuck on.

A second, genuinely diarization-shaped problem compounds it: on a mixed feed the
VAD hears continuous speech across a speaker change, so one segment spans several
characters' lines.

Practical consequence, worth stating before someone plans a show around it: the
mixed-feed degrade is reasonable for a **monolingual** production and poor for a
multilingual one. The PRD's *Out of Scope* list already defers mixed-feed
diarization; this adds that multilingual mixed-feed tracking is deferred with it.

---

### D. The compute criterion holds at four concurrent channels, and the ceiling is about seven

The accuracy fixtures are dialogue: exactly one channel is active at any instant,
so "8.3× real time" was measuring a single stream however many channels the
manifest listed. The devplan asks for *"sustained faster-than-real-time with 3–4
concurrent active channels"*, which that cannot answer.

`make-fixture --load-test <n>` builds the missing artifact: n characters on
independent timelines, every channel speaking continuously. The 4-channel fixture
runs with a **mean of 3.46 simultaneous speakers, four at once for 70 % of its
length** — nothing like a play, which is the point.

Measured, same audio, varying the channel subset (`small`, interim 1.5 s):

| Concurrent channels | Throughput |
|---|---|
| 1 | 7.59× real time |
| 2 | 3.78× |
| 3 | 2.48× |
| 4 | **1.75×** |
| 8 | **0.84× — slower than real time** |

Scaling is 1/N almost exactly, which is what a single sequential Whisper engine
should do, and the 8-channel run was a prediction from the first four rows before
it was measured. **The ceiling on this machine is about seven continuously active
channels**; four leaves a 1.75× margin.

End-to-end latency at four concurrent channels, realtime: **median 586 ms, p95
1092 ms, max 1652 ms**. Two segments of 98 exceeded 1.5 s; none came close to the
3 s worst case. So the criterion is met — but the margin at four channels is a
factor of 1.75, not the factor of 8 the dialogue fixture suggested, and that is
the number to carry into any decision about model size or channel count.

This also puts a measured floor under the PRD's load-management argument. Sixteen
*configured* channels is fine precisely because only three or four are ever
*active*; active-speaker gating is not an optimization, it is what makes the
channel count possible at all. Idle channels genuinely cost nothing — the VAD
never opens on them and no decode happens.

Extrapolating the same way for `medium` (roughly 3× the decode cost) puts four
concurrent channels near 0.6× — under real time. That remains an extrapolation,
one `--model` flag from being measured, and it matters because the PRD recommends
`medium` for non-English shows.

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
- Whether `medium` — which the PRD recommends for non-English shows — fits at all
  with interim emission on four simultaneous channels. Finding D's measured 1.75×
  margin with `small` and `medium`'s roughly 3× decode cost put it under real
  time; measuring it is one `--model` flag over `corpus/fixture-load`.
- Whether real theatre audio changes the ~160 ms per-decode constant much. It is
  dominated by the fixed 30-second mel window, so it should be stable, but reverb
  and overlap make segments longer and more numerous.
- Whether the honesty criterion should be restated as "no confident-wrong event
  longer than one segment" (finding B).
