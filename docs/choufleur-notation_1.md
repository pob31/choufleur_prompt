# Choufleur — Notation and Show File Specification

*Normative specification for line identity, annotation layers, cue notation, language tagging, and the show file format*

Version 1.0 — Draft — August 2026

Dual-licensed under MIT OR Apache-2.0. Show files produced with this format are user data — no license terms apply to them.

---

## 1. Purpose and Scope

This document is the normative reference for:

- **Line identity** — how every script line receives a stable ID, and how IDs survive script rewrites, cuts, and re-imports (§3)
- **Shorthand notation** — the human-readable cue notation used on screen and typed inline in the source `.docx` (§4, §5)
- **Annotation layers** — cues, landmarks, language tags, operator notes, and personal cue categories (§6–§9)
- **Anchor model** — including reserved anchor kinds for future visual, timer, and manual cues (§10)
- **Show file format** — the open, versioned JSON document that carries a whole production (§11, §12)

It does **not** cover tracking algorithms, ASR configuration, or UI design — those live in the PRD (`choufleur-prd_1.md`).

---

## 2. Design Principles

1. **Pristine text.** Script text is never mutated by annotation. Cues, landmarks, and notes live in separate layers that point at lines; the text a client displays is exactly the text the playwright wrote.
2. **Stable anchors.** Layers reference **line IDs**, never line numbers, page numbers, or character offsets. Page and line numbers change with every edit; IDs don't.
3. **Human-readable everywhere.** The same shorthand language appears on operator screens and inline in the prep `.docx`. What you type is what you later read.
4. **Warn-only semantics.** Nothing in this notation can express "trigger". A cue in Choufleur is a *warning timestamp*, never an instruction to hardware. The word "go" in this spec always means "the moment the operator would act", purely as a reference point for warnings.
5. **No silent data loss.** Re-import may orphan annotations when lines are cut or heavily rewritten; orphans are preserved in the file and surfaced for manual resolution. Nothing is deleted without explicit user confirmation, and the show file is backed up before any operation that could orphan data (§3.4).
6. **Open format.** UTF-8 JSON, versioned, unknown fields preserved on round-trip. Any tool can read and write a show file.

---

## 3. Line Identity and Re-anchoring

This is the load-bearing problem of the format: scripts get rewritten and cut during production, and every annotation — a sound cue placed weeks ago, a note scribbled during a run — must follow its line through those changes.

### 3.1 Line ID format

```
L-<seq4>-<hash6>        e.g.  L-0142-a3f9c1
```

- `seq4` — zero-padded sequence number assigned at **first** import, in script order. It is never renumbered afterwards; after a re-import with insertions it no longer reflects position. It exists for human debugging and uniqueness, **not** as a position. Position is always the line's index in the script arrays.
- `hash6` — first 6 hex characters of `SHA-256( normalize(text) + "|" + characterId )`, where `normalize` = Unicode NFC → lowercase → strip punctuation → collapse whitespace.
- IDs are **opaque identifiers, not checksums**. When a line's text is later edited and the ID is retained through fuzzy matching (§3.3), the hash portion is *not* recomputed. Readers must never parse meaning out of an ID.
- IDs are never reused, even after a line is deleted.

### 3.2 Normalization (used for hashing and matching)

| Step | Rule |
|------|------|
| 1 | Unicode NFC normalization |
| 2 | Lowercase (locale-independent) |
| 3 | Strip punctuation (Unicode category P) |
| 4 | Collapse runs of whitespace to a single space, trim |

For unsegmented scripts (CJK, Thai, …) matching operates on character n-grams instead of word tokens; the normalization steps are identical.

### 3.3 Re-import re-anchoring algorithm (normative)

When an edited `.docx` is re-imported, new lines are matched to old lines in four passes. Each pass only considers lines not matched by an earlier pass.

**Pass 1 — Exact.** A new line whose normalized-text hash matches exactly one old line inherits that line's ID.

**Pass 2 — Order alignment.** Duplicate hashes (repeated lines — "Yes.", "What?") are resolved by longest-common-subsequence alignment over the hash sequences of the old and new scripts: repeated identical lines map to each other in order of appearance.

**Pass 3 — Fuzzy.** Each remaining new line is compared against remaining old lines within a ±10-line window of its LCS-interpolated position. A normalized similarity ≥ **0.8** (token-set ratio; character n-gram ratio for unsegmented scripts) is accepted as the same line, edited. The old ID is retained; the hash portion is not recomputed.

