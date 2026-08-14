# Choufleur — Development Plan

Version 1.0 — Draft — August 2026

Dual-licensed under MIT OR Apache-2.0.

Companion to the [PRD](choufleur-prd_1.md) and the [notation spec](choufleur-notation_1.md). The PRD says *what*; the notation spec says *exactly what*; this document says *in what order, and how we'll know it works*.

---

## How to Read This Plan

- **Milestones are scope-sized, not time-sized.** One developer, part-time, alongside professional theatre work. No calendar dates anywhere; a milestone is done when its "done when" is true.
- **Every phase boundary is a pause point.** Life and shows intervene; the plan is built so the project is in a useful, resumable state whenever a phase closes. Additional pause points are marked inside phases.
- **Phase 0 answers the existential question before anything app-shaped exists.** If ASR script-following doesn't work on real theatre audio, nothing else matters. Everything Phase 0 produces is permanent infrastructure — crates and an eval harness, not a throwaway prototype.
- **The replay harness is the regression backbone forever.** Any later change to matching, ASR config, or warning scheduling re-runs against the recorded corpus and must not regress the pinned baseline.

---

## Phase 0 — Tracking-Engine Risk Spike (offline, go/no-go)

Everything runs against files on disk. No live audio capture, no server, no client. Python is permitted as a labeling/exploration sidecar only (`research/`), never imported by Rust code.

### M0.1 — Eval corpus assembly

*Goal:* Turn the existing rehearsal recordings into a labeled, reproducible evaluation corpus. Without ground truth the spike can only be vibe-checked, not measured.

*Tasks:*
- Select 1–2 productions; export per-channel WAVs (and a mixed-down variant of the same material) for at least one full act
- **Check the script is the one that was performed.** A prep document ages: a show's
  text keeps moving after the premiere while the script usually does not. Measured
  on real touring material, drift between a premiere-dated script and a mid-tour
  recording was the largest single error source, and it is indistinguishable from
  engine failure unless it is looked for. `research/script_vs_audio.py` reports the
  ceiling — what fraction of script lines appear anywhere in the audio at all —
  and should be run before any tuning
- Prepare the matching script as plain structured text (character + line, per act/scene) — hand-rolled format is fine here; the real show file arrives in Phase 1
- Produce a **ground-truth timeline** (line → onset timestamp): forced alignment in the Python harness (e.g. WhisperX/aeneas), then hand-correct — correcting is far faster than labeling from scratch
- Define the corpus manifest: paths, channels, character→channel map, language tags, SHA-256 of each audio file. Audio lives on external storage; the manifest (with hashes) lives in git

*Done when:* One full act exists as `corpus/<show>/manifest.json` + WAVs + script + ground-truth timeline, and a second act (or second show) is at least audio-ready.

### M0.2 — Offline ASR pipeline

*Goal:* Rust reads WAVs, gates speech with VAD, and emits timestamped Whisper transcript segments per channel. This milestone eats the nastiest dependency risk (whisper-rs + ort + Metal all building and running together).

*Tasks:*
- Bootstrap the cargo workspace (`server/` with `crates/`)
- `choufleur-asr`: Silero VAD via `ort` (segment open/close policy, 1–5 s chunks); whisper-rs with Metal; forced language per segment; `initial_prompt` biasing hook fed with upcoming script text
- Hallucination filter v0: repetition loops, known filler outputs
- `choufleur-replay` CLI: streams a manifest's channels through VAD→ASR in simulated real time *and* batch mode, dumps segments as JSONL

*Done when:* `choufleur-replay transcribe corpus/<show>` produces per-channel timestamped segments for the full act, faster than real time on the dev machine with the `small` model.

### M0.3 — Matcher + tracker v0

*Goal:* The pure, I/O-free tracking engine: a probability over script position, updated per transcript segment. This is the crate the live server will later call unchanged.

