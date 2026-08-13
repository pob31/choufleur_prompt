# server/

The Rust workspace: tracking engine, speech recognition, and the offline replay
harness. See [`../docs/choufleur-devplan_1.md`](../docs/choufleur-devplan_1.md)
for where this sits in the plan and
[`../docs/choufleur-phase0-notes.md`](../docs/choufleur-phase0-notes.md) for what
building it has taught us so far.

## Prerequisites

- Apple Silicon (M1 or newer), macOS 14+
- Rust stable (`rustup`), 1.82 or newer
- Xcode Command Line Tools and `cmake` (`brew install cmake`) — needed to build
  whisper.cpp when the recognition stage lands
- ~2 GB of disk for models

```bash
cargo test          # everything below needs no models and no audio
cargo bench -p choufleur-core
../scripts/fetch-models.sh
```

## Crates

| Crate | What it is |
|---|---|
| `choufleur-core` | The tracking engine. Normalization (notation §3.2), language-aware matching, span enumeration, and the position tracker. **Pure**: no I/O, no async, no clock — time arrives on transcript segments, which is what makes replay and live runs the same computation. |
| `choufleur-asr` | Buffers in, transcript segments out. The VAD segmentation policy and the hallucination filter are here; the model-bound stages (resampling, Silero, Whisper) are the next milestone. Knows nothing of files, devices, or the tracker. |
| `choufleur-replay` | The CLI harness: build or verify a corpus, track it, score it. The regression and tuning backbone, permanently — not a prototype. |

Dependency direction is one-way: `core` depends on nothing internal, `asr` depends
on `core` for its types, `replay` depends on both.

## The replay CLI

```bash
cargo run -p choufleur-replay -- make-fixture corpus/fixture-smoke
cargo run -p choufleur-replay -- verify       corpus/fixture-smoke
cargo run -p choufleur-replay -- track        corpus/fixture-smoke --segments out/segments.jsonl -o out/trace.jsonl
cargo run -p choufleur-replay -- eval         corpus/fixture-smoke --trace out/trace.jsonl --pretty
```

`eval` exits non-zero when the devplan's go/no-go gate is not met, so it works as
a regression check without anyone parsing the report.

### Files it passes around

All JSONL, one self-describing record per line, so every intermediate stage is
greppable and diffable.

| File | Written by | Holds |
|---|---|---|
| `manifest.json` | you, or `make-fixture` | channels, character map, SHA-256 per audio file |
| `script.json` | you, or `make-fixture` | the interim Phase 0 script format (notation §11's show file replaces it in Phase 1) |
| `ground-truth.jsonl` | `research/align.py` + a human | line → onset, the basis of every measurement |
| `segments.jsonl` | `transcribe` | what was heard, per channel, with ASR quality signals |
| `trace.jsonl` | `track` | every tracker decision — **including every rejection** |
| `report.json` | `eval` | coverage, lag, confident-wrong, recovery, gate result |

The trace records refusals as carefully as advances. An eval that can only see
where the tracker went cannot explain why it stayed put, and "why did it lose act
two scene three every night" is the question this whole harness exists to answer.

## Tuning knobs

`TrackerConfig` (pass a JSON file to `track --tracker-config`) and `VadConfig`
carry per-field documentation; fields marked **[sweep]** are the dials M0.4 turns
against the real corpus. Two constraints are not obvious and are easy to break:

- `prior_floor` must stay above `accept_threshold`, or a perfect match a few lines
  ahead becomes unacceptable and `window_ahead` is silently decorative.
- `interim_interval_ms` trades compute for latency directly. Setting it to 0
  restores end-of-utterance segmentation, which puts a floor under detection lag
  equal to the length of the line being spoken.

## Known v0 limitations

Stated rather than discovered later:

- No within-line position fraction; the tracker resolves to a line.
- Per-channel hypothesis fusion is sequential, not probabilistic — segments are
  applied in global timestamp order with a character-identity weight. There is no
  multi-hypothesis blending.
- Inverted lines stay unrecoverable until the next landmark. This is by design
  (PRD, forward-only constraint) and is why landmark coverage matters.
- Mic bleed is handled structurally, by per-channel identity and the
  character-mismatch penalty, not by level-thresholded attribution. The latter
  belongs upstream in the capture path and is not built yet.
