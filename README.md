# Choufleur

*A script GPS for theatre technicians.*

Choufleur follows a live performance in real time by listening to the actors' microphones, keeps every technician's screen on the right page of the script, and taps them on the shoulder before their cues.

It **never triggers anything**. Sound, lighting, video, flys — the operator always makes the call. Choufleur just makes sure that if you're deep in an EQ tweak when your next cue approaches, you get a peripheral nudge in time to refocus.

## How it works

- A **server** (macOS primary) takes per-actor audio feeds from the console — Dante or direct outs, up to 16 channels — and runs local speech recognition (Whisper) against the known script. Fully offline; no internet dependency.
- Any device on the venue network opens the **web client**, joins the show, and picks the cue list that concerns it (LX, sound, video, flys, stage management). Each operator sees the shared script position, their own cues, and their own notes.
- Warnings are configurable per cue (standby/final lead times) and deliberately peripheral — a red frame at the edge of vision, not an alarm.
- The system tracks position with honest confidence levels. When it's lost, it says so and asks for help: the first operator to respond can scroll position for everyone until tracking re-locks.
- Multilingual by design — including several languages within one play, down to individual bilingual lines.
- Rehearsal-friendly: jump to any scene or cue, pause while the director gives notes, loop the same passage, and re-import a rewritten script without losing a single cue or note.

## Status

**Phase 0 — tracking-engine risk spike.** The offline harness exists: the tracking
engine, the corpus tooling, and the evaluation that decides go/no-go. The
recognition stage is not wired up yet, and no real theatre audio has been tracked,
so the question Phase 0 exists to answer is still open.

```bash
cd server && cargo test                                    # 104 tests, no models needed
cargo run -p choufleur-replay -- make-fixture corpus/fixture-smoke
cargo run -p choufleur-replay -- verify corpus/fixture-smoke
```

| Document | Contents |
|----------|----------|
| [docs/choufleur-prd_1.md](docs/choufleur-prd_1.md) | Product requirements — architecture, tracking, warnings, protocol, platform |
| [docs/choufleur-notation_1.md](docs/choufleur-notation_1.md) | Normative spec — cue notation, line identity, language tagging, show file format |
| [docs/choufleur-devplan_1.md](docs/choufleur-devplan_1.md) | Development plan — phased milestones, go/no-go gate, workspace layout, test strategy |
| [docs/choufleur-phase0-notes.md](docs/choufleur-phase0-notes.md) | What building it has taught us, including two findings that change the design |

## Repository layout

```
choufleur/
├── server/          # Rust workspace — see server/README.md
│   └── crates/
│       ├── choufleur-core     # tracking engine: normalization, matching, position
│       ├── choufleur-asr      # buffers in, transcript segments out
│       └── choufleur-replay   # offline replay, tracking and evaluation harness
├── corpus/          # evaluation recordings — manifests in git, audio is not
├── research/        # Python sidecar for forced alignment; never in the show path
├── scripts/         # model fetching
├── remote/          # Flutter/Dart — web-first client (Phase 3, not started)
├── docs/            # PRD, notation spec, development plan, notes
├── LICENSE-MIT      # MIT OR Apache-2.0 dual license
├── LICENSE-APACHE   #
└── README.md
```

## Related projects

Part of an open source theatre tooling ecosystem, all built from direct professional practice: **Tagada** (stage position tracking), **S21 HiJack** (DiGiCo S21 snapshot extension), **WFS DIY** (open source wavefield synthesis), and an audio description workspace.

## License

Dual-licensed [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE), at your option — the Rust ecosystem convention. Show files you produce with Choufleur are your data.

---

*"It's like a GPS for your script, so you always know where you are and what's coming up."*
