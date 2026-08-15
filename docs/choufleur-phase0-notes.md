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

### E. First contact with real theatre audio: the recogniser is not the problem

*La Reprise* (Milo Rau), rehearsal multitrack, six excerpts, ~53 minutes. Excerpt 1
is a Johan monologue in Dutch and English, one close mic, 322 s. The script is the
production's own *Regiefassung* — the script plus every lighting, camera and stage
note written over it, typed by an assistant fluent in neither French nor Dutch.
That is what a real prep document looks like, and it is the right thing to test on.

Three things came out of the first run.

**Recognition is better than expected.** Dutch came back fluent and often verbatim:
*"Wat heb ik net gedaan? Ik ben het toneel opgelopen."* is the script line exactly.
The Hamlet verse came back as near-perfect Shakespeare. Whatever eventually limits
this system on real audio, it is not `small`'s ability to hear a stage.

**A real bug, found only by real material.** `transcribe` took each character's
*first* language and applied it to their whole channel. Johan opens in Dutch and
then quotes Hamlet, so the English was forced to Dutch — and Whisper did not
mis-hear it, it **translated** it: *"I am thy father's spirit"* came back as
*"Ik ben jouw vader Spirit"*, fluent, confident, and wrong, with nothing anywhere
to flag it. `DecodeHint` now carries a candidate *set* of languages and the most
confident decode wins, which is what notation §8.2 asks for bilingual lines and
what a language switch mid-channel needs. The same segment now returns *"I am thy
Father Spirit."* The cost is a decode per extra candidate on that channel — 9.3×
real time falls to 4.8× — which is another reason `track --from-audio` is the
primary mode: knowing the position means knowing the language, and paying once.

**The ceiling is set by the material, not the engine.** `research/script_vs_audio.py`
asks, for every script line, whether anything resembling it appears *anywhere* in
the transcript — the best any tracker could do. On this excerpt: **16 of 24 lines,
67 %** (English 73 %, Dutch 62 %). The tracker reached 15 distinct lines and ended
on the correct final line, so it is running at roughly the available ceiling.

The gap is not ASR and not matching. The actor paraphrases: the script's *"Volgens
mij het allermoeilijkste: opkomen"* was performed as *"Opkomen. Dat is eigenlijk
het allerlastigste"*, and several passages were reworked wholesale or spoken in an
order the script does not have. Meanwhile the Hamlet verse — memorised, fixed —
tracked line by line without a stumble. That contrast is the finding: **verse and
scripted dialogue track well; devised prose that is still being found in rehearsal
does not, and cannot.**

Consequences worth stating before the gate is run:

- A ≥ 90 % coverage gate is unreachable on rehearsal material where the text is
  still moving, however good the engine is. The gate needs a *performance* run, or
  it needs to be scored against the ceiling rather than against the script.
- `script_vs_audio.py` should be run on every corpus before any tuning. Tuning a
  matcher against lines that were never spoken is how thresholds get quietly
  wrecked.
- This is precisely the case the PRD's Family A divergence warnings exist for. The
  tracker holding position and reporting uncertainty through a paraphrased passage
  is correct behaviour, not failure — and on this excerpt it produced no
  confident-wrong events at all.

### F. Steadiness beats precision, and freezing is not the safe option

Six excerpts of *La Reprise* (~53 min, 1 to 6 channels). The script is the
premiere Regiefassung; the recordings are from well into the tour, so the text has
drifted and was never updated. That is an ordinary situation, not a spoiled corpus.

The first run tracked almost nothing: 1 of 44 lines on one excerpt, 2 of 34 on
another, overwhelmingly `below_threshold`. Two causes, both mine.

**A converted script has no landmarks.** One act, one scene, no explicit tags — so
the only landmark anywhere is the implicit one at line 0. With `window_ahead: 8`
and re-anchoring that only considers landmarks, a run that missed the opening lines
could never reach line 9, however plainly the actors were speaking line 30. Being
lost was a permanent condition. Now `lost_search_all` widens the candidate set to
the whole script once confidence reaches `Lost`, and a sufficiently strong,
unambiguous match re-anchors without needing a landmark. Being lost is precisely
the situation in which looking everywhere is correct.

**Refusing to move is not the conservative choice.** The engine was built so that
the position only advanced on a match good enough to call `Line`. The operator's
actual requirement, from the person who runs these shows: *"the text once it's
going is going to be more or less sequential. We don't need pinpoint accuracy, but
steadiness. The operator will read the whole page as it advances."* A page that
keeps pace a few lines behind is useful; a page frozen two minutes ago is worse
than useless, because nothing marks it as stale.

So movement and confidence were separated. `follow_threshold` (0.45) is what it
takes to move the position, reported at `Block` — the PRD's own "somewhere in this
exchange" level, which existed in the taxonomy but was never used for matching.
`accept_threshold` (0.62) is what it takes to call it `Line`. Honesty is preserved
by *saying* how sure it is rather than by standing still, and the eval's
confident-wrong rule is unaffected: `Block` is below `Line`, so a coarse position
can never register as confidently wrong.

Same audio, same transcripts, before and after:

| Excerpt | before | after | ends on |
|---|---|---|---|
| 1 Johan, nl/en, 1 ch | 15/24 | **22/24** | last line ✓ |
| 2 Sébastien, fr monologue, 1 ch | 4/8 | **8/8** | last line ✓ |
| 3 Sara & Fabian, fr, heavy accents, 2 ch | 1/44 | 9/44 | line 19/44 |
| 4 Sébastien & Tom, fr + gibberish, 2 ch | 2/34 | **17/34** | last line ✓ |
| 5 Perche, boom mic (zone), fr, 1 ch | 1/19 | **9/19** | last line ✓ |
| 6 Tutti, car scene, 6 ch | — | 4/40 | line 4/40 |

Four of six now finish on the correct final line, including the boom-mic zone
channel with no speaker identity at all.

Two things this exposed that are not yet fixed:

