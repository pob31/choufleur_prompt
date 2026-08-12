# Choufleur — Product Requirements Document

*A script GPS for theatre technicians*

Version 0.2 — Draft — August 2026

Dual-licensed under MIT OR Apache-2.0 — see the License section.

---

## Problem Statement

Theatre technicians running a show — sound, lighting, video, flys, stage management — frequently need to focus deeply on their console while simultaneously tracking dramatic position and upcoming cues. Show callers are not always present. Missing a cue due to divided attention is a real operational risk.

Existing show control tools (QLab, EOS, etc.) manage cue execution but provide no ambient awareness of script position. Choufleur is not a show control replacement — it is an attention management tool. A tap on the shoulder, not a robot operator.

---

## Core Concept

A networked, ASR-assisted script tracking system that follows a live performance in real time, displays current script position on operator devices, and delivers configurable pre-cue warnings as peripheral nudges.

Choufleur supports multilingual productions natively, including plays that mix several languages — down to individual bilingual lines (see Multi-Language Support).

The system never triggers cues. It warns. The technician always makes the call.

---

## Architecture

### Server (macOS primary, Windows secondary)

A headless, compiled **Rust** service. There is no native server GUI: prep and configuration are browser pages served by the server itself, consistent with the web-first client. A thin native shell (menu-bar/tray app for launch, device pickup, and status) is a post-MVP nicety, never a dependency.

- Multi-channel audio input via Dante or direct console feeds (up to 16 channels)
- Per-channel active speaker detection — ASR only runs on open/speaking channels
- ASR engine: Whisper family; model size selected per show and per language (see ASR Engine and Latency Budget)
- Per-channel input-health monitoring — signal metrics plus ASR-quality signals (see Warning Families)
- Script position tracking with confidence levels
- Landmark-based re-anchoring when tracking drifts
- Cue warning logic with configurable lead times per cue
- Position broadcast over local network via WebSocket
- No internet dependency — fully local operation
- Graceful degradation to single mixed channel for simpler setups

### Client (web-first, any device)

The client is a web application: any device on the venue network — tablet, laptop, phone — opens a page served by the server, joins the show, and picks the role / cue list that concerns it. Dedicated iPad and Android apps are installable variants of the same Flutter client, not a separate product.

- Receives position updates from server
- Displays current script page — always showing where the show is
- Role picker on join — each operator selects which cue types they follow; operator access gated by a short show code, with a view-only role for guests (see Access control)
- Per-technician cue filter — each operator sees only their relevant cues
- Personal cue categories — each operator organizes their own cues into freely named categories (e.g. QLab, Ableton Live, console, spatial, music) for grouping and color accents
- Notes displayed alongside the script — side by side on wide screens, or as tappable help bubbles anchored to their line on narrow screens
- Red frame peripheral warning on pre-cue alert
- Independent browsing mode without disrupting other clients (see Multi-Operator Protocol)
- Persistent footer — current cue, next cue, estimated time to next cue
- One-tap return to live position from browsing mode

---

## ASR Engine and Latency Budget

### Whisper is chunk-based, not streaming

Whisper processes windows of audio (natively 30 s of log-mel context); it does not emit incremental hypotheses. Real-time tracking therefore requires an explicit chunking strategy: voice-activity detection (e.g. Silero VAD) gates each channel and feeds short speech segments (1–5 s) to the model, with a sliding-window / local-agreement policy for stable partial results. Candidate implementations: whisper.cpp's streaming mode, faster-whisper with short-segment inference, LocalAgreement-style streaming wrappers. The choice is an implementation detail; the requirement is bounded, measured latency.

### Latency budget

Cue lead times go down to 5 seconds, so end-to-end latency (audio → VAD close → ASR → match → broadcast) must be bounded and known.

- Target: **≤ 1.5 s typical, ≤ 3 s worst case**
- The warning scheduler subtracts measured pipeline latency from cue lead times — a 5 s lead with a 3 s pipeline is effectively a 2 s warning. This floor is acknowledged, surfaced in the UI, and validated during tech rehearsals.

### Multilingual model sizing

The multilingual accuracy of `base` is poor for most non-English languages; `small` is the floor and `medium` (or distilled large variants) is recommended for non-English shows where compute allows. Model size is a **per-show, per-language configuration**, not a platform constant.