**Pass 4 — Orphans.** Old lines still unmatched that carry any anchored annotation (cue, landmark, note) become **orphaned anchors**: they are moved to the show file's `orphans` array together with their text, character, and the IDs of their last-known neighbouring lines, and are surfaced in the prep UI for manual reattachment. New lines with no match receive fresh IDs (next available `seq`). Old unmatched lines carrying *no* annotations are simply dropped from the script arrays (there is nothing anchored to lose).

### 3.4 Backup and consent rules (normative)

- Before any re-import, the current show file is copied to `backups/<showfile>.<UTC timestamp>.json` inside the show's folder. Re-import must refuse to run if the backup cannot be written.
- Orphaned annotations are resolved **only** by explicit user action: reattach to a chosen line, or confirm deletion. There is no automatic expiry, no bulk silent cleanup. A show file with unresolved orphans is valid and loads normally.
- The import report (§12) always lists: lines matched per pass, new lines, orphans created, and inline tags parsed/rejected.

### 3.5 Worked example

Old script:

```
L-0001-4be1a0   MARIE:  Tu ne devrais pas être ici.        ← cue "SND 7" anchored here
L-0002-99c2d7   JEAN:   Je sais.
L-0003-e0f114   MARIE:  Alors pars.                         ← note anchored here
```

The director cuts nothing but rewrites line 1 and inserts a new line before line 3. Re-import:

```
L-0001-4be1a0   MARIE:  Tu ne devrais pas être là.          ← Pass 3 fuzzy (0.86) — ID kept, cue follows
L-0002-99c2d7   JEAN:   Je sais.                            ← Pass 1 exact
L-0004-b7a2c9   JEAN:   Ne me demande pas ça.               ← new line, fresh ID (seq continues)
L-0003-e0f114   MARIE:  Alors pars.                         ← Pass 1 exact — note follows
```

Note that `L-0001`'s hash suffix no longer matches its text — by design (§3.1). If line 1 had instead been cut entirely, it would appear in `orphans` with its cue, flagged for the operator to reattach.

---

## 4. Shorthand Notation — Display Form

The canonical on-screen rendering of a cue:

```
LX Q12 @-30s — House preset warm
SND Q7.5 @-60s,-10s — Wind bed fade in
```

### 4.1 Grammar (EBNF)

```ebnf
cue        = type SP number [SP leadlist] [SP "—" SP label] ;
type       = "LX" | "SND" | "VID" | "FLY" | "SM" | custom ;
custom     = 2*4( "A"…"Z" | "0"…"9" ) ;        (* registered per show, §6.2 *)
number     = ["Q"] 1*16( alnum | "." ) ;       (* opaque string; "Q" preserved as typed *)
leadlist   = lead *( "," lead ) ;              (* descending, earliest first *)
lead       = "@-" duration ;
duration   = minutes "m" [seconds "s"] | seconds "s" ;
label      = free text to end of field ;
```

### 4.2 Semantics

- **Types.** Five built-in types: `LX` (lighting), `SND` (sound), `VID` (video), `FLY` (flys), `SM` (stage management). Productions may register custom types (2–4 uppercase alphanumerics, e.g. `PYRO`, `AUTO`) in the show file's `cueTypes` registry (§6.2).
- **Numbers are opaque strings, namespaced per type.** `LX 12` and `SND 12` coexist. `12.5` sorts between `12` and `13` for display but the system attaches no arithmetic meaning. The `Q` prefix is cosmetic and preserved as typed.
- **Lead list = warning stages.** `@-60s,-10s` produces a *standby* warning 60 s before the anchor and a *final* warning 10 s before. A single lead produces exactly one warning. In every case the anchor moment itself produces the "now" frame flash. Warnings are exactly what is written — a single lead does **not** get an implicit second stage.
- **Latency floor.** The warning scheduler subtracts measured pipeline latency from leads (see PRD, *ASR Engine and Latency Budget*). Leads shorter than the measured latency are delivered as early as physically possible and visually marked as degraded.

---

## 5. Inline Shorthand — `.docx` Import Form

During prep, tags can be typed directly into the Word script inside braces. On import they are parsed into layers and stripped from the stored text.

### 5.1 Grammar (EBNF)

```ebnf
inline      = "{" tag "}" ;
tag         = cuetag | landmarktag | langtag | notetag ;

cuetag      = type ":" number [SP leadlist-nl] [SP label] ;
leadlist-nl = lead-nl *( "," lead-nl ) ;
lead-nl     = "-" duration ;                   (* "@" omitted in inline form *)

landmarktag = "LM" [":" weight] ;              (* weight = "1" | "2" | "3", default 2 *)
langtag     = "LANG" ":" langspec ;
langspec    = bcp47 *( "+" bcp47 ) ;           (* e.g. fr+en for a bilingual line *)
notetag     = "NOTE" ":" free text ;
```