*Tasks:*
- Normalization pipeline exactly per notation spec §3.2 (NFC, lowercase, punctuation strip, whitespace collapse) — shared later with re-anchoring
- Fuzzy line matcher (token-set ratio; per-language normalizer hooks as a trait, EN + FR implemented for real)
- Position tracker: forward-only constraint, confidence levels (word/line/cue-block/scene), confidence decay, skip tolerance, landmark weighting
- Per-channel hypothesis fusion (channels carry character identity from the manifest)

*Done when:* `choufleur-replay track` produces a position trace (timestamped line-id + confidence) for the full act from M0.2's segments, deterministic given the same inputs.

### M0.4 — Metrics, tuning sweep, and the gate

*Goal:* Score the tracker against ground truth, sweep the main knobs, make the go/no-go call.

*Tasks:*
- `choufleur-replay eval`: coverage, lag distribution, confident-wrong events, recovery time — one command, one report
- Sweep: model size (`small`/`medium`), biasing on/off, per-channel vs mixed feed, VAD thresholds
- Measure per-segment pipeline latency (VAD close → ASR → match) against the ≤1.5 s typical / ≤3 s worst-case budget
- Short findings note committed in-repo

*Done when:* The report is reproducible via one command on ≥2 distinct recordings, and the gate below has an answer.

### ★ GO/NO-GO GATE

On real recordings, full act, `small` model (`medium` acceptable for non-English), Apple Silicon:

| Criterion | Threshold |
|---|---|
| **Page** | Reported position within **±5 lines** of ground truth for **≥ 90 %** of speech-active time |
| Precision | Within **±1 line** for **≥ 60 %** of speech-active time |
| Lag | Median line-detection lag (audio onset → tracker update) **≤ 2.0 s**, p95 **≤ 4.0 s** |
| Honesty | **No confident-wrong event lasting longer than one segment** (tracker at word/line confidence while > ±5 lines off), and total confidently-wrong time **< 2 %** of speech-active time |
| Recovery | After any tracking loss, re-anchor within **≤ 30 s** of subsequent speech, or at the next landmark |
| Compute | Sustained faster-than-real-time with 3–4 **concurrent** active channels |

Three of these were revised after Phase 0 measured real material; the reasoning is
in the [Phase 0 notes](choufleur-phase0-notes.md) and matters more than the numbers.

**Page, not line.** The operator reads a page and needs the current line in the
middle third of it (PRD, *How accurate is accurate enough*). ±1 line is a
transcription-alignment criterion, not an operator one, and it is roughly five
times stricter than the job requires — a system that satisfied every real need
would have failed the old gate. ±1 is kept as a secondary *precision* figure
because cue timing does care about it, at a threshold that reflects what is
achievable rather than what would be nice.

**Honesty in duration, not in count.** A cut wider than skip tolerance cannot be
noticed until the next material is heard, so a brief stale window after a cut is
physics, not a defect. What must not happen is a *sustained* confident-wrong
stretch. Note that a position advancing at cue-block confidence is by definition
not confidently wrong, which is what makes steady coarse tracking safe.

**Concurrent, and it means it.** Ordinary dialogue takes turns, so a corpus of
scene work measures one active channel however many the manifest lists. The
criterion needs a genuinely overlapping fixture — `make-fixture --load-test <n>`
builds one.

**Pass** → Phase 1. **Marginal** (e.g. coverage 80–90 %) → one bounded iteration on biasing/landmarks/VAD, then re-gate. **Fail** → stop, or pivot the concept to manual-drive-first with ASR assist. Do not build the app on a broken engine.

*Pause point:* even stopping here leaves a working eval harness and the answer to the hard question.

---

## Phase 1 — Show Format and Prep Foundations

Pure data-layer work in `choufleur-show`; no audio hardware needed, so these milestones fit fragmented evenings and can interleave with Phase 2.

### M1.1 — Show file model

*Goal:* Serde-typed show file per notation spec §11, with round-trip fidelity.

*Tasks:* full schema types (acts/scenes/lines, layers, operators, orphans, cueTypes, calibration); line-ID generation (§3.1); unknown-field preservation; language inheritance resolution (§8.2) as a queryable API.

*Done when:* Golden show files — including the §11.1 example and one with unknown fields — round-trip byte-stably, and inheritance resolution passes unit tests.