- **`script_vs_audio.py`'s ceiling is pessimistic.** It uses symmetric Dice, which
  scores a short fragment against a long line badly even when the fragment is
  correct — and far-field capture is mostly fragments. Johan_1 tracked 92 % against
  a "67 % ceiling". The ceiling tool needs the asymmetric measure the tracker uses.
- **`script_vs_audio.py`'s ceiling is still pessimistic** and should use the
  asymmetric measure the tracker uses.

### G. Matching something is not the same as making progress

Excerpt 6 is the car scene: cross talk, fighting, grunting, six channels. It
tracked 4 of 40 lines — and the score was not the worrying part. It sat on line
four for seventeen minutes at `Line` confidence and **never once declared itself
lost**, so it would never have raised the help request the PRD promises. A frozen
page with no warning, in the scene where an operator most needs one, is a worse
outcome than tracking badly and saying so.

Two defects behind it, and the first affects every run, not this excerpt.

**Interim hypotheses were double-counted as speech.** They are *prefixes* of the
segment that follows, so their durations overlap: on this excerpt 386 s of real
speech summed to 825 s. Every decay timer therefore ran at roughly twice real time,
which would declare tracking lost — and raise a help request — in the middle of a
passage that was tracking perfectly well. Only final segments tile the speech
exactly once, so only they are counted now.

**Decay reset on any match rather than on progress.** In cross talk the tracker
keeps finding weak support for lines near where it already is, which reset the
unmatched-speech timer indefinitely. A second timer now measures speech since the
position last actually moved forward, with a deliberately generous threshold
(`stall_to_lost_s`, 90 s) because a monologue legitimately holds one line for a
long time — excerpt 2 has 25-second lines.

The two together took excerpt 6 from **4 of 40 lines to 20 of 40**, with no change
anywhere else, because declaring `Lost` is what engages the whole-script search
added in finding F. The mechanism matters more than the number: on genuinely
impossible audio the correct behaviour is not good tracking, it is prompt honest
failure — and prompt honest failure is also what lets it find its way back.

Lost-events per excerpt are now plausible where before they were not: 0 on the
clean French monologue, 1 on the bilingual one, 2 on the fight.

### H. Learning the performed text: it works, and the naive version makes things worse

The dominant error source is script drift, so the obvious question is whether a
production can teach Choufleur its own text night after night. Measured, on the
La Reprise corpus, the answer is yes — with two conditions that are not optional.

`ScriptLine` gains an `alternates` array: ways a line has actually been performed,
matched *in addition to* the written text, which is never touched and is still what
the client displays (notation §2, principle 1). `research/learn_alternates.py`
aligns a run's transcript against the script offline and proposes them.

**Learn from the recording, never from the tracker's own trace.** Learning from the
live tracker's confident matches would be confirmation bias: it reinforces whatever
the tracker already believed and drifts the script toward its own mistakes. The
offline pass has the whole recording in both directions, no forward-only constraint
and no latency budget, and is simply a better observer.

**Proposals require a mutual best match.** The first implementation took each
line's best-matching passage, and it *reduced* tracking on one excerpt. The reason
is visible in one example: line L-0002 was proposed the alternate *"Ik ben het
toneel opgelopen. Opkomen."* — whose first half is **line L-0001's text**. The
alternate made L-0002 match passages that were never L-0002. Another line latched
onto a wholly different speech on the coincidental overlap of the word *acteur*. A
proposal is only trustworthy when the line's best passage also has that line as
*its* best line: they must choose each other.

Same audio, tracking with the written script versus with learned alternates:

| Excerpt | written | learned | Δ |
|---|---|---|---|
| 3 Sara & Fabian (most drifted) | 9/44 | **19/44** | **+10** |
| 4 Sébastien & Tom | 17/34 | 20/34 | +3 |
| 5 Perche (boom mic) | 9/19 | 11/19 | +2 |
| 1 Johan | 22/24 | 21/24 | −1 |
| 2, 6 | unchanged | | 0 |

The largest gain lands on the excerpt with the most drift, which is what the theory
predicts and the reason to believe the mechanism rather than the number.

Two honest caveats. This learned and measured on the **same run**, so it bounds
what a *corrected script* is worth — the counterfactual "if the script had said what
the actors say" — and says nothing yet about generalising to the next night. And
one excerpt still regresses, because a single night cannot separate a real change
in the text from an ASR slip or a one-off improvisation. That is what `--min-runs`
is for: require a variant to recur across runs before proposing it. It is
implemented and **untested**, for want of a second night — and it is the specific
reason learning *night after night* is worth more than learning once.

### I. It can find the page in a whole show, without being told where to look

Every measurement so far used a script cut to the excerpt — which quietly hands the
tracker the answer. The real case is a whole show loaded at once, with the engine
expected to work out where in it the performance is. Tested by tracking the same six
transcripts against the **full 377-line Regiefassung** instead of their excerpts.

The first attempt failed completely: five of six never moved a single line. That is
what exposed `lost_search_all` as dead config (see the correction in finding F).
With the search actually wired up, and with the initial `Scene` state treated as
searchable too — it means "we know the scene but not the line", which is nearer to
lost than to tracking — **all six locate themselves, with 100 % of position updates
falling inside the correct region**:

| Excerpt | should be | found | time to first fix |
|---|---|---|---|
| 1 Johan | L001–L024 | L001–L024 | 2.5 s |
| 2 Sébastien | L026–L033 | L029–L033 | 110 s |
| 3 Sara & Fabian | L091–L134 | L100–L134 | 73 s |
| 4 Sébastien & Tom | L136–L169 | L141–L169 | 70 s |
| 5 Perche (boom) | L191–L208 | L196–L208 | 138 s |
| 6 Tutti (car scene) | L245–L283 | L245–L280 | 57 s |

Not one false location — no excerpt settled in the wrong part of the show, which is
the failure that would matter. What costs time is the cold start: the tracker enters
each excerpt one to two minutes in, because it must accumulate enough evidence to
outweigh the distance prior before it will believe a jump of two hundred lines.

