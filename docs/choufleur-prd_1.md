# Choufleur — Product Requirements Document

*A script GPS for theatre technicians*

Version 0.2 — Draft — August 2026

Licensed under GPL-3.0-or-later — see the License section.

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
- Role picker on join — each operator selects which cue types they follow
- Per-technician cue filter — each operator sees only their relevant cues
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

Available on both server and client devices.

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

### Network audio

- Dante ubiquitous in theatre — joins existing venue network infrastructure
- Dante Virtual Soundcard is a paid per-machine Audinate license; hardware alternatives (Dante AVIO adapters, PCIe cards) exist where licensing is a concern
- No special hardware beyond what the venue already has

---

## Multi-Operator Protocol

All clients share the server's authoritative position by default.

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

Defined normatively in the notation spec (§8): a show default, overridable per scene, per character, and per line; most specific wins. A bilingual line is tagged with both languages (`["sv", "en"]`) and matched against both, keeping the better score. Mid-line, per-word code-switching is **out of scope for v1**.

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
| Server | macOS primary (Apple Silicon M1+ baseline), Windows secondary (best-effort, reduced channels) |
| Client | Web-first (any browser on the venue network); iPad/Android as installable Flutter variants |
| Audio framework | JUCE |
| ASR engine | Whisper family via whisper.cpp or faster-whisper; model size per show/language (see ASR section) |
| Script import | .docx (OOXML) parsed server-side |
| Show file | Open versioned JSON per [choufleur-notation_1.md](choufleur-notation_1.md) |

Note: the browser client implies WebSocket as the transport — browsers speak neither raw OSC nor UDP, which reinforces the protocol choice below.

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

### Prep / rehearsal mode — REST

REST handles all request-response interactions during prep. Cleaner than fitting configuration through a WebSocket.

- Load and save show files
- Script import and character assignment
- Cue tagging and landmark configuration
- Audio device configuration
- Per-operator cue filter preferences

### Why not OSC

OSC excels at continuous parameter streams — fader values, spatial coordinates. Choufleur's messages are event-driven and structured. Forcing cue warnings and position corrections into OSC address patterns adds friction without benefit.

### Why not MQTT

Elegant for multi-client broadcast via topic subscriptions, but requires a broker process running alongside the server. WebSocket fan-out to connected clients is sufficient for Choufleur's scale.

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
├── server/          # JUCE application — audio, ASR, position tracking
├── remote/          # Flutter/Dart — web-first client; iPad/Android variants
├── docs/            # PRD, notation spec, architecture notes
│   ├── choufleur-prd_1.md
│   └── choufleur-notation_1.md
├── LICENSE          # GPL-3.0-or-later
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
- Automated rehearsal report generation (post-MVP consideration)

---

## Open Questions

- **OSC output post-MVP** — could sync visual cue list position in QLab/EOS without triggering; worth exploring
- **Browser client depth** — full feature parity with installed client, or warnings and script display only?
- **Simultaneous speakers on mixed feed** — degraded but functional; needs testing with real show audio
- **Pace calibration** — how to handle pauses, musical numbers, scene changes in ETA calculation
- **Haptic modality** — which devices/wearables, and per-operator configuration model

---

## License

Choufleur is free software, licensed under the **GNU General Public License v3.0 or later** (GPL-3.0-or-later). See the `LICENSE` file at the repository root.

- Show files produced with Choufleur are **user data** — the GPL does not apply to them
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

All GPL3. All built from direct professional practice. Tagada is also the intended `source` for future visual cue anchors (notation spec §10).

---

*"It's like a GPS for your script, so you always know where you are and what's coming up."*