### M1.2 — .docx import + inline shorthand parser

*Goal:* First import per §12: OOXML parse, tag parse/strip, ID assignment, import report.

*Tasks:* OOXML `word/document.xml` reader; §5 grammar parser (all four tag kinds, `{{`/`}}` escaping, standalone-paragraph anchoring, bidi isolation); character detection; import report struct.

*Done when:* A fixture `.docx` exercising every §5 rule imports to a golden expected show file, and malformed brace groups are left untouched and reported — never guessed.

### M1.3 — Four-pass re-anchoring, orphans, backups

*Goal:* The load-bearing algorithm of the format (§3.3–3.5): annotations survive rewrites, nothing is lost silently.

*Tasks:* Pass 1 exact; Pass 2 LCS order-alignment; Pass 3 windowed fuzzy (reusing `choufleur-core` normalization/similarity); Pass 4 orphan collection; timestamped backup with refuse-to-run-on-backup-failure; orphan report.

*Done when:* A golden suite covering each pass passes — including the §3.5 worked example verbatim, a duplicate-line case ("Yes." × 12), and a cut-line-with-cue orphan case.

*Pause point:* Choufleur is already a useful standalone script-annotation tool here.

### M1.4 — Replay harness speaks show files

*Goal:* Retire M0.1's ad-hoc script format; the tracker consumes real show files end-to-end.

*Tasks:* Convert corpus scripts to show files; wire landmarks, act/scene implicit anchors, language tags, and character→channel maps from the show file into `choufleur-core`/`choufleur-replay`; re-run the Phase 0 eval.

*Done when:* The eval report regenerates from show-file input and **matches or beats the Phase 0 numbers** — this becomes the pinned regression baseline.

---

## Phase 2 — Live Headless Server

### M2.1 — Live audio capture

*Goal:* cpal multi-channel capture feeding the *same* VAD→ASR→track path the replay CLI uses. This milestone de-risks the offline/live seam — the single biggest "works in the lab" trap.

*Tasks:* CoreAudio device enumeration/selection; channel mapping config (`venue.toml`, see Pre-Coding Decisions); ring buffers into `choufleur-asr`; per-channel signal metrics (RMS/peak/clip/silence); a loopback rig (hardware loop or BlackHole) playing corpus WAVs into live capture.

*Done when:* A corpus act played through the loopback produces a tracking trace matching the offline replay within a small tolerance — this *is* the virtual-soundcheck workflow, proven on the dev desk.

### M2.2 — Server skeleton + protocol

*Goal:* The axum binary: WebSocket fan-out, REST show load, typed protocol crate.

*Tasks:* `choufleur-protocol` — every message in the PRD's protocol table as serde types (typed events + parameters, no display prose, per the localization rule); `hello` with protocol version + full-state resync; `position_update` broadcast; REST endpoints for show load and device config; static-serving stub for the future client.

*Done when:* A scripted test client (or `websocat`) joins, receives full state, and streams live `position_update`s during a loopback replay.

### M2.3 — Warning scheduler + pace/ETA v0

*Goal:* Cue warnings fire at the right moments, latency-compensated. This is the product's actual job, put under regression control.

*Tasks:* Lead-list scheduling (standby/final/now per notation spec §4.2); measured-pipeline-latency subtraction with degraded-lead marking; naive per-act pace model + ETA; `cue_warning` broadcast.

*Done when:* An automated replay test asserts that every cue in the corpus act fired at the correct compensated offset.

### M2.4 — Run controls, holds, access control

*Goal:* The server behaves the way rehearsals do: nonlinearly.

*Tasks:* `position_jump`, `run_pause`/`run_resume`, position history; preshow/intermission holds auto-engaging at act ends, releasing on confident match or manual confirm; show-code join; view-only rejection of all steering messages; attribution on every control.

*Done when:* A scripted WS session exercises every client→server message in the PRD table and server state transitions match spec, including view-only rejections.

*Pause point:* the headless server is fully drivable by any WS test client.

### M2.5 — Journal, run log, crash recovery

*Goal:* Fail loudly, recover fast (PRD, *Failure Behavior*).