Examples as typed in the `.docx`:

```
HAMLET: To be, or not to be, that is the question. {LX:12 -30s House preset warm}

MARIE: Tu ne devrais pas être ici. {SND:7 -60s,-10s Wind bed fade in} {LM:3}

INGRID: Jag förstår inte. I don't understand any of it. {LANG:sv+en}

JEAN: Alors pars. {NOTE:director wants a longer beat before this}
```

### 5.2 Import rules (normative)

- **Anchoring.** A tag anchors to the line (paragraph) that contains it. A tag standing alone in its own paragraph anchors to the **preceding** dialogue line.
- **Stripping.** Parsed tags are removed from the stored text; whitespace at the removal point is normalized. The pristine line text is what remains.
- **Escaping.** Literal braces in script text are written `{{` and `}}`; the importer unescapes them.
- **Errors.** A brace group that does not parse is left in the text **untouched** and listed in the import report. The importer never guesses.
- **Bidi.** Tags are Latin-script tokens with explicit delimiters. The importer isolates brace groups from surrounding bidirectional runs before parsing, so tags embedded in Arabic or Hebrew paragraphs parse identically.
- All four tag kinds (`cue`, `LM`, `LANG`, `NOTE`) are importable. `NOTE` tags import into the *importing operator's* note layer (§9).

---

## 6. Cue Layer

### 6.1 Cue object

```json
{
  "id": "cue-lx-12",
  "type": "LX",
  "number": "12",
  "anchor": { "kind": "line", "lineId": "L-0142-a3f9c1" },
  "leads": [30],
  "label": "House preset warm",
  "createdBy": "import",
  "notes": []
}
```

- `id` — unique within the show file, conventionally `cue-<type>-<number>` lowercased.
- `leads` — seconds, descending (earliest warning first), mapping to §4.2 stages.
- `anchor` — a discriminated union; v1 always `{"kind": "line", ...}` (see §10).
- `createdBy` — `"import"` (from inline shorthand) or `"prep"` (placed in the UI); informational.

### 6.2 Cue type registry

```json
"cueTypes": [
  { "type": "LX",   "name": "Lighting",        "color": "#E5B800" },
  { "type": "SND",  "name": "Sound",           "color": "#3FA7D6" },
  { "type": "VID",  "name": "Video",           "color": "#9C6ADE" },
  { "type": "FLY",  "name": "Flys",            "color": "#E0533D" },
  { "type": "SM",   "name": "Stage management","color": "#59A96A" },
  { "type": "PYRO", "name": "Pyrotechnics",    "color": "#FF6F00" }
]
```

The five built-ins are always present; custom entries follow the `custom` rule of §4.1. Operators' cue filters (PRD, *Per-technician cue filtering*) select on `type`.

---

## 7. Landmark Layer

```json
"landmarks": [
  { "lineId": "L-0142-a3f9c1", "weight": 3 }
]
```

- `weight` ∈ {1, 2, 3} — a re-anchoring confidence multiplier for the position tracker. 3 = "this phrase is unmistakable and unique in the play".
- Scene and act boundaries are **implicit weight-3 landmarks**; they need no entry here. Explicit landmarks add re-anchoring points inside scenes. Act boundaries additionally carry hold semantics (see PRD, *Show structure and holds*).

---

## 8. Language Tagging

Language is a property of the text itself, so it is stored **inline** on script objects rather than as a detached layer — the one deliberate exception to the layers model.

### 8.1 Codes

BCP-47 tags — in practice ISO 639-1 subtags (`fr`, `en`, `sv`, `ar`, `ja`), with region only where it changes the ASR model or matching behaviour (`pt-BR`).

### 8.2 Inheritance (most specific wins)

```
line.lang  →  character.lang  →  scene.lang  →  act.lang  →  show.defaultLang
```

A `lang` field is always an **array**. A line tagged `["fr", "en"]` is bilingual: the tracker decodes/matches it against both languages and keeps the better score. `null` or absent means "inherit".

### 8.3 Scope of v1

Line-level is the finest granularity. Mid-line, per-word code-switching is **out of scope for v1**; a half-French half-English line is simply tagged `["fr", "en"]`. Fields are already arrays, so no format change is needed if segment-level tagging arrives later.

---

## 9. Operator Layers — Notes and Categories

Everything under `operators.<opId>` is personal: notes, filter preferences, and cue categories. Two operators can use identical category names without any interaction — the namespace is the operator, so `"music"` for the sound operator and `"music"` for the video operator are unrelated objects.