That number is worse than it first looked, and the correction came from the
operator: **starting mid-scene is what rehearsals are**, and it is far more common
than a late start. A performance runs once from the top — the 2.5 s row — but the
tool spends most of its life in rehearsals that stop and restart from the middle of
a scene dozens of times an evening, often with nobody touching the console. Quick
jump answers the announced restarts; the unannounced ones are the ordinary case.

So time to first fix is a first-class property, not a backstop, and the PRD and the
gate have been amended to say so. A median around 60 s of speech is too slow for
that use.

Where the time goes is not one thing. Excerpt 5 took 65 s but contained only **five
segments** of speech in that window — a boom mic capturing almost nothing, and no
tracker can locate a show from silence. Excerpt 2 took 109 s over **66 segments**,
which is a real deficiency: its 25-second paragraphs against 2.7-second segments
mean every segment is a *fragment* of its line, and the symmetric overlap term
penalises fragments by construction.

A sweep on that term is the leading candidate for closing the gap: `overlap_exp` 0
takes the median first fix from 61 s to 43 s with no confident-wrong events and
about 1 % less positional accuracy. It is **not** applied as a default here — one
night of one production, scored on lines-reached rather than against ground truth,
is not enough to move a core scoring parameter, and doing so would be the same
overfitting rejected for the challenger threshold in finding J. It belongs in M0.4's
sweep, against ground truth, on more than one show.

Widening the search to include the initial `Scene` state cut two of the cold starts
by more than half (excerpt 4: 220 s → 70 s; excerpt 5: 231 s → 138 s) and moved
excerpt 6 onto its exact first line.

### J. A concurrent second hypothesis, adopted only when it earns the position

Finding I left the tracker looking beyond its window only once it had already given
up — too late twice over. Recovery waits out the decay timer, and a position that is
confidently wrong while still finding weak local support never questions itself at
all. The suggestion, from the operator: run the search *continuously* and switch when
the alternative has been clearly better for a few seconds. It is multi-hypothesis
tracking, and it is the right shape.

The compute objection turned out not to exist. Scanning all 377 lines per segment
costs **0.03 ms median, 0.71 ms worst** against a 160 ms decode — a margin of
several thousand. There is no reason not to look.

A challenger is maintained alongside the position: the best explanation anywhere in
the script, advancing as the dialogue does, with a running average of how well it has
explained recent speech. It is adopted only after beating the incumbent's running
average by a margin, over several segments and several seconds. Three details did
the work, and two came from being wrong first.

**A rival is judged on the words, not on the distance.** The distance prior exists to
keep the position from leaping about; applied to a challenger it would simply punish
it for being far away, which is the whole point of it. Challengers are scored from
their own position as origin.

**An absolute bar, not only a margin.** The first version hijacked the position
during improvisation — caught by an existing scenario test, not by the corpus. When
nothing matches, the incumbent's evidence collapses toward zero, so a rival needs
only to beat *nothing*; any coincidental word overlap elsewhere in the script wins.
Off-script speech must leave the tracker saying it is lost, not confidently
somewhere else, so a challenger must now also explain the words well on its own
account.

**Adoption reports `Block`, however strong the evidence.** Across the corpus, eight
of nine adoptions were correct and the ninth was **indistinguishable by score** —
0.65, inside the range of the correct ones (0.62–0.78). No threshold separates them,
and tuning one to exclude a single observed failure would be fitting noise. What can
honestly be said is that a just-relocated tracker has not yet been confirmed by
anything: nothing has matched the *next* line from its new position. Reporting
`Block` says exactly that, keeps a wrong adoption out of the confident-wrong count,
and costs nothing when the adoption was right — the next match promotes it.

Time to first fix on the full script, before and after:

| Excerpt | before | after |
|---|---|---|
| 3 Sara & Fabian | 73 s | **32 s** |
| 5 Perche (boom) | 138 s | **65 s** |
| 1, 2, 4, 6 | unchanged | |

All six still locate the correct region, and **no excerpt records a single
confident-wrong update**. The one bad adoption self-corrected within 16 seconds.

### K. Learning does generalise across nights — but more nights did not help

Three takes of two scenes, 9th to 11th January 2019, a show already well into its
tour. The first genuinely held-out test: learn from the earlier nights, track the
later one.

**How stable is the text?** `research/night_variation.py` measures two things on one
scale — how close a night is to the *written* line (fidelity), and how close two
nights are to *each other* (consistency).

| Scene | fidelity | consistency | gap |
|---|---|---|---|
| Sara & Fabian | 0.55 | 0.68 | +0.13 |
| Sébastien & Tom | 0.60 | 0.71 | +0.12 |

The company is more consistent with itself than with the script, which is the
precondition for learning to be worth anything — but only by about 0.12. And in
both scenes the two nights furthest apart are the least alike, which is what
continuing drift looks like.

**Held-out tracking of the 11th:**

| Learned from | Sara & Fabian (44) | Sébastien & Tom (34) |
|---|---|---|
| nothing (written script) | 14 | 17 |
| the 9th | 15 | **22** |
| the 9th and 10th | 13 | 21 |
| the 9th and 10th, both agreeing | 14 | 19 |

**It generalises.** 17 → 22 on a night the learner never saw is the result worth
having, and it is largest on the scene whose delivery is most consistent — which
`night_variation.py` predicted in advance from the audio alone.

**More nights did not help, and requiring agreement helped least.** That was the
opposite of the prediction. Two causes, one fixed and one open.

Fixed: recurrence was counted on *exact transcript strings*, which almost never
repeat — the recogniser words things slightly differently every night even when the
delivery is identical. Exact voting cut 38 proposals to 2. Variants are now
clustered by similarity, and the most recent phrasing represents the cluster, since
the text keeps drifting.

Open: even with clustering, more alternates tracked slightly *worse*. Every added
variant is another thing that can match somewhere it should not, and the ambiguity
margin then rejects both. **Fewer, better alternates beat more alternates** — which
means the interesting parameter is not how many nights are collected but how
aggressively proposals are pruned, and two learning nights is not enough data to
tune that. Anyone continuing this should treat the number of alternates per line as
the dial, not the number of nights.