*Tasks:* Append-only fsync'd position journal; JSONL run log (flight recorder, one file per run); relaunch → restore journaled position → hold pending confident match or manual confirm; client resync on reconnect.

*Done when:* `kill -9` mid-loopback-replay, relaunch, and the server restores position in hold state and resumes on the next confident match; the run log replays the whole session coherently.

*Pause point:* end of Phase 2 is a complete, trustworthy headless engine.

---

## Phase 3 — Client MVP (Flutter web, `remote/`)

Client development runs against `choufleur-replay serve-trace` — a fake-server mode streaming a recorded WS session — so no milestone here needs live audio.

### M3.1 — Scaffold, join flow, localization from day one

*Tasks:* WS client + reconnect + typed message decode (hand-mirroring `choufleur-protocol`); show-code entry, remembered per device; operator/view-only roles; cue-type filter picker; ARB scaffolding with EN + FR from the first screen — retrofitting l10n is misery, so it starts structural.

*Done when:* Two browsers join a live server (one operator, one view-only), both render live position as text, UI switchable EN↔FR per device.

### M3.2 — Script display, live scroll, footer

*Tasks:* Virtualized script rendering from the show file; auto-scroll locked to position with confidence styling; persistent footer (current cue / next cue / ETA); notes side panel (wide) and tap bubbles (narrow).

*Done when:* A full loopback act plays and the script follows live, footer live, on tablet and laptop form factors.

### M3.3 — Warnings UX

*Tasks:* Peripheral red-frame standby/final/now states; per-operator filter + personal categories (colors, grouping); tap-to-acknowledge (`warning_ack`/`ack_state`); browsing mode with warnings still firing and one-tap return to live.

*Done when:* During replay, a filtered operator receives exactly their cues' warning stages, can acknowledge, and can browse ahead without losing a warning.

*Pause point:* genuinely usable at a real rehearsal from here (read-only trust level).

### M3.4 — Run controls, manual drive, help flow

*Tasks:* Quick-jump picker (page / act–scene / cue); pause/resume UI; manual-drive claim/scroll/release with everyone-sees-it banner attribution; help-request flow (first responder gets drive instantly, no dialog); long-press point correction.

*Done when:* Two-device test: server drops confidence (simulated) → help request broadcast → device A claims and drives → device B follows with banner → shadow re-lock proposes release.

### M3.5 — Failure UX

*Tasks:* Stale banner with age ("last position 40 s ago", position greyed); resync on reconnect with missed warnings shown as missed; screen wake lock, with a loud join-time failure if the platform denies it.

*Done when:* Pulling the server mid-replay produces the stale banner within seconds; restart resyncs cleanly; wake lock verified **on the actual tablets that will be used**.

*Pause point:* end of Phase 3 = field-trial-ready MVP.

---

## Phase 4 — Prep Workflow, Warning Families, Field Hardening

### M4.1 — Prep pages

*Tasks:* Browser-served prep: .docx upload + import report display; re-import with orphan-resolution UI (reattach / confirm-delete, honoring the §3.4 consent rules); cue/landmark/lead editing; character↔channel assignment; audio device + level check page.

*Done when:* A show goes from `.docx` to show-ready entirely in the browser, and a rewritten `.docx` re-imports with orphans resolved through the UI, backup verified on disk.

### M4.2 — Divergence + input-health warning families

Deliberately *after* the client exists: these detectors need eyes-on-screen tuning, and the run log makes each night's misfires diagnosable.

*Tasks:* Sliding-window confidence-trend detectors (`off_script`, `paraphrase_drift`, `skipped_material`, `improvisation`); channel health escalation (`silent`/`clipped`/`garbled`, reusing the hallucination-filter signal); routing (Family A → all, Family B → sound + SM); client rendering; thresholds surfaced in prep.

*Done when:* Replay-based tests trigger each of the seven warnings deterministically from doctored corpus runs (muted channel, lines cut from audio, injected noise), and each clears correctly.

### M4.3 — Notes in anger + operator fragment export

