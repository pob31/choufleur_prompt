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

That number is less alarming than it looks, and worth being precise about. A show
run starts at the top, where the tracker already is — that is the 2.5 s row, and it
is the normal case. The minutes apply to **starting cold in the middle**: recovery
after a long loss, or beginning a rehearsal at an arbitrary point. The PRD already
answers the second with quick-jump, which sets the position directly and costs
nothing. So the honest reading is that unaided global location works and is worth
having as a backstop, while remaining slower than telling it where to start.

Widening the search to include the initial `Scene` state cut two of the cold starts
by more than half (excerpt 4: 220 s → 70 s; excerpt 5: 231 s → 138 s) and moved
excerpt 6 onto its exact first line.

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