A second measurement worth recording: the same scene ran **415, 430 and 457 seconds**
on three consecutive nights, and 578, 565, 626 for the other — a 10 % spread in
duration. Any pace or ETA model calibrated on one night carries that much error into
the next.

### Checked: the excerpt boundaries are not the explanation

The La Reprise excerpts were cut from longer recordings without matching the audio
to the script excerpt — deliberately, and worth verifying rather than assuming,
since sloppy edges would inflate every "line never appeared" count and make drift
look worse than it is.

Asking of each script line whether *anything* resembling it exists anywhere in that
excerpt's audio:

| Excerpt | lost to the edges | genuinely absent mid-scene |
|---|---|---|
| 1 Johan | 0 | 2 |
| 2 Sébastien | 0 | 0 |
| 3 Sara & Fabian | 0 | 9 |
| 4 Sébastien & Tom | 2 at the start | 4 |
| 5 Perche | 0 | 4 |
| 6 Tutti | 2 at the end | 6 |

Four lines in the whole corpus are attributable to the cut. Everything else absent
is absent from the *middle* of a scene, where trimming cannot explain it, so the
drift diagnosis stands on its own.

One consequence for finding I: excerpt 4 entering at L141 against an expected L136
is two lines of missing audio and three of cold start, not five of cold start. The
other excerpts' late entries are cold start as reported.

Incidentally this makes the corpus a better test than a tidy one would have been.
Starting mid-scene with no run-up is exactly what happens when an operator switches
the system on late, and the tracker handled it.

### L. Levels: theatre mics are gained for the shouting, and both models mind

From the operator: gain is set so the loudest moment of the night does not clip —
an actor stripped and beaten in a car boot in act three — which leaves ordinary
dialogue far down. Measured across the corpus, that is exactly right, and worse
than expected:

| Channel | peak | speech level |
|---|---|---|
| car boot (the fight) | −2.6 dBFS | −44.7 dBFS |
| Johan | −27.3 | −43.8 |
| boom mic | −34.2 | −54.6 |
| Sara | −38.2 | **−66.8** |

Sara's speech averages eleven bits below full scale. Whisper and Silero are trained
near −20 dBFS, so this is far outside the distribution they learned — not a
signal-to-noise problem, since gain changes no SNR, but a range problem. And the
level predicts the tracking: Johan at −43.8 dB tracked 22 of 24 lines, the boom at
−54.6 managed 9 of 19, Sara at −66.8 managed 9 of 44.

`choufleur-asr::agc` adds a causal per-channel automatic gain between resampling and
detection. Two things it must not do, both learned the hard way.

**It must not measure the future.** Normalising a whole file offline, as the first
experiment did, is not available live.

**It must not amplify the room.** The offline experiment produced **forty confident
hallucinations** — "Thank you very much.", "We care of home." — because amplified
room tone is something a recogniser will find words in. So the floor and the voice
are tracked separately and gain is withheld unless they are far enough apart to mean
someone is speaking. The floor is learned only from moments quiet *relative to the
voice*: letting it rise during speech made a gapless monologue pull the floor up to
meet the voice until the gain faded out mid-line.