*Tasks:* Double-tap `note_add` in show mode; post-show note review with run-log context; operator fragment export (`choufleur-operator`); fragment merge **via CLI only** (UI deferred).

*Done when:* A note taken during a replay run appears in post-show review with its run context; an exported fragment merges into a second copy of the show file by CLI with the §11.2 conflict rules.

### M4.4 — Field trials + packaging

*Tasks:* Single-binary packaging with embedded client assets + model-fetch tooling; venue virtual soundcheck through the actual Dante chain; **≥ 2 shadow tech rehearsals** (Choufleur runs, nobody depends on it); run-log review after each; threshold/model tuning loop.

*Done when:* One full rehearsal tracked live in a venue **meeting the Phase 0 gate numbers in the field**, with the run log as evidence, and at least one operator other than the author having used the client.

*Pause point:* this is effectively v0.1.

---

## Phase 5 — Release

### M5.1 — Open-source hygiene

*Tasks:* README quickstart; CI (fmt/clippy/tests + a small checked-in synthetic replay smoke test — the full corpus regression stays a local pre-release ritual); contribution note; issue templates.

*Done when:* A stranger with an M-series Mac and a mixed-feed interface can go from clone to tracked script using only the README.

### M5.2 — v0.1 tag

*Done when:* Tagged release with macOS binary, sample show file, and this devplan updated with actual-vs-planned notes.

---

## Dependency Spine

```
M0.1 → M0.2 → M0.3 → M0.4 ═ GATE
                              ├── M1.1 → M1.2 → M1.3 → M1.4 (re-baselines eval)
                              │                          ↓
                              └────────────→ M2.1 → M2.2 → M2.3 → M2.4 → M2.5
                                                     ↓
                                            M3.1 → M3.2 → M3.3 → M3.4 → M3.5
                                                                          ↓
                                            M4.1 → M4.2 → M4.3 → M4.4 → Phase 5
```

M1.1–M1.3 need no audio hardware and interleave with Phase 2 whenever the desk isn't available. M2.1 depends only on Phase 0 crates; M2.3 onward wants M1.4's show-file cues.

---

## Rust Workspace Decomposition

```
server/                          # cargo workspace
├── crates/
│   ├── choufleur-core           # SPIKE, permanent. Pure logic, zero I/O, zero async:
│   │                            #   normalization, fuzzy matching, position tracker,
│   │                            #   confidence model, landmarks, hold state,
│   │                            #   warning-schedule computation (pure fn of position+cues)
│   ├── choufleur-asr            # SPIKE, permanent. Silero VAD (ort) + whisper-rs,
│   │                            #   chunking policy, initial_prompt biasing,
│   │                            #   hallucination filter. Buffers in, segments out.
│   │                            #   Knows nothing of cpal or files.
│   ├── choufleur-show           # Phase 1. Show file serde model, line IDs,
│   │                            #   .docx import, shorthand parser, 4-pass re-anchoring,
│   │                            #   backups, orphans, import reports.
│   ├── choufleur-protocol       # Phase 2. Typed WS/REST message enums (serde).
│   │                            #   Single source of truth; Dart mirrors by hand.
│   ├── choufleur-server         # Phase 2. The binary: cpal capture, tokio+axum,
│   │                            #   journal, run log, static client serving.
│   └── choufleur-replay         # SPIKE, permanent. CLI: transcribe / track / eval /
│                                #   bench / serve-trace over the corpus. The regression
│                                #   and tuning harness forever.
remote/                          # Flutter — web-first client (Phase 3)
research/                        # Python: forced alignment, notebooks.
                                 #   Never imported by anything in server/.
```

The discipline that makes the spike non-throwaway: `choufleur-core` accepts transcript segments and emits position updates through plain function calls — the replay CLI and the live server are just two drivers of the same engine. Dependency rules: `core` depends on nothing internal; `asr` and `show` depend on `core` (shared normalization); `server` depends on all; `replay` depends on all but `server`.

---

## Test and Validation Strategy per Phase