### 9.1 Notes

Notes are per-operator and private by default.

```json
"operators": {
  "sound-pierre": {
    "displayName": "Pierre",
    "filter": ["SND", "SM"],
    "categories": [
      { "id": "cat-qlab",    "name": "QLab",        "color": "#3FA7D6" },
      { "id": "cat-ableton", "name": "Ableton Live","color": "#9C6ADE" },
      { "id": "cat-console", "name": "Console",     "color": "#E5B800" },
      { "id": "cat-spatial", "name": "Spatial",     "color": "#59A96A" },
      { "id": "cat-music",   "name": "Music",       "color": "#E0533D" }
    ],
    "cueCategories": { "cue-snd-7": "cat-qlab" },
    "notes": [
      {
        "id": "note-0007",
        "anchor": { "kind": "line", "lineId": "L-0142-a3f9c1" },
        "text": "Check compressor release before this",
        "createdAt": "2026-08-12T20:14:00Z",
        "shared": false
      }
    ]
  }
}
```

- `anchor.kind` may be `"line"` or `"cue"` (`"ref"`: cue id) — a note can annotate a cue itself.
- `shared: true` opts a note into visibility for all operators; default is private.
- Show-mode double-tap notes append here via the `note_add` WebSocket message and are reviewed post-show.
- Clients render notes beside the script (wide layouts) or as tappable bubbles anchored to their line (narrow layouts) — see PRD, *Architecture / Client*.
- Notes are annotations anchored to line IDs and therefore **survive script re-imports** via §3; orphaned notes follow the §3.4 consent rules.

### 9.2 Personal cue categories

Cue **types** (§6.2) are shared production vocabulary — LX, SND, VID… Cue **categories** are a personal, second-level organization an operator lays over the cues they follow: a sound operator might split theirs into *QLab*, *Ableton Live*, *Console*, *Spatial*, *Music*.

- `categories` — the operator's own registry: `{ "id", "name", "color" }`. Free-form names; each operator configures their own set.
- `cueCategories` — a map from cue id to one of the operator's category ids. Unassigned cues are simply *uncategorized*; assignment is always optional.
- **Independence is structural.** Categories live only under the owner's `operators.<opId>` subtree and reference shared cues by id. They never appear on the shared cue object (§6.1), so identically named categories belonging to different operators can never collide, merge, or leak into each other's displays.
- Clients may use categories for grouping, color accents, and secondary filtering within the operator's cue filter.
- Categories travel with the operator fragment (§11.2): exporting an operator's subtree carries `categories` and `cueCategories` along with notes and filters. On merge, category assignments referencing a cue id that no longer exists are surfaced for review, never silently dropped (§2, principle 5).
- Categories have no inline `.docx` shorthand — they are personal organization, configured in the client, not part of the shared script annotation pass (§5).

---

## 10. Anchor Model and Reserved Anchor Kinds

`anchor` is a discriminated union on `kind`. **v1 implements only `line`.** The following kinds are reserved by this spec — readers must not error on them, and leads/labels are anchor-independent, so the cue grammar (§4) is unchanged whichever anchor a cue uses:

| Kind | Shape | Meaning (future) |
|------|-------|------------------|
| `line` | `{ "kind": "line", "lineId": "L-…" }` | Warn relative to a script line being reached (v1) |
| `manual` | `{ "kind": "manual" }` | Armed only by an operator action |
| `timer` | `{ "kind": "timer", "afterRef": "<cueId or lineId>", "offsetSec": 45 }` | Warn N seconds after another event |
| `visual` | `{ "kind": "visual", "source": "<detectorId>", "event": "<name>" }` | Driven by a stage event, not dialogue — e.g. a performer position from **Tagada** |
| `music` | `{ "kind": "music", "source": "<followerId>", "position": { "bar": 24, "beat": 3 } }` | Driven by musical score position from a future score-following engine — musical theatre, opera |

This is how visual and musical cues enter the system later without a format break.

---

## 11. Show File Schema

A show file is a single UTF-8 JSON document. Version 1 uses no container; a zip container (`.chou`) bundling calibration audio and the source `.docx` is a possible v2.

### 11.1 Top-level structure

