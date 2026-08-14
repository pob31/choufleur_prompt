# Live spike — script following on screen, audio running in

*A vertical slice through Phase 2 and Phase 3, cut as narrow as it will go: audio goes
in, the script scrolls on a screen, nothing else.*

## What this is

The Phase 0 work proved the engine can follow a show offline. It has never been
watched. This spike puts a screen in front of it and runs Hécube's mono mix in, so
that the thing the operator actually experiences — text moving under their eyes at the
right moment — exists and can be judged.

Hécube is the right corpus: one mono channel, monolingual, two full performances
already transcribed and tracked, and a measured baseline to compare the live run
against (**91 % / 83 % of glances inside a 6-line window**, finding R).

**In scope.** Audio in → position out → script scrolls, centred on the current line,
with an honest indication of confidence and a loud one when the position is lost.

**Out of scope, deliberately.** Cue warnings. Notes and annotations. Editing. Prep.
Operator controls beyond "follow / don't follow". Auth, multi-show, persistence,
localization. All of that is Phase 2–4 and none of it is needed to learn what this
spike is for.

**What it is not:** it is not M2.2. The real server is a separate `choufleur-server`
axum binary speaking the full typed protocol. This is a `serve` subcommand on the
replay CLI that reuses the existing streaming engine, so the display exists in days
rather than after the whole of Phase 2. It graduates and is deleted.

## Architecture

The engine already has the seam. `Engine::run(&corpus, &mut dyn Consumer)` drives
`WAV → resample → VAD → Whisper → filter → Consumer`, and `track` already wraps the
tracking consumer in a `Tee` to capture segments. Broadcasting is another wrapper —
no engine change at all.

```
  ┌─ std::thread (blocking, single-threaded, owns Metal) ──────────┐
  │  Engine::run(corpus, &mut Broadcast { inner: TrackConsumer })  │
  │      … existing path, untouched …                             │
  │      on_segment → tracker.update → TrackerEvent[]              │
  └───────────────────────┬───────────────────────────────────────┘
                          │ tokio::sync::broadcast<Update>
  ┌───────────────────────▼───────────────────────────────────────┐
  │  axum:  GET /            → the page (one self-contained HTML)  │
  │         GET /script.json → the map, once                       │
  │         GET /ws          → hello + position stream             │
  └───────────────────────┬───────────────────────────────────────┘
                          │ WebSocket
                      the display
```

Three properties worth stating because they are what make this cheap:

- **The engine stays synchronous.** It runs on one `std::thread`; the async runtime
  never touches Whisper. The only bridge is a `broadcast` channel.
- **Position is state, not a log.** A slow or reconnecting client wants the *latest*
  position, never a backlog, so the channel is lossy by design and every connection
  opens with a full snapshot.
- **Typed messages only, no display prose** (PRD §protocol). The page renders its own
  words from `confidence` and `lineIndex`.

## Protocol

Deliberately a subset of the PRD's table, with the same names and shapes so it is a
prefix of the real thing rather than a detour.

| direction | message | payload |
| --- | --- | --- |
| S→C on connect | `hello` | `{ protocol, showTitle, lineCount, position }` |
| S→C | `position_update` | `{ seq, lineId, lineIndex, confidence, tAudio }` |
| S→C | `run_state` | `{ tAudio, xrt, fellBehind }` — once a second, for the footer |

`confidence` is the existing ladder: `word | line | block | scene | lost`. `seq` is
monotonic per run. No client→server messages exist in the spike; the page is a
read-only display and "follow" is local state in the browser.

## The display

This is the part the corpus has opinions about, so the rules below are consequences of
measurements rather than taste.

**Centre the current line; scroll the text under it.** The operator reads the page,
not the line (their words). Measured median error when confident is **0 lines**, so
centring the exact line is honest rather than optimistic.

**Show about ±10 lines.** p90 error on a good night is 3 lines, and there is almost no
probability mass at small-but-nonzero errors (finding R), so a window a few lines wider
than p90 puts the truth on screen essentially whenever the system is right at all.
Widening further buys nothing and costs legibility.

**Confidence is a visual weight, not a number.** `word`/`line` render solid; `block`
softer; `scene` dimmed. A percentage would be a lie until calibration is measured
(finding P).

