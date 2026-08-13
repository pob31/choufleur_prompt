# Corpus

Evaluation recordings for the tracking engine. **Audio never enters git**; the
manifests, scripts and ground-truth labels do. A manifest carries a SHA-256 per
audio file, so a corpus that has drifted is caught rather than silently re-scored.

```
corpus/
├── README.md              ← this file
├── fixture-smoke/         ← synthetic, regenerate with make-fixture, never committed
└── <show>-<act>/
    ├── manifest.json      ← committed
    ├── script.json        ← committed
    ├── ground-truth.jsonl ← committed
    ├── ch01-<character>.wav   ← NOT committed
    └── mixdown.wav            ← NOT committed
```

Audio lives wherever you keep it — an external drive is expected. Point the tools
at it with `--audio-root /Volumes/…/choufleur-audio`, which re-bases audio paths
only, leaving the committed files where they are.

---

## 1. Exporting a recording

One **full act** is the unit. Half an act tells you nothing about drift, and a
whole show is more labelling than the question needs.

From the console's virtual soundcheck recording, or the DAW capture of it:

| | |
|---|---|
| Format | WAV, mono, **48 kHz**, 16- or 24-bit PCM |
| One file per channel | `ch01-marie.wav`, `ch02-jean.wav`, `ch09-zone-dsl.wav` |
| Also export | `mixdown.wav` — a mono mix **of the same take** |
| Processing | High-pass filter only. No gate, no compression, no de-noise |
| Alignment | Every file must start at the same instant and be the same length |

That last row matters more than it looks: all timestamps are relative to a single
shared timeline, so a channel exported from a different start point silently
shifts every measurement made from it. `choufleur-replay verify` checks lengths
agree, but it cannot detect a common offset.

**Zone channels.** An ambient or area mic carries no speaker identity. Leave
`character` off its manifest entry (the field is optional) and name the file for
where it is on stage, not who it hears.

**Consent and provenance.** Recording is the venue's existing console workflow and
its consent implications stay there — Choufleur records nothing. Note in the
manifest's `provenance` who was recorded, when, and under what agreement, so the
next person to open the corpus knows what they are holding.

---

## 2. Writing the manifest and the script

Copy a generated fixture as the starting shape:

```bash
cargo run -p choufleur-replay -- make-fixture /tmp/shape
```

The **script** is the interim Phase 0 format: a flat list of lines with `id`,
`act`, `scene`, `character`, `text`, optional `lang`, optional `landmark`. It is
deliberately not the show file of notation §11 — that arrives in Phase 1 and
replaces this, at which point the eval is re-run to confirm nothing moved.

Line ids are `L-0001`, `L-0002`, … in script order. Tag a handful of lines with
`"landmark": 3` — unmistakable, unique phrases. They are what the tracker
re-anchors on after it loses its place, and a corpus with none of them cannot
exercise recovery at all.

Then fill in the hashes and check the whole thing:

```bash
cargo run -p choufleur-replay -- verify corpus/<show>-<act> --update-hashes
cargo run -p choufleur-replay -- verify corpus/<show>-<act>
```

---

## 3. Labelling the ground truth

`ground-truth.jsonl`, one record per line of script:

```json
{"lineId":"L-0142","onset":812.44,"end":816.10,"channel":3}
{"lineId":"L-0143","onset":816.90,"end":819.02,"channel":5}
```

Labelling from scratch is a day's work per act. Aligning first and correcting
afterwards is an hour or two:

```bash
cd research && python3 -m venv .venv && . .venv/bin/activate && pip install -r requirements.txt
python align.py ../corpus/<show>-<act>
```

That writes `gt-draft.jsonl` plus `gt-draft.labels.txt`, an Audacity label track.
Open `mixdown.wav` in Audacity, **File → Import → Labels**, correct, then
**File → Export → Export Labels** and convert back:

```bash
python align.py ../corpus/<show>-<act> --from-labels gt-draft.labels.txt
```

### Conventions

- **Onset** is the first audible phoneme of the line. Target ±200 ms; this is the
  number every lag measurement is made against.
- **End** is coarse — it only defines speech-active time, so being half a second
  generous costs nothing.
- **Overlapping dialogue** is fine and expected. The eval's rule is *latest onset
  wins*: at any instant, the expected position is the line that most recently
  started.
- **A line not performed in this run** gets `"omitted": true` rather than being
  deleted. Deleting it makes the corpus disagree with its own script; marking it
  keeps the record of what was cut. Do this for every line the actors skipped,
  or the eval will score the tracker against material that was never spoken.
- **Improvised or off-script speech gets no label.** It is not in the script, so
  it is not speech-active time — which is exactly right: the tracker is not being
  tested on material it could not possibly match.

---

## 4. Running the eval

```bash
cargo run --release -p choufleur-replay -- transcribe corpus/<show>-<act> -o out/segments.jsonl
cargo run --release -p choufleur-replay -- track     corpus/<show>-<act> --segments out/segments.jsonl -o out/trace.jsonl
cargo run --release -p choufleur-replay -- eval      corpus/<show>-<act> --trace out/trace.jsonl --segments out/segments.jsonl --pretty -o out/report.json
```

Keep `segments.jsonl` from a good transcription run. Matcher and threshold work
then re-runs in milliseconds against a fixed transcript, and — unlike the ASR
stage, whose Metal arithmetic is only reproducible on the same machine — that path
is bit-identical anywhere. It is the pinned regression baseline.

---

## The fixture corpus

`corpus/fixture-smoke/` is generated, not recorded:

```bash
cargo run -p choufleur-replay -- make-fixture corpus/fixture-smoke
```

Synthesized speech has perfect diction, no reverb, no bleed, no overlap and no
audience. It proves the pipeline runs end to end and stays deterministic. It
proves **nothing** about the go/no-go gate, and thresholds must never be tuned
against it.