| Phase | Strategy |
|---|---|
| 0 | The eval harness *is* the test. Corpus manifest + SHA-256 hashes in git, audio external. Pinned metrics report committed as the baseline artifact. Criterion micro-benches on the match hot path. |
| 1 | Golden-file tests: fixture `.docx` → expected show JSON; old show + new `.docx` → expected re-anchored show (one golden per pass, incl. §3.5 verbatim and duplicate-line LCS cases). Property tests on normalization (idempotence, NFC stability). Round-trip byte-stability with unknown fields. M1.4 re-runs the Phase 0 eval — **regression gate: no metric regresses**. |
| 2 | Loopback-vs-offline trace equivalence (M2.1). Scripted WS integration tests covering every protocol message incl. view-only rejections. Warning-timing assertions against the run log. Kill-recovery test. End-to-end latency measured in-band (capture timestamp → broadcast timestamp) and asserted against the ≤1.5 s / 3 s budget. |
| 3 | Thin widget tests only where cheap; the real harness is `choufleur-replay serve-trace` streaming recorded sessions, so client work never needs live audio. Manual device matrix (the actual show tablets) for wake lock and stale-banner behavior. |
| 4 | Doctored-corpus tests for all seven warning types (deterministic triggers). Field-trial protocol: venue virtual soundcheck → shadow tech rehearsals → run-log review checklist after every run ("where did it lose, why, which knob"). The run log is the field-test oracle. |
| 5 (CI) | fmt/clippy/unit/golden on every push; a seconds-long checked-in synthetic replay as smoke test; the full corpus regression stays a local pre-release ritual. |

---

## Deliberate Deferrals

| Deferred | Re-enters | Why |
|---|---|---|
| Windows / ASIO / DVS-WDM handling | After v0.1 is field-proven on macOS | Steinberg SDK build friction; no validation hardware in the loop; PRD already calls it best-effort |
| Flutter native builds (iPad/Android) | After the web client survives field trials | Same codebase, packaging cost only. **Contingency:** if iOS Safari wake lock proves unreliable at M3.5, the iPad build jumps the queue |
| Haptic modality | Post-v1 (PRD roadmap) | Needs native builds anyway |
| Operator fragment merge **UI** | When a second real operator preps remotely | Format (M1.1) + CLI merge (M4.3) land early so no data is ever strandable; UI is polish |
| SM ack-state matrix view | After basic acknowledgment ships (M3.3) | Optional per PRD |
| ETA sophistication (pauses, musical numbers) | Tuned from field run logs, post-M4.4 | Naive per-act pace suffices to validate the footer; PRD marks it an open question |
| CJK/Thai n-gram matching, RTL display validation | First real show needing them | Per-language normalizer trait exists from M0.3; only EN/FR (+ corpus languages) *validated* early |
| Protocol codegen (Rust→Dart) | When protocol churn hurts | Hand-mirrored types + a JSON-fixture cross-check test is cheaper for one developer |
| Menu-bar/tray shell, OSC output, `.chou` container, mixed-feed diarization, audio nudge | Post-MVP per PRD | No plan impact |

---

## Pre-Coding Decisions

Five things the design docs left open, decided here so no milestone stalls on them:

1. **Position wire format.** `position_update` carries `{ seq, lineId, lineIndex, withinLineFraction?, confidence, actId, sceneId }` — `seq` monotonic per run, `lineIndex` for cheap scrolling, `lineId` as the durable reference. `hello` carries a protocol version. (Reflected in the PRD protocol section.)
2. **Ground truth + corpus storage.** Forced-align-then-hand-correct labeling; multi-GB audio on external storage; manifest with SHA-256 hashes in git; labeling conventions written once in `corpus/README`.
3. **Model storage/distribution.** Fetch-on-first-run script with a documented manual-download fallback; the set of supported model sizes/quantizations listed in the README per release.
4. **Venue vs show config split.** The show file keeps character ↔ *logical* channel; a `venue.toml` beside the binary maps logical channels to physical devices/inputs and defines zone channels. Shows travel; venues don't.
5. **Operator identity.** Client-generated persistent device ID + self-chosen display name, claimable against an existing `operators.<opId>` subtree in the show file. The show code grants the operator *role*; the opId is *who*.