**Lost must be loud.** The error distribution is bimodal — on the line, or off by tens
to hundreds, with almost nothing in between (finding R). So the failure the UI must
serve is not "slightly wrong", it is "confidently elsewhere". On `lost`, the page says
so plainly and stops pretending to scroll. This is the GPS "recalculating", and it is
the single most trust-preserving element in the spike.

**Animate small moves, cut large ones.** A one- or two-line advance eases; a
relocation jumps. Smoothly gliding across 300 lines would both look absurd and hide
the fact that something significant happened.

**A "follow" toggle, and nothing more.** Scrolling by hand turns it off; one control
turns it back on. Any richer browsing is M3.3.

## Staging

### Stage A — file paced against the wall clock *(the deliverable)*

`--realtime` already exists: `VirtualClock` sleeps until `start + t_audio` per block,
and `wallLatencyMs` per segment is already measured against the audio deadline. So
Stage A needs **no new audio path at all**.

```
choufleur-replay serve test_Choufleur/HecubePasHecube_TiagoRodrigues/manifest-20241116.json \
    --realtime --port 8080
```

This is the honest majority of the risk: transport, display, scroll behaviour, latency
under a real 2-hour run. It has no hardware dependency and is fully reproducible.

### Stage B — real capture

Replace the WAV reader with `cpal` input, feeding the identical
`ChannelFrontend::push_block_48k`. This is M2.1 proper, and its done-when is the
virtual soundcheck: play the corpus WAV out of the interface, capture it back, and the
live trace should match the offline one within tolerance.

Stage B is where the offline/live seam actually gets de-risked. Stage A is where the
display gets designed. Doing A first means B is debugging one component, not three.

## Work items

| # | item | done when |
| --- | --- | --- |
| 1 | `choufleur-protocol`-lite: `Update` enum, serde, in `choufleur-replay` | round-trips in a unit test |
| 2 | `Broadcast` consumer wrapping the tracking consumer | offline `track` output byte-identical with it inserted |
| 3 | `serve` subcommand: thread + axum + `/ws` + `/script.json` | `websocat` receives `hello` then a stream of `position_update` |
| 4 | The page: one self-contained HTML file, virtualized list | 984 lines render and scroll at 60 fps on a laptop and a tablet |
| 5 | Confidence styling + lost banner + follow toggle | each state reachable by replaying a trace with known events |
| 6 | Stage A full run | Hécube 16 Nov plays end to end, never falls behind, page follows for 2 h |
| 7 | Live-vs-offline comparison | `window_accuracy.py` on the live trace within a few points of 91 % |
| 8 | *(Stage B)* cpal capture behind the same frontend | loopback trace matches offline within tolerance |

Items 1–5 are a couple of days. Item 6 is a two-hour wall-clock run per attempt, so it
wants to be started early and watched.

## Risks

**Latency looks worse than it measures.** Measured p95 was 1092 ms at four concurrent
channels; one mono channel should be far better, and Hécube transcribes at 8.4× real
time so there is ~8× headroom. But interim hypotheses land every 1.5 s, and *perceived*
lag is set by the interims, not the finals. If it feels sluggish the first lever is
`--interim-ms`, not the model.

**Position jitter from interims.** Interim segments move the position mid-line, which
is exactly how the lag budget is met, but on screen it may read as twitchy. Mitigation
is display-side smoothing, never suppressing the interim — the information is real.

**A 2-hour run is a long feedback loop.** Keep a 5-minute excerpt manifest for
iteration and use the full run only as a gate.

**Metal in a thread.** The engine already owns one reused `WhisperState`; keeping it on
a single dedicated thread preserves that. Do not be tempted to make the engine async.

**The spike becomes the product.** It reuses the replay CLI and skips the typed
protocol crate, auth, and show files on purpose. It should be deleted at M2.2, and the
plan should say so in the code.

## What this buys

It answers questions no offline metric can. Whether a display that is right 91 % of the
time *feels* trustworthy. Whether the lost banner reads as honest or as broken. Whether
1 second of lag is invisible or maddening. Whether an operator watching text scroll for
two hours is helped or hypnotised — which is, in the end, the thing the product claims
to fix.

It also front-loads the two Phase 2/3 seams most likely to hide a nasty surprise: the
engine→transport boundary, and the offline→live audio boundary. Both are cheaper to
find now than after the protocol and client are built on top of them.