**A second bug surfaced through it.** With AGC on, hallucinations rose to 23 — all
English, in an all-French scene. The cause was not the gain. The script converter had
mis-detected two French lines as English (*"Tu peux aller à ta table."*, *"Un
sampler."*), which put English into those characters' candidate set for **every**
segment; and on marginal audio Whisper is *more* confident producing familiar English
filler than correct French, so the most-confident-decode rule picked the
hallucination every time. Requiring a language to account for at least 15 % of a
character's lines before it is offered blindly fixes it — and it is the right rule
independent of the bug, since the operator confirms the genuine switching in this
production is Johan's and Tom's parts only. Johan keeps `[nl, en]` at 13/11; Sara
loses English at 1 of 22.

Sara & Fabian, same recording:

| | segments | ceiling | hallucinations | lines |
|---|---|---|---|---|
| as recorded | 70 | 48 % | 7 | 13 |
| AGC only | 121 | 57 % | 23 | 8 |
| AGC + language share | 118 | **61 %** | **0** | 10 |

The audio side is unambiguous: half again as much speech recovered, the best ceiling
of any configuration, no hallucinations, and twice as fast for want of a second
decode per segment.

**But the recovered speech does not reach the tracker**, and this is now the third
independent sighting of the same obstacle. Quiet speech surfaces as *fragments*, and
the symmetric overlap term penalises a fragment exactly as it penalises over-reach.
Disabling it takes this scene from 10 lines to **25 of 44** — and breaks five scenario
tests, because the term is genuinely holding up guarantees about spans and jumps.
Exempting single-line spans, on the theory that over-reach needs two lines by
definition, breaks the same five. The two cases are entangled somewhere not yet
understood.

Left as it is, deliberately. Three measurements agree the term costs real accuracy on
quiet and far-field audio; the scenario tests are equally clear that removing it
costs correctness elsewhere. That is a design problem to solve against ground truth
in M0.4, not a threshold to flip on a proxy metric.

---

### M. A whole show on one mixed feed, and a failure that no threshold can fix

*Hécube, pas Hécube* (Tiago Rodrigues, Comédie-Française), two full performances a
night apart, after months of touring. One mono mixed feed each — no multitrack, no
speaker identity, the case finding C called "reasonable for a monolingual production"
on reasoning alone. Script imported from a rehearsal document: 984 lines, 16 scenes.

|                            | 16 Nov | 17 Nov |
| -------------------------- | ------ | ------ |
| audio                      | 7521 s | 7449 s |
| transcription              | 8.37× real time | 8.07× |
| distinct lines reached     | 594/996 (60 %) | 578/996 (58 %) |
| median \|script % − show %\| | 4.1 pts | 4.8 pts |
| lost / re-anchor           | 29 / 20 | 22 / 29 |

The second column is the point. Position in the script and position in the show agree
to within about four percentage points, on a two-hour recording, from a single feed,
starting at line 0 with no hint. Both nights, independently. That is the mixed-feed
half of finding C, measured rather than assumed.

**The failure worth studying.** At 350 s the actor says *"…de te regarder en face,
Polymestor, dans la détresse où je suis à présent."* That is L-0046. The tracker sat
at L-0192 — the right neighbourhood — but L-0046 is 146 lines *behind* it and the
search is forward-only, so the line being spoken was invisible. What it could see
ahead was L-0354, *the same Euripides speech*, which the company performs in scene 4
after reading it in scene 2. It jumped 162 lines and scored 0.92 — word confidence —
on a match that was genuinely correct, just not the current copy.

    L-0045  « Ô mon très cher ami Priam !… »        =  L-0347
    L-0046  « La honte m’empêche de te regarder… »  =  L-0354  =  L-0360

Twelve such families, 32 lines. No threshold helps: the second copy is a *good* match.
And this is not a quirk of one production — it is what a play about rehearsing a play
means, and the same shape appears wherever a text is quoted, replayed or reprised.

Two things follow. It recovered unaided in 20 s, re-anchoring backward across 300
lines when the company broke off to argue about the text — the recovery path working
exactly as designed. And the fix belongs in prep, not in the matcher: `prep_report.py`
now finds distant twins and marks both copies, so a matcher can demand more of a line
that occurs twice than of one that occurs once. Wiring the tracker to *use* that
annotation is still to do, and is the first thing to try on this corpus.

### N. The cues are already written down

The devplan assumes an operator types their cues in. They usually should not have to.
A sound operator who has run a show owns a *conduite* — the script as a PDF, marked
up — and the real one for Hécube holds 104
cue notes and 113 marks as live PDF annotations: numbered cues against the light plot,
Dugan automixer states, per-actor mutes and trims, distance-compensating delay, and
visual triggers (*"Tissus qui tombe >"*, *"Elsa enjambe >"*). `conduite_to_cues.py`
reads it and anchors **103 of 104 cues** to script lines.

Getting there taught the same lesson twice. Anchoring each note by its own highlighted
words placed 11 %: the marks are short (*"polymestor"*, *"Ça va ?"*) and — finding M
again — this script says the same thing in three places, so short text matches
anywhere. Adding a forward-only rule made it worse, because one wrong early anchor
drags the floor past most of the show and starves every later cue.

What worked was aligning *pages* first. A page carries eight or ten lines of dialogue,
far more evidence than any one highlight, and page order is show order; each page is
located by how much of each line is printed on it, and only then is a cue placed among
that page's lines. 111 of 115 pages locate, and the ambiguity a mark cannot resolve,
its page can.

**115 of the 133 cues fire on text.** The notes have a grammar — *"what I wait for >
what I then do"* — and the cue number tells that arrow apart from the one that merely
chains steps inside an action. Parsed that way, 115 cues are triggered by the spoken
text and 16 by something the operator watches for (*"Tissus qui tombe > 9.1 Lumière
47"*, *"Elsa pose son sac > 5.2 cut musique"*). Reading the grammar rather than
guessing at French stage vocabulary also cut the false visual count from 31 to 16, and
survives the next show in another language.

That ratio is the product case, measured on a real show: seven cues in eight are
exactly what script following is for, and the eighth can only ever be shown early —
which the client must present differently rather than implying a precision it cannot
have. Colour splits them again by who acts: 62 QLab and 53 desk moves on text.

Note finally what the conduite does **not** contain: no strikeouts, and every "cut"
note in the margin means *cut the music*. The performed cuts are marked another way
entirely — grey shading, mostly in the flattened layer — and a conduite that had been
flattened differently might not carry them at all, which keeps the audio-derived
proposals worth having as an independent source.

### O. Ambient-only capture fails, and it fails at recognition

*Lazzi* (Fabrice Melquiot), a two-hander, 1 h 53 m, captured on **one room mic** — no
close mics, no reinforcement, the sound design being a handful of music cues. The
hardest capture case in the corpus and the one that would most weaken the PRD's
requirements if it worked.

It does not work. 65 of 1023 lines reached (6 %), 64 losses, 1984 rejected segments,
and a jump to 81 % of the script six minutes into the show.

The tracker is not the problem, and the transcript says so plainly:

    Je vous m’en mord du cul.
    Les personnes en surtois.
    Qui aura l’impression d’être quand on dévoile la romine ?

Fluent, confident, and almost entirely wrong. Mean `avg_logprob` **−0.87**, against
**−0.34** for the Hécube mixed feed. The tracker rejected 1984 segments because there
was nothing in them to match; asking it to follow this is asking it to track noise.

That is a cleaner result than a partial success would have been. It separates the two
failure modes the corpus had so far confounded: Hécube proved a *mixed* feed tracks
well, and Lazzi shows the limit is not mixing but **intelligibility at the microphone**.
Level (finding L) predicted tracking within a production; this says the same across
productions, at the extreme.

What it does not yet establish is whether the limit is the room or the model — `small`
was used throughout Phase 0, and the PRD recommends `medium` for non-English shows.
That test is the obvious next one and costs nothing but time.

Until it is run, the honest reading is: **close mics are a requirement, not a
preference**, and a production that cannot provide them is out of scope rather than
merely degraded.

### P. The tracker optimises the wrong thing

Put by the operator, and it reframes the problem: the job is to turn the page and to
prod a technician whose attention has drifted. Not to transcribe, and not to *match* —
to know **where we are and how sure we are**. Those are different objectives, and the
engine has been built for the wrong one.

The tracker decides. Each segment is scored, and if the best score clears a threshold
the position moves; otherwise the segment is discarded. On Lazzi that discarded 1984
of 2199 segments and reached 6 % of the script, because no single segment ever looked
good enough to act on. But 1984 unusable observations are not 1984 *empty* ones.

`position_filter.py` keeps a distribution instead — `p(line | everything heard)`,
updated by a motion model (the show advances at a knowable pace, smeared) and an
observation model scored on **character trigrams**, since a recogniser that hears
"romine" for "ruine" has still delivered most of the trigrams. Word matching scores
that zero, which is precisely why the hard tracker starves on a bad channel.

On the same garbled Lazzi transcript the hard tracker could not use:

    250 s L0002 · 671 s L0101 · 1491 s L0252 · 2826 s L0497 · 4386 s L0769 · 6744 s L0996

Monotonic, start to end, over 1 h 53 m. The signal was always in there; the threshold
was throwing it away. Median belief within ±12 lines — about a page — is **52.9 %** on
Lazzi and **79.6 %** on Hécube.

**But the confidence is not calibrated, which is the part that was actually asked
for.** When the filter says it is 90 % sure, it agrees with the show's average pace
only 54 % of the time on Hécube and 43 % on Lazzi. Some of that gap is the filter and
some is the yardstick: linear pace is a poor proxy for where a show really is, since
scenes differ and nothing runs to a metronome. Which of the two is at fault cannot be
established with the material as it stands.

So this is where labelled onsets stop being a tidy-up task and become the blocking
one. A number like "95 % certain" is a *claim about calibration*, and calibration is
the one property that cannot be measured against a proxy — it needs ground truth, on
at least one act, and it has been item 7 on the list all along.

Two further things the trajectories show. The filter inherits finding M's
repeated-passage problem and its leak term makes it worse: Hécube throws it back to
L0029 and L0112 late in the show, which is the scene-2 copy of the scene-4 text.
And confidence and accuracy are not the same failure — a filter can be sure and wrong,
which for an operator is worse than being unsure, so the prodding behaviour must key
off the *distribution's* shape rather than off the peak alone.

### Q. How good does the map have to be? Measured, and the errors are asymmetric

The GPS framing, made precise by the operator: the map is known, the route is decided,
position is the only live parameter. So what does map quality actually buy? Hécube can
answer, because its cuts exist in three forms: still in the script (as imported), the
operator's grey marks (ground truth), and the audio-derived proposals.

Same two nights, three maps:

| map | 16 Nov reached | 17 Nov reached | med err |
| --- | --- | --- | --- |
| as imported, cuts still in (984)   | 60 % | 58 % | 4–5 % |
| operator's grey cuts removed (958) | 63 % | 63 % | 5–6 % |
| grey + audio proposals (947)       | 64 % | 64 % | 5–6 % |

And one more row, run by accident and kept deliberately: applying the audio proposals
**by stale line-ID** — the IDs predated an importer fix that renumbered the script, so
18 removals landed on the wrong lines, several of them performed. Night 17 collapsed
to **16 % reached, p90 error 49 %**.

Three conclusions, one per row-gap.

**A stale-but-superset map already works.** Cuts still in cost four points of reach
and nothing in error. The tracker walks past text nobody says the way a driver ignores
a closed road on the map: it is dead weight, not a wrong turn.

**Right cuts are worth having, not worth much.** Four to six points, ~10 % fewer
wandering moves. This is the ceiling on what cut-editing in prep can buy.

**Wrong removals are catastrophic.** Take out a line that is performed and the show
arrives at text with no home; the matcher snaps it to the nearest thing that fits —
confidently, elsewhere. The GPS equivalent is exact: a road missing from the map does
not read as "unknown road", it reads as *you are on that other road over there*.

So the asymmetry is the design rule, now measured rather than principled: **extra text
is cheap, missing text is ruinous** — which is why cuts are marked and never deleted
(notation §2, principle 5), and why prep proposals must pass a human. It also fixes
what "how good must the script be" means: it must *contain* the performance. It need
not contain only the performance.

Two corollaries. The repeats are the play — *"Tranquille… On est large"* is pacing,
not noise — and they stay in the map and stay matchable; the twin annotation demands
more evidence for them, it never removes them. And the two cut sources are
complementary with almost no overlap (3 lines of 37): a repeated line that is cut can
never be certified by audio, because its twin is still heard elsewhere, while the
operator's marks catch exactly those. Neither source suffices alone.

Last, the stale-ID accident is the Phase 1 lesson: annotations keyed to positional
IDs do not survive a re-import. Line identity must be content-derived — which is what
M1.4's hash-based IDs are for, now with a measured failure to justify them.

### R. Inside the operator's window: on the line, or visibly lost — rarely in between

The operator's question, verbatim: *on a 6-line window, on average, how often are we
off the mark?* Answerable without hand labels by using the recording's own
unambiguous moments as spot checks — a kept segment matching exactly one script line
strongly, no runner-up close (repeats excluded by the uniqueness test itself), pins
the show to that line at that instant. ~175 such anchors per Hécube night, one every
40 s or so. `window_accuracy.py`.

The caveat first: anchors exist only where the ASR heard something clean, which
correlates with the tracker doing well. These are optimistic bounds — the honest
reading is "when the show is knowable, is the display right?"

| corpus | in a 6-line window | median error | p90 |
| --- | --- | --- | --- |
| Hécube 16 Nov (mixed feed) | **91 %** | 0 lines | 3 |
| Hécube 17 Nov | **83 %** | 0 lines | 48 |
| Lazzi (ambient) | 35 % | 25 lines | 459 |

The distribution matters more than the average: it is **bimodal**. Median error is
*zero* — when the display claims a position it is typically on the exact line, not
merely near it. And when it is wrong it is wrong by tens to hundreds of lines, never
subtly. The misses are not a fringe of ±5s; they are three episodes: the show's open
before the first fix settles, the scene-2/4 Euripides twins (finding M, in both
directions — and a few "misses" are the *anchor* fooled by the twin, so the
measurement inherits the problem it measures), and one five-minute lost stretch on
night 17 pinned at L0719.

For the GPS UX this shape is the good one. The killer failure for operator trust is
subtle wrongness — a page that looks plausible and is one scene off. That mode is
nearly absent: the display is either right or *visibly, declaredly* lost, and the
lost state already has its contract (say so, ask for guidance). It also says the
6-line window is the right size to publish: nothing is gained below ±3 (91 % ≈ 91 %
at ±5) because there is almost no mass at small-but-nonzero errors.

Found en route, embarrassingly on theme: the first run of this measurement compared
current-script anchors against traces tracked on the *old 996-line script*, and
produced a beautifully tight median error of 10 — which was the index skew between
the two script versions, not tracking error. Same lesson as finding Q, third
occurrence today: nothing keyed by line index survives a re-import.

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

### S. Watching it run: what an operator noticed in the first hour

The live spike (`serve`) puts the script on a screen and plays the show through the
speakers while it tracks. Four things surfaced in the first hour of somebody actually
watching that no offline metric had produced in a week.

**Playback and analysis must not share a thread.** The first version pushed audio from
inside the engine loop, so every Whisper decode — ~600 ms — stalled the feed to a
250 ms buffer. It warbled continuously. The compute budget was never the issue (8.4×
real time on this material); sharing a thread was.

**A buffer of exactly one block is not a buffer.** Capacity was `buffer_ms`, the
producer hands over 250 ms at a time, so it had to wait for the queue to run *dry*
before every push and the gap went out as silence. Worse than the noise: each inserted
silence pushes the audio content later, so the sound fell steadily further behind while
both clocks stayed technically correct. The operator described it exactly — "it's
getting far ahead of the sound".

**The display must never lead the voice.** The recogniser is handed a block before that
block reaches the speaker, so without gating the screen announces a line before it is
audible. That reads as broken even when the tracking is perfect. Updates are now held
until their audio has been heard: the display may lag, never lead.

**And the operator asked for the thing that actually fixes being lost:** click the line.
Finding R says that when the tracker is wrong it is usually wrong by a *lot*, and
`lost_search_all` can take a minute to recover — during which the person in the room
can see the page and knows the answer. `Tracker::set_position` already existed for
exactly this and had no route from a UI. It is now one tap, and it is the single most
useful control in the spike.

### T. Two lines that say the same thing are not two answers

Reported from watching: *"there are places where the actors each say the words from the
original text and the detection gets lost"*. That is a mechanism, not a mood, and the
trace confirms it — **165 ambiguous rejections in one performance**, including a line
scored **1.00**, a perfect match, thrown away because its twin tied it.

The margin rule was written to keep the tracker quiet when two places fit equally well.
The assumption underneath it is that the rivals are *different places*. When they carry
the same words the assumption fails: either reading explains what was heard, so
refusing to move is the one response guaranteed to be wrong. Picking the nearer copy
costs a line or two; freezing costs the scene, because the decay timer then runs to
lost and needs a human.

`PreparedLine` now carries a hash of its normalized text and identical lines no longer
silence each other (`equivalent_text_competes`, default off, switchable to keep it
measurable). Measured: night 16 **91 % → 92 %** in a 6-line window with p90 error 3 → 2
lines, ambiguous rejections 165 → 147; night 17 unchanged at 83 %. Real, and smaller
than hoped — the test is exact text equality, while many ambiguities are between
genuinely different lines that merely score alike.

### U. The matcher is built for speeches; a quarter of this play is not

Also reported from watching: *"fast dialogue is problematic somehow"*. Measured on
night 16:

| | accepted segments | rejected segments |
| --- | --- | --- |
| median words | 14 | 6 |
| under 5 words | 9 % | **42 %** |

And **268 of 984 lines (27 %)** of Hécube are under five words. Short text is weak
evidence, and the weak-evidence path narrows deliberately to one line ahead, no
multi-line spans, no landmark recovery — "Yes" is not allowed to relocate the show.
That is right in isolation and wrong in aggregate: in a rapid exchange, if the position
is off by a single line the correct answer is not in the window at all, so everything is
rejected until the tracker decays to lost.

This is the clearest remaining structural gap, and unlike the others it is not about
audio at all. Candidate direction: on a zone or mixed feed a short segment should still
be allowed multi-line spans, since one VAD segment routinely covers three short
exchanges. It needs measuring against both nights before it is believed.

### V. The lexicon earns its place; the matchers are redundant

The full 2×2 the operator asked for — lexicon on/off × matcher — on both Hécube
nights. The two act at different stages, so it costs two transcriptions rather than
eight: the lexicon changes recognition, the matcher changes only matching.

| lexicon | matcher | night | reached | lost | time lost |
| --- | --- | --- | --- | --- | --- |
| off | words | 16 | 611 | 27 | 758 s |
| off | both | 16 | 618 | 23 | 749 s |
| **on** | **both** | 16 | **621** | **21** | **537 s** |
| off | words | 17 | 559 | 34 | 1205 s |
| off | both | 17 | 570 | 34 | 1200 s |
| **on** | **both** | 17 | 586 | **23** | **807 s** |

**Time spent lost falls 28 % and 33 %** with the lexicon — far more than any matcher
change bought, and in the direction argued against an hour earlier on the strength of
watching it produce `Les, Polyxènes, Troie, Les, Polyxènes, Troie…`. That recitation is
real; the hallucination filter catches it; the net is strongly positive. Best cell is
both together on both nights, so the two are complementary.

`window_accuracy` is deliberately absent from that table. Its anchors are derived from
the transcript, so changing the transcript changes the denominator and the percentages
stop being comparable down the lexicon column.

**The two matchers, though, are near-identical.** Racing characters+sound against
words-only over night 16: they agree **96.4 %** of sampled moments, the lead changes
hands 5 times, median gap 0 lines — but the maximum gap is 711. So the alternating lead
an operator sees is real, rare, and the only part that matters; the rest is two names
for the same tracker.

### W. What watching it changed about the display

None of these came from a metric.

- **The reading line sits a third down, not centred.** The operator needs what is
  coming; centring spends half the screen on text whose only job is to confirm you were
  right.
- **The scroll anticipates, the highlight does not.** After a match goes quiet the
  viewport drifts partway toward the next line, because a reader's eye is already there
  before the actor finishes. The highlight stays on confirmed ground — it is a claim,
  the scroll is only comfort.
- **A one-line move eases; a relocation cuts.** Animating a jump across scenes implies
  a journey that did not happen.
- **No reading band.** A tinted strip across the current line sounded helpful and was
  glare: the current line is already the only bright thing on a dim page.

### X. A third kind of repetition: the running gag

Findings M and T covered two shapes — the same passage performed twice hundreds of
lines apart, and lines whose text is *identical*. Watching a live run turned up a
third, and it is the one a play is most likely to contain.

An operator saw the position jump backwards and named the moment: *"Étrange et
mystérieux"*. The script around it:

    L0111  sc-2  ÉRIC   C'est très mystérieux. Je pense que la statue de chienne…
    L0113  sc-2  GAËL   C'est un peu étrange.
    L0114  sc-2  DENIS  C'est étrange et symbolique.
    L0115  sc-2  GAËL   Mais plus étrange que symbolique.
    L0148  sc-2  DENIS  Ah, oui. Symbolique. Mais un peu étrange.

A company riffing on two words across thirty-seven lines. Not duplicates, so the
identical-text rule does not see them; not distant, so the twin detector in
`prep_report.py` — which requires a 20-line gap — deliberately skips them. They are
*near*-duplicates at close range, which is precisely the configuration character and
phonetic matching bring closer together.

**Measured, today's matchers did not cause it.** Night 16 has 11 backward jumps of
10+ lines either way, 2480 vs 2425 lines travelled backwards, and *zero* landing in
this run under either configuration. The live jump is not reproducible from the
recorded transcript, because a fresh Whisper pass on Metal is not bit-identical — so
it is real, observed, and outside what the offline harness can replay. Worth stating
plainly: the regression test cannot see this class of event at all.

The prep answer is probably not the twin detector but the opposite of it — a
*near*-duplicate detector with no minimum gap, whose output is not "these are the
same" but "these will be confused". Untested.

Alongside it, the operator's verdict after an hour of watching: smooth, never totally
lost, and **the position never left the visible page**. That is the PRD requirement in
its original words — the last spoken line in the middle third of what is displayed —
reported from the chair rather than computed from a trace.

### Y. The length penalty was the biggest single win, and "latency" was not latency

Reported from the chair: *"longer text blocks at faster speed give the tracking
mechanism difficulties. Shouting too. I can see the words are not that bad in the
transcript, but it's not following."*

That is a description of `overlap_exp`. The matcher multiplies its score by an overlap
term that penalises a segment for carrying more words than the line it matches, and a
long fast speech produces exactly that — a segment far longer than any single line, so
the score collapses however well the words agree. Shouting compounds it, because the
voice detector holds the run open longer.

Swept on both nights, time spent lost:

| exponent | night 16 | night 17 | losses | lines reached |
| --- | --- | --- | --- | --- |
| 0.50 | 560 s | 778 s | 22 / 21 | 669 / 647 |
| **0.35** | **253 s** | **153 s** | **10 / 11** | **691 / 686** |
| 0.25 | 124 s | 297 s | 8 / 9 | 692 / 667 |
| 0.15 | 195 s | 117 s | 11 / 8 | 668 / 667 |

0.35 rather than the lowest number: best across both nights on lines reached, and all
21 tracker scenarios still pass. **At 0 they do not** — which is why this dial sat
untouched all day. Three proxy measurements said the term cost accuracy and the
scenarios said removing it cost correctness; neither could settle it without a corpus,
and two full nights could.

Ruled out first, both worth recording as negatives: it is **not** the span limit (only
2 % of rejected segments need more than `max_span` = 3 lines, so segments are not
outrunning spans, they are being scored down), and **not** the engine falling behind
(no backlog reported).

**Then: "latency has dropped noticeably."** It had not. Median time between line
changes is 4.9 s before and 5.0 s after; p90 identical; the pipeline still takes its
usual ~600 ms. What changed is where the time is spent:

| | word | line | block | lost |
| --- | --- | --- | --- | --- |
| before | 9.3 % | 32.4 % | 48.2 % | **10.1 %** |
| after | 10.3 % | **42.2 %** | 43.1 % | **4.4 %** |

**Latency from the operator's chair is time-without-a-trustworthy-position, not
milliseconds through the pipeline.** Being lost or uncertain is what feels like lag,
because the page holds still while the room moves on. The lever was in the matcher, and
chasing it as a performance problem — smaller model, shorter interims, more threads —
would have spent the effort in the wrong place and made recognition worse on the way.

### Z. Rarity weighting is wrong here, and why that is not obvious

Asked for: lightweight, fast semantic matching that copes with paraphrase. The cheapest
candidate is IDF weighting — count each token by how rare it is in the script, so
`Polymestor` outweighs `le`. It needs no model, no download and no time in the hot
path, and it is standard practice in every retrieval system.

Measured on night 16, every script line against the passage that best explains it:

| band | equal weights | IDF weighted |
| --- | --- | --- |
| not found (<0.30) | 10 | **36** |
| paraphrased (0.30–0.62) | 298 | 287 |
| recognisable (0.62–0.85) | 272 | 268 |
| as written (>0.85) | 191 | 180 |

Mean 0.680 → 0.662, with 317 lines worse against 128 better. **It hurts.**

The reason is specific to this problem and inverts the usual assumption. IDF assumes
rare words are *reliable* — in a document collection they are, because the text is what
it is. Here the text arrives through a recogniser, and finding on the same corpus was
that the rare words are precisely the ones it destroys: `Hécube` → *cubes*, `Polyxène`
→ *problème*, `Euripide` → *épisode*. Weighting by rarity is therefore weighting by
unreliability, while the function words that survive recognition intact get discounted
for being common.

Two consequences worth keeping. Any scheme that leans on distinctive vocabulary —
IDF, keyword extraction, landmark-only matching — inherits this, and the lexicon
prompt is the *right* response to the same fact, since it repairs the rare words
rather than trusting or discounting them. And the paraphrase band is 298 lines, so the
question stands: the answer is more likely learned alternates, which record what the
company actually says (built, and measured to generalise across nights in finding K),
than sentence embeddings, which would raise the floor under every candidate at a
moment when the measured problem is ambiguity rather than absence of a match.