Mitigating factor, stated honestly: Choufleur performs *alignment against a known script*, not open transcription. Fuzzy matching plus decode biasing (seeding Whisper's `initial_prompt` with upcoming script text) tolerates a much higher word error rate than dictation would — `small` may prove sufficient where raw WER numbers look alarming. Tech rehearsal is the validation gate.

### Hallucination on silence and noise

Whisper hallucinates fluent text on non-speech input. VAD gating is mandatory, and hallucination patterns (repetition loops, known filler outputs) are filtered from the match pipeline. The same filter doubles as a detector for the `channel_garbled` input-health warning.

### Compute budget

Hardware baseline: **Apple Silicon (M1 or newer)**, using Metal/CoreML acceleration. Windows x86 is supported best-effort with reduced concurrent channel counts.

Load-management strategies, in order of importance:

1. **Active-speaker gating** — rarely more than 3–4 channels speak simultaneously; idle channels cost nothing
2. **Shared model instance** — one loaded model, batched or round-robin inference across active channels (e.g. faster-whisper/CTranslate2 batching), rather than one model instance per channel
3. **Documented degrade path** — fewer tracked channels → single mixed feed; tracking quality degrades gracefully, the tool stays useful

---

## Operating Modes

### Rehearsal / Prep Mode

A browser page served by the server — usable from the server machine or any client device.

- Import Word script (.docx) — inline cue shorthand parsed and stripped on import (see Script Format and Prep Workflow)
- Assign lines to characters
- Tag cues with type (LX, sound, flys, etc.) and configurable lead time
- Mark landmark lines for position re-anchoring
- Add personal notes per line or cue
- Test audio input and calibrate levels
- Run tracking tests during tech rehearsals to tune sensitivity
- Export/import show file for transfer and backup

### Show Mode

- UI simplifies to script display and warnings — minimal interaction surface
- Editing locked out — deliberate gesture required to switch modes (long press or confirmation dialog)
- Script scrolls automatically with show position
- Quick note attachment via double-tap on a line — reviewed post-show
- Manual position nudge available to all operators
- Position takeover for recovery — see Manual Drive under Multi-Operator Protocol
- Browsing ahead does not affect other devices

### Show structure and holds

The script is structured as acts containing scenes (notation spec §11), and the tracker has an explicit notion of "the show is not currently running":

- **Preshow hold** — armed at the top of act 1; the tracker ignores walk-in music and audience noise instead of hunting for matches in it (exactly the conditions where ASR hallucinates)
- **Intermission hold** — entered at an act boundary; re-arms at the top of the next act
- Holds engage automatically at act ends and release on first confident match of the next act's opening lines, with a manual release always available
- Act boundaries are implicit weight-3 landmarks with hold semantics; act tops are natural quick-jump targets
- The pace/ETA model is calibrated **per act**

### Run Controls (Rehearsal)

Rehearsals do not run linearly. These controls are available to all operators and every use is attributed on all clients:

- **Quick jump** — jump to a page, act/scene, or cue via a picker; the jump resets the system position for everyone. This is the standard way to start work from an arbitrary point.
- **Pause** — one tap suspends tracking and cue warnings while the director gives notes; the paused state is clearly shown on every client; resume re-arms tracking at the held position.
- **Resume from previous position** — the server keeps a short history of recent positions and jumps; one tap returns to the start of the section being worked, for actors looping the same passage over and over.

---

## Script Position Tracking

The system maintains a probability distribution over current script position and updates it continuously with each recognised audio fragment.

### Tracking levels (high to low confidence)

| Level | Description | Behaviour when lost |
|-------|-------------|---------------------|
| Word | Exact transcript match | Falls back to line level |
| Line | Rough semantic match, fuzzy | Falls back to cue block level |
| Cue block | "Somewhere in this exchange before cue N" | Holds, waits for landmark |
| Scene | Absolute anchor, very reliable | Re-anchors at scene change |

### Key principles

- **Forward-only constraint** — position never moves backward automatically
- **Fuzzy matching** — handles paraphrasing, contractions, word substitutions; language-aware normalization per line (see Multi-Language Support)
- **Confidence decay** — after a configurable timeout without a match, the system flags uncertainty rather than guessing, and escalates to a help request (see Multi-Operator Protocol)
- **Landmark weighting** — distinctive, unique phrases tagged during prep are weighted heavily for re-anchoring
- **Skip tolerance** — if an expected line does not appear within a time window, position advances anyway
- **Uncertain is better than wrong** — a false confident position is more dangerous than an honest "I've lost track"

Known compound failure mode, stated explicitly: skip tolerance can advance past material that then arrives late (inverted lines), and the forward-only constraint prevents automatic recovery until the next landmark. This situation is not silently absorbed — it feeds the divergence warning system and, if unresolved, the help request flow.

### Handling difficult situations

| Situation | System behaviour |
|-----------|-----------------|
| Skipped line | Skip tolerance advances position after timeout |
| Inverted lines | Forward-only constraint holds; re-anchors on next landmark; divergence warning if prolonged |
| Paraphrased line | Fuzzy matching scores partial match, position advances |
| Improvisation | Confidence decay flags uncertainty; divergence warning; operator nudges manually |
| Scene change | Hard re-anchor; scene boundaries are reliable landmarks |
| Simultaneous speakers | Mitigated — not solved — by per-channel identity: the tracker fuses per-channel hypotheses; mic bleed is handled by level-thresholded attribution before speech is credited to a channel's character; mixed-feed mode gets no per-channel help and degrades accordingly |

---

## Audio Input

### Primary — discrete per-actor channels

- Up to 16 channels from console via Dante, direct outs, or aux sends
- Per-channel speaker identity cross-referenced with character assignments in script
- Active speaker detection gates ASR processing — rarely more than 3–4 channels simultaneously active
- Per-channel signal metrics — RMS/peak levels, clipping detection, silence detection — feeding the input-health warnings
- Dramatically reduces compute load and improves tracking accuracy

### Secondary — single mixed feed

- Single stereo or mono mix send via USB audio interface
- Practical for simpler setups or tablet-only deployments
- Loses per-actor channel advantage but sufficient for page tracking and cue warnings

### Ambient / area microphones — for unmic'ed actors

Not every production mics its actors. Ambient stage mics (boundary/PZM at the stage edge, hanging mics, shotguns) are a supported input class, with honest expectations:

- An ambient channel is configured as a **zone channel** — it carries no character identity; the tracker matches its transcript against *any* expected speaker at the current position
- Far-field audio degrades ASR: distance, reverb, and spill cost accuracy. The mitigating factor is the same as everywhere in Choufleur — alignment against a known script tolerates far higher word error rates than open transcription. Expect **line/cue-block-level confidence rather than word-level**; landmarks and scene anchors do more of the work
- Multiple zone channels (downstage left/centre/right) each remain separate inputs — no identity, but coarse stage-position context and better local SNR than one distant mic
- A high-pass filter and gentle compression on the feed before Choufleur helps; calibrate during tech rehearsal like any other channel
- **Hybrid casts are the expected case**: mic'ed principals on per-actor channels, ambient zones covering everyone else — the tracker fuses both

### Virtual soundcheck — tuning without actors

Tracking thresholds, model sizes, and latency are tuned by **virtual soundcheck**, the standard Dante workflow: the console replays a multitrack recording of a rehearsal through the same feeds Choufleur normally receives. Choufleur cannot tell replay from live and needs no recording feature of its own; recording and its consent implications stay where they already live — in the venue's existing console workflow.

### Network audio

- Dante ubiquitous in theatre — joins existing venue network infrastructure
- Dante Virtual Soundcard is a paid per-machine Audinate license; hardware alternatives (Dante AVIO adapters, PCIe cards) exist where licensing is a concern
- No special hardware beyond what the venue already has

---

## Multi-Operator Protocol

All clients share the server's authoritative position by default.

### Access control

The client is a web page on the venue network, and venue WiFi is rarely locked down. Joining is therefore gated, lightly:

- **Show code** — a short code (e.g. 4 digits, shown on the server screen) is required to join as an **operator**; entered once per device, remembered for the run of the show
- **View-only role** — joins without the code (or with a separate view code): sees script position, cues, and warnings, but the server rejects `position_correction`, `manual_drive`, `position_jump`, and run-control messages from view-only clients. For assistant directors, guests, anyone who should watch but never steer
- No accounts, no passwords — the goal is keeping a random phone in the audience from claiming manual drive, not enterprise auth

### Viewing states per client

| State | Description |
|-------|-------------|
| **Locked** | Follows server position automatically |
| **Browsing** | Temporary non-tracking mode: the user scrolls ahead or back to check, edit, or add notes without affecting anyone else. The live position indicator stays visible, warnings still fire, and one tap returns to live. |
| **Manual drive** | This client is the position source for everyone (see below) |

### Manual drive — position takeover

When tracking cannot recover on its own, one operator takes over and scrolls position for everyone.

- Entered via a **deliberate override command** — never by accidental scrolling
- While active, **every client shows a prominent banner**: who is driving, "position manually overridden"
- The driver scrolls; all locked clients follow
- ASR keeps running in shadow mode; when it re-locks with confidence it proposes a return to automatic tracking, which the driver confirms to release
- Last write wins if drive changes hands; every handover is attributed on screen

### Help request flow

When the server loses tracking (confidence decay timeout), it does not guess — it asks:

- A help request is broadcast to all clients
- The **first operator to respond is granted manual drive immediately** — no confirmation dialog on the responder's side; seconds matter
- All other clients see the claim attributed: "Pierre is re-locating position"
- A simple point-correction (long press on the correct line) is also always available for small nudges, with confirmation and on-screen attribution

### Per-technician cue filtering

Each operator configures which cue types they see:
- Sound engineer — sound cues only
- LX operator — lighting cues only
- Flys — fly cues only
- Stage manager — all cues

### Personal cue categories

Within their filtered cues, each operator can define their own second-level **categories** — for a sound engineer, perhaps *QLab*, *Ableton Live*, *Console*, *Spatial*, *Music*. Categories are freely named, carry a color, and are used for grouping and secondary filtering in the client.

Categories are strictly personal: they live in the operator's own subtree of the show file and reference shared cues by id, so identically named categories belonging to different operators are independent objects that can never collide or leak between displays (notation spec §9.2). Cue *types* (LX, SND, …) remain the shared production vocabulary; categories are how each operator organizes their own corner of it.

Warnings are personal. Script position is shared.

---

## Warning Design and Warning Families

The goal is peripheral awareness, not alarm. A tap on the shoulder.

- **Red frame** on script display — catches peripheral vision without demanding attention
- **Configurable lead time per cue** — some cues need 30 seconds warning, some need 5; multi-stage standby/final warnings supported (see the notation spec, §4)
- **Warning fires even while browsing** — operator sees alert even when looking ahead in the script
- **Footer always visible** — current cue, next cue, estimated time remaining

### Family A — Divergence warnings

"What we hear no longer matches the script." Detected from match-confidence trends over a sliding window, per channel. Routed to **all operators** — position trust is shared.

| Warning | Trigger |
|---------|---------|
| `off_script` | Sustained low match confidence while audio is healthy |
| `paraphrase_drift` | Partial matches trending downward |
| `skipped_material` | Skip tolerance fired across multiple lines or a tagged cue block |
| `improvisation` | Confident speech recognized, no script match |

Thresholds and window lengths are configurable and tuned during tech rehearsal.

### Family B — Input-health warnings

"The pipe itself is broken." Detected from per-channel signal metrics plus ASR-quality signals. Routed primarily to the **sound operator and stage manager**.

| Warning | Trigger |
|---------|---------|
| `channel_silent` | Expected speaker's channel is dead |
| `channel_clipped` | Sustained clipping on an active channel |
| `channel_garbled` | Levels look fine but ASR confidence collapses / gibberish rate spikes (hallucination-filter signal) |

The two families are deliberately distinct because the operator response differs: divergence means *trust the actors, distrust the position*; input health means *fix the pipe — position may still be fine via other channels*.

### Warning acknowledgment

A standby warning can be **tapped to acknowledge** — "I'm on it" — which dims it locally so an already-alert operator isn't nagged. Optionally, the stage manager's client shows the acknowledgment state of upcoming cues across operators: the digital equivalent of hearing "standing by" on comms. Acknowledgment is never required — an unacknowledged warning simply keeps warning — and final warnings and the "now" flash always fire regardless.

### Warning modalities roadmap

- **v1: visual only** — red frame, banners, footer
- **Later addition: haptic** — device vibration, possibly wearables; modality choice per operator
- **Unlikely: audio** — operators' ears are working ears; an audio nudge into the comms feed remains a post-MVP maybe, not a commitment

---

## Script Format and Prep Workflow

The script text is pristine; everything else is a layer. This model, the line-ID scheme, the shorthand grammar, and the show file format are normatively specified in [choufleur-notation_1.md](choufleur-notation_1.md).

- Import from .docx — the server parses the OOXML (`word/document.xml`) directly
- Every line receives a **stable line ID** on first import; cues, landmarks, language tags, and notes are stored in separate layers **anchored to line IDs** — never to page or line numbers
- Inline shorthand may be typed in the .docx during prep — `{LX:12 -30s House preset warm}` — parsed into layers and stripped on import (notation spec §5)
- Character/line assignment in prep mode — who says what
- Personal notes layer per operator — private by default, shareable per note
- Show file export/import for transfer between devices and backup
- Prep happens in advance; show file loaded at the venue

### Script amendments

Directors cut and rewrite. Annotations must survive.

- Re-importing an edited .docx re-anchors all layers to the new text via the four-pass algorithm in the notation spec (§3): exact match, order alignment, fuzzy match, orphan collection
- The show file is **automatically backed up (timestamped) before every re-import**; re-import refuses to run if the backup cannot be written
- Annotations that cannot be re-anchored become **orphans** — preserved in the show file with their context and surfaced for manual reattachment. **Nothing is ever deleted without explicit user confirmation.**

---

## Estimated Time to Next Cue

- System calibrates expected speaking pace per act during rehearsals
- ETA displayed in persistent footer on all clients
- Should account for pauses, scene changes, musical numbers — approach to be validated during tech-rehearsal calibration (open question)
- Treated as an estimate — not a countdown clock

---

## Multi-Language Support

Choufleur is multilingual by design, including several languages within one play.

### Language comes from the script, not detection

The expected language at any moment is read from the script's language tags — never auto-detected from audio. Whisper is forced to the tagged language per segment, which is materially more reliable than letting it guess.

### Tagging model — line-level with inheritance

Defined normatively in the notation spec (§8): a show default, overridable per act, per scene, per character, and per line; most specific wins. A bilingual line is tagged with both languages (`["sv", "en"]`) and matched against both, keeping the better score. Mid-line, per-word code-switching is **out of scope for v1**.

### Language-aware matching

The fuzzy-matching normalization pipeline adapts per language:

- Unicode NFC normalization everywhere; UTF-8 throughout
- Diacritic folding per language policy
- French elisions and contractions (*j'suis*, *t'as*)
- German compound tolerance
- Unsegmented scripts (CJK, Thai) matched at character/n-gram level instead of word tokens

### Display

- RTL scripts (Arabic, Hebrew) render via Flutter's bidi support — a client concern; matching is script-agnostic
- Cue shorthand tokens are Latin-script with explicit delimiters, isolated from bidi runs, so notation parses identically inside RTL text

---

## Platform

| Component | Target |
|-----------|--------|
| Server | Rust, headless; macOS primary (Apple Silicon M1+ baseline), Windows secondary (best-effort, reduced channels) |
| Audio capture | cpal — CoreAudio (macOS), WASAPI/ASIO (Windows) |
| ASR engine | whisper-rs (whisper.cpp bindings, Metal/CoreML); Silero VAD via ONNX Runtime; model size per show/language (see ASR section) |
| Network stack | tokio + axum — WebSocket, REST, and the served web client |
| Client | Web-first (any browser on the venue network); iPad/Android as installable Flutter variants |
| Script import | .docx (OOXML) parsed server-side |
| Show file | Open versioned JSON (serde-typed) per [choufleur-notation_1.md](choufleur-notation_1.md) |

Implementation notes:

- The heavy ASR work is native C/C++ (whisper.cpp, ONNX Runtime) regardless of host language; Rust orchestrates it and owns the show-critical process — memory safety matters most in hour three of a performance
- Windows caveat: Dante Virtual Soundcard's WDM mode presents as separate stereo pairs; its ASIO mode exposes one multi-channel device, and cpal's ASIO backend requires the Steinberg SDK at build time
- Python is welcome as an **offline research harness** — replaying virtual-soundcheck recordings to experiment with matching algorithms — but never in the show path
- The browser client implies WebSocket as the transport — browsers speak neither raw OSC nor UDP, which reinforces the protocol choice below

---

## Network Protocol

### Show mode — WebSocket

WebSocket handles all live show communication between server and clients.

- Bidirectional — clients send position corrections back to server, not just receive
- Browser-native — the web client requires no extra library
- Event-driven message model maps naturally to script position updates, cue warnings, operator state broadcasts
- JSON payloads for all messages

**Message types**

| Direction | Message | Payload |
|-----------|---------|---------|
| Server → clients | `position_update` | Script position, confidence level, current cue |
| Server → clients | `cue_warning` | Cue id, type, warning stage (standby/final), lead time remaining |
| Server → clients | `position_attributed` | Corrected position, operator device name |
| Server → clients | `tracking_lost` | Last known position, confidence decay state |
| Server → clients | `help_request` | Last known position — first operator to claim gets manual drive |
| Server → clients | `divergence_warning` | Kind (`off_script` \| `paraphrase_drift` \| `skipped_material` \| `improvisation`), position, confidence trend, affected channel/character |
| Server → clients | `divergence_cleared` | Kind, position |
| Server → clients | `input_health` | Channel id, state (`ok` \| `silent` \| `clipped` \| `garbled`), metrics snapshot |
| Server → clients | `run_state` | Paused/resumed/jumped/manual-drive state, with operator attribution |
| Client → server | `position_correction` | Corrected script position |
| Client → server | `browse_mode` | Operator entering/leaving browse state |
| Client → server | `manual_drive` | Claim / scroll / release — a claim is also the answer to a `help_request`; first claim wins |
| Client → server | `position_jump` | Target page, act/scene, or cue (run control) |
| Client → server | `run_pause` / `run_resume` | Pause tracking for director notes; resume, optionally from a previous position |
| Client → server | `note_add` | Line id, text, operator id (show-mode double-tap notes) |
| Client → server | `warning_ack` | Cue id, warning stage — acknowledge a standby ("I'm on it") |
| Server → clients | `ack_state` | Acknowledgment states of upcoming cues (stage manager view) |

### Prep / rehearsal mode — REST

REST handles all request-response interactions during prep. Cleaner than fitting configuration through a WebSocket.

- Load and save show files
- Script import and character assignment
- Cue tagging and landmark configuration
- Audio device configuration
- Per-operator cue filter preferences and personal cue categories

### Why not OSC

OSC excels at continuous parameter streams — fader values, spatial coordinates. Choufleur's messages are event-driven and structured. Forcing cue warnings and position corrections into OSC address patterns adds friction without benefit.

### Why not MQTT

Elegant for multi-client broadcast via topic subscriptions, but requires a broker process running alongside the server. WebSocket fan-out to connected clients is sufficient for Choufleur's scale.

---

## Failure Behavior and Degraded Modes

A tool whose job is protecting attention must fail loudly and recover fast. Mid-show failure behavior is specified, not improvised:

### Server crash

- Current position, hold state, and run-control state are **journaled to disk continuously** (append-only, fsync'd at position changes)
- On relaunch the server reloads the show file, restores the journaled position, and resumes in a **hold** state pending one confident match or a manual confirmation — it never resumes guessing
- Clients auto-reconnect and resync full state on `hello`

### Network drop

- A client that loses its WebSocket shows an unmistakable **stale banner**: "connection lost — last position 40 s ago", with the stale position greyed, never displayed as live
- Reconnection resyncs state fully; warnings missed while disconnected are shown as missed, not replayed as if current

### Device sleep

- The web client **must hold a screen wake lock** while in show mode — a locked tablet is a missed cue
- If the platform denies the wake lock, the client says so at join time rather than failing silently during the show

### Audio degradation

- Channel loss escalates through the input-health warnings (Family B); the degrade path is per-channel → remaining channels → single mixed feed, and tracking quality follows gracefully rather than collapsing

---

## Run Log

The server keeps a **flight recorder** for every run: an append-only JSONL file beside the show file (one per performance/rehearsal) recording timestamped position updates, warnings fired and acknowledged, divergence and input-health events, manual interventions (jumps, drives, corrections, pauses) with attribution, and per-channel confidence summaries.

- Write-only during the show; reviewed afterwards
- Tech-week tuning: "why did it lose act 2 scene 3 every night?" is answerable from the log, not from memory
- Post-show note review shows each note in its run context
- This is the substrate that makes future automated rehearsal reports nearly free

---

## Show File Format

A show is a single open, versioned, UTF-8 JSON document:

- `"format": "choufleur-show"`, `"formatVersion": "major.minor"` — readers ignore and preserve unknown fields; only major versions break
- Shared production content (script, characters, cue/landmark layers) is separated from per-operator content (notes, filter preferences), with per-operator export/merge fragments
- Orphaned annotations from script amendments are carried in the file until resolved
- Timestamped backups are written before every re-import

The schema, worked examples, and all normative rules are in [choufleur-notation_1.md](choufleur-notation_1.md) (§11).

---

## Repository Structure

Single repository containing server and client as sibling projects.

```
choufleur/
├── server/          # Rust — audio capture, ASR, position tracking, web serving
├── remote/          # Flutter/Dart — web-first client; iPad/Android variants
├── docs/            # PRD, notation spec, architecture notes
│   ├── choufleur-prd_1.md
│   └── choufleur-notation_1.md
├── LICENSE-MIT      # MIT OR Apache-2.0 dual license
├── LICENSE-APACHE   #
└── README.md
```

The remote is not useful without the server and vice versa — keeping them together avoids split repo friction during development.

---

## Out of Scope for MVP

- Automatic cue triggering of any kind
- Cloud connectivity or internet dependency
- Audience-facing features
- Video display
- Segment-level (mid-line) language code-switching
- Visual, timer, and manual cue anchors — the notation spec reserves them (§10) so they can arrive without a format break
- Haptic warning modality (planned later addition — see Warning Modalities Roadmap)
- Speaker diarization on the mixed feed
- OSC output to show control systems (post-MVP consideration)
- Automated rehearsal report generation (post-MVP consideration — the run log is its substrate)
- **Score following for musical theatre and opera** (future direction — track position in the music as well as the text; the notation spec reserves a `music` anchor kind (§10) so musical cues arrive without a format break)

---

## Open Questions

- **OSC output post-MVP** — could sync visual cue list position in QLab/EOS without triggering; worth exploring
- **Browser client depth** — full feature parity with installed client, or warnings and script display only?
- **Simultaneous speakers on mixed feed** — degraded but functional; needs testing with real show audio
- **Pace calibration** — how to handle pauses, musical numbers, scene changes in ETA calculation
- **Haptic modality** — which devices/wearables, and per-operator configuration model

---

## License

Choufleur is free software, dual-licensed under **MIT OR Apache-2.0** — the Rust ecosystem convention, letting users pick either. See `LICENSE-MIT` and `LICENSE-APACHE` at the repository root.

- Contributions are accepted under the same dual license (Apache-2.0's patent grant is why both are offered)
- Show files produced with Choufleur are **user data** — no license terms apply to them
- Whisper model weights are distributed under their own license (MIT for OpenAI's released weights) and are not part of Choufleur's source

---

## Related Projects

Choufleur is part of a broader open source theatre tooling ecosystem:

| Tool | Purpose |
|------|---------|
| **Tagada** | Stage position tracking |
| **S21 HiJack** | DiGiCo S21 snapshot system extension |
| **WFS DIY** | Wavefield synthesis — open source spatial audio |
| **Describer tool** | Audio description workspace for accessibility |

All open source, all built from direct professional practice (the sibling projects are GPL3; Choufleur is MIT/Apache-2.0). Tagada is also the intended `source` for future visual cue anchors (notation spec §10).

---

*"It's like a GPS for your script, so you always know where you are and what's coming up."*
