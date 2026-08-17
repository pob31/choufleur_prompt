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

**Phase 0 — tracking-engine risk spike, and it has now met real theatre.** Two full
performances of *Hécube, pas Hécube* (Tiago Rodrigues, Comédie-Française) have been
tracked end to end against the operator's own script and conduite, and the display has
been watched and corrected from the chair through several complete runs.

On the operator's prepped script, tracking both nights:

| | night 16 | night 17 |
|---|---|---|
| glances landing inside a 6-line window | **93 %** | **95 %** |
| p90 position error | 2 lines | 1 line |
| jumps over 100 lines | **0** | **0** |
| time without a trustworthy position | 34 s | **0 s** |
| time at low confidence | 22.6 % | 15.8 % |

Two hours of performance, one mono mixed feed, French, against a script six months
older than the recordings. End-to-end latency is 351 ms median, and `small` runs at
6.2× real time — so the compute budget is not the constraint.

**What that took, and what it says.** Almost every gain came from the matcher rather
than from recognition, and almost every one was found by an operator watching the page
and describing what felt wrong:

- **Long moves went to zero.** Charging *confirmations* for distance rather than score
  — a move of a hundred lines must be seen more times before it is believed.
- **Silence, music and improvisation stop the clock.** A passage the script cannot
  predict is marked as such, and the tracker waits rather than treating the noise as
  evidence against where it is.
- **Long speeches hold their place.** A five-second fragment agrees with a fraction of a
  173-word line, so consecutive fragments are scored together while the position is
  stalled. Time at low confidence roughly halved.
- **A bigger model does not help.** `medium` writes a measurably better transcript —
  lines recognisable as written 84 → 120 — and tracks no better, three separate ways.
  The binding constraint is the matcher.

**A prep and live display** is served from the same binary: the script scrolling under a
continuous follower, an operator's cue sheet on a rail beside it with leader lines to
the exact phrase that fires each cue, and editors for the script, the cues and the cue
list's own vocabulary. Several devices join independently — position is shared,
everything else belongs to a list.

**An app**, signed, for the machine that runs the show. It is what makes live capture
possible at all: macOS gives a plain binary a microphone stream that runs and delivers
nothing — no error, no prompt — and only a bundle is ever asked about. It holds one
window on the library, downloads the models on first run, and takes every server with
it when it closes.

**Still open.** Multitrack has never been tracked against a corpus, which is the case
with the most to gain since knowing who is speaking should resolve most remaining
ambiguity. Near-identical lines still cause the one reproducible error. And the app is
not notarized yet, so another Mac will refuse it until it is.

Findings, including the ones that failed and why, are in
[docs/choufleur-phase0-notes.md](docs/choufleur-phase0-notes.md).

```bash
cd server && cargo test          # 170 tests; those needing models skip without them
../scripts/fetch-models.sh       # Whisper + Silero, ~490 MB, once
cd .. && ./server/target/release/choufleur-replay make-fixture corpus/fixture-smoke
./server/target/release/choufleur-replay transcribe corpus/fixture-smoke -o out/segments.jsonl
./server/target/release/choufleur-replay track corpus/fixture-smoke --segments out/segments.jsonl -o out/trace.jsonl
./server/target/release/choufleur-replay eval corpus/fixture-smoke --trace out/trace.jsonl --segments out/segments.jsonl
```

Watch it follow a show, with the audio audible and the script on screen:

```bash
# the library, and a show server started from it — what the app runs
./server/target/release/choufleur-replay ui --port 8080

# a live run on its own: sound out of the default device, the page at localhost:8080
./server/target/release/choufleur-replay serve <manifest> --port 8080

# the same script with no audio, for preparing it and its cue lists
./server/target/release/choufleur-replay serve <manifest> --prep --port 8080
```

### The app

```bash
scripts/sidecar.sh                        # build the server the app carries
cd server/crates/choufleur-app && cargo tauri dev

# signed, and a DMG beside it
APPLE_SIGNING_IDENTITY='Developer ID Application: … (TEAMID)' scripts/release-app.sh
```

Add `APPLE_ID`, `APPLE_PASSWORD` (an app-specific one) and `APPLE_TEAM_ID` — in
`scripts/.env.release`, which is not committed — and it notarizes too. Without that a
second Mac refuses it; `release-app.sh` says so rather than leaving it to be discovered
on the door of a venue.

### Releasing

Tagging is what cuts a release. `.github/workflows/release.yml` builds on an Apple
Silicon runner, runs the tests, signs, notarizes both the app *and* the disk image, and
proves the result before publishing — a build that Gatekeeper would reject fails the job
instead of becoming a download. It leaves a **draft**, so nothing is public until you
say `gh release edit v0.1.0 --draft=false`.

```bash
# the version lives in two files and the workflow refuses a tag that disagrees
git tag -a v0.1.0 -m "…" && git push origin main --follow-tags
```

Six repository secrets, under Settings → Secrets and variables → Actions. The workflow
checks all six are present, and that the certificate matches the identity, before it
builds anything — the whole point being that the bundler would otherwise *warn* and
publish an unsigned DMG.

| Secret | What it is |
|---|---|
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: Name (TEAMID)` — exactly as `security find-identity -v -p codesigning` prints it |
| `APPLE_CERTIFICATE` | that certificate **and its private key**, as base64. In Keychain Access expand the triangle, select both rows, Export 2 items as `.p12`, then `base64 -i cert.p12 \| pbcopy`. Exporting the certificate alone signs nothing |
| `APPLE_CERTIFICATE_PASSWORD` | the password given to that `.p12` |
| `APPLE_ID` | the Apple account email |
| `APPLE_PASSWORD` | an **app-specific** password from appleid.apple.com → Sign-In and Security, of the form `abcd-efgh-ijkl-mnop`. Not the account password |
| `APPLE_TEAM_ID` | the ten characters in brackets in the identity |

**The microphone only works from the app.** A binary run from a terminal is given a
stream that delivers nothing at all rather than an error, and never appears in System
Settings to be allowed. `cargo tauri dev` is not proof either — under it, capture is
attributed to the terminal's own permission. Test in the bundle.

Models are fetched once per machine into `~/Choufleur/models`, from the app's first-run
panel or from a terminal:

```bash
./server/target/release/choufleur-replay models list     # what is here, and where to put it
./server/target/release/choufleur-replay models fetch    # ~490 MB, resumable, checksummed
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
│       ├── choufleur-asr      # resample, VAD, Whisper: buffers in, segments out
│       ├── choufleur-server   # the library on disk: shows, versions, safe writes
│       ├── choufleur-replay   # the binary: servers, audio, CLI, and the web client
│       └── choufleur-app      # the desktop shell — one window, and everything's lifetime
├── corpus/          # evaluation recordings — manifests in git, audio is not
├── research/        # Python sidecar for forced alignment; never in the show path
├── scripts/         # model fetching, and building the app
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