```json
{
  "format": "choufleur-show",
  "formatVersion": "1.0",
  "meta": {
    "title": "La Mouette / The Seagull",
    "company": "Théâtre de l'Est",
    "created": "2026-06-02T10:00:00Z",
    "modified": "2026-08-12T20:14:00Z",
    "sourceDocx": { "filename": "mouette-v4.docx", "sha256": "…" }
  },
  "defaultLang": ["fr"],
  "cueTypes": [ { "type": "LX", "name": "Lighting", "color": "#E5B800" } ],
  "characters": [
    { "id": "char-marie", "name": "MARIE", "lang": null, "channels": [3] },
    { "id": "char-ingrid", "name": "INGRID", "lang": ["sv"], "channels": [5] }
  ],
  "acts": [
    {
      "id": "act-1",
      "title": "Act 1",
      "lang": null,
      "scenes": [
        {
          "id": "sc-1-2",
          "title": "Scene 2",
          "lang": null,
          "lines": [
            { "id": "L-0142-a3f9c1", "character": "char-marie",
              "text": "Tu ne devrais pas être ici.", "lang": null },
            { "id": "L-0143-77d0be", "character": "char-ingrid",
              "text": "Jag förstår inte. I don't understand any of it.",
              "lang": ["sv", "en"] }
          ]
        }
      ]
    }
  ],
  "layers": {
    "cues": [
      { "id": "cue-snd-7", "type": "SND", "number": "7",
        "anchor": { "kind": "line", "lineId": "L-0142-a3f9c1" },
        "leads": [60, 10], "label": "Wind bed fade in",
        "createdBy": "import", "notes": [] }
    ],
    "landmarks": [ { "lineId": "L-0142-a3f9c1", "weight": 3 } ]
  },
  "operators": {
    "sound-pierre": {
      "displayName": "Pierre", "filter": ["SND"],
      "categories": [ { "id": "cat-qlab", "name": "QLab", "color": "#3FA7D6" } ],
      "cueCategories": { "cue-snd-7": "cat-qlab" },
      "notes": []
    }
  },
  "orphans": [],
  "calibration": { "paceByScene": {}, "channelProfiles": {} }
}
```

### 11.2 Rules

- **Versioning.** `formatVersion` is `major.minor`. Readers accept any file with a known major version and **ignore and preserve unknown fields** (minor versions only add fields). Breaking changes bump the major version.
- **Shared vs per-operator.** Everything outside `operators.<opId>` is shared production content. Each operator's filter preferences, personal cue categories (§9.2), and notes live under their key.
- **Operator fragments.** An operator can export their layer as a fragment file — `"format": "choufleur-operator"`, carrying one `opId` subtree — which merges into a show file by `opId` (notes merged by note `id`, newest `createdAt` wins on conflict; `categories`/`cueCategories` replaced wholesale as one personal set). This is how personal notes and categories move between a home tablet and the venue file.
- **Orphans** (§3) live at top level so any tool can surface them.
- **Encoding.** UTF-8 throughout, NFC-normalized text on import.

---

## 12. Import / Export Lifecycle

**First import.** `.docx` → parse OOXML (`word/document.xml`) → unescape/parse/strip inline tags (§5) → assign line IDs (§3.1) → build layers → write show file → **import report** (tags parsed, brace groups rejected, language coverage per scene, characters detected).

**Re-import (script amended).** Timestamped backup (§3.4) → parse new `.docx` → 4-pass re-anchoring (§3.3) → orphan report → user resolves orphans in prep UI (reattach or confirm-delete) — annotations are never silently dropped.

**Operator export/merge.** Any operator subtree can be exported as a `choufleur-operator` fragment and merged into another copy of the show file (§11.2).

---

## 13. End-to-End Example

`.docx` source paragraph (bilingual Swedish/English production, French default):

```
INGRID: Jag förstår inte. I don't understand any of it. {LANG:sv+en} {SND:7 -60s,-10s Wind bed fade in} {LM:3}
```

After import, the stored line (pristine, tags stripped):

```json
{ "id": "L-0143-77d0be", "character": "char-ingrid",
  "text": "Jag förstår inte. I don't understand any of it.",
  "lang": ["sv", "en"] }
```

Cue and landmark layers gain:

```json
{ "id": "cue-snd-7", "type": "SND", "number": "7",
  "anchor": { "kind": "line", "lineId": "L-0143-77d0be" },
  "leads": [60, 10], "label": "Wind bed fade in", "createdBy": "import", "notes": [] }
```

```json
{ "lineId": "L-0143-77d0be", "weight": 3 }
```

On the sound operator's screen, approaching the line:

```
SND Q7 @-60s,-10s — Wind bed fade in
```

— standby frame at −60 s, final warning at −10 s, "now" flash when the line is reached. The tracker matches the line against both Swedish and English models; the operator decides when to act. Choufleur only taps the shoulder.
