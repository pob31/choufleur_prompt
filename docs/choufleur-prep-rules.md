# Preparing a Choufleur script

You are turning a theatre script into the file Choufleur follows during a performance.
Read the whole script first. It matters more that you understand what kind of document
this is than that you apply any rule below quickly.

Output a single JSON object, described at the end. Nothing else — no commentary, no
fences.

## What this file is for

An operator sits in the dark with this text on a screen. Speech recognition listens to
the performance and moves a highlight down the page so they know where the company has
got to, and how long until their next cue. Everything below follows from that.

Two failure modes, and they are not equal:

- **A line that is on the page but wrong** — mis-attributed, mistyped — is visible. The
  operator sees it, fixes it in ten seconds, and the show is unaffected.
- **A line that is missing** is invisible. The tracker sails past the place it should
  have been, the operator loses their position, and nobody finds out why until afterwards.

So: **never drop text.** If you cannot decide what a paragraph is, make it a dialogue
line and leave it. Every paragraph of the source must appear somewhere in the output.
This is checked mechanically and the import is refused if text has gone missing.

## Read the script. Do not write a program to read it.

The temptation, faced with six hundred paragraphs, is to write a script that finds the
speakers with a regular expression and emits the JSON. Resist it.

Choufleur already has that program. It is deliberately simple, it handles the explicit
cases — a colon, a bracket, a leading number — and you are being asked instead precisely
because those rules break on material like this. `SILENCE` looks like a name. `1.AM I A
HUMAN (Presenting everyone)` looks like a name. Three consecutive lines reading `S`, `O`
and `S` look like three characters, and cost 344 of 589 lines when a rule believed them.
Writing your own rules reproduces those failures in a new language, with the added
disadvantage that nobody can see what they were.

The value you add is judgement, and judgement does not survive being compiled into a
regular expression. So read the text. Where a passage is irregular — and it will be,
because scripts are written by people over years, under pressure — decide about that
passage, not about a pattern it might belong to.

Automation is fine for the mechanical half: extracting the words from a document,
counting paragraphs, checking your own output. It is the classification that must not be
automated.

**Say what you were unsure about.** After the JSON, in a separate message, list the
decisions you would want a human to look at: passages you could not classify, names you
suspect are the same person spelled two ways, sections where you guessed. An operator
will rework the script anyway; a short list of where to start is worth more than a
confident silence.

## Two shapes you will meet

They want different things from you.

**A dialogue play** — *Hécube, pas Hécube* is one — has many short lines, named speakers
alternating, stage directions woven through, and cut passages. The work is attribution
and typing: who says this, is that a didascalie, is this line struck. Nine hundred lines,
twenty-three characters. Most of the difficulty is here.

**A devised piece** — *Lovedoll* — is big slabs of text under section headings, one or
two performers per section, no dialogue and often no attribution at all. The work is
structure: where the sections begin, which passages are lists rather than prose. Do not
manufacture speakers for it. An empty `character` is the correct answer far more often
than it looks.

## Lines

One paragraph of the source is one line. Do not merge lines, and do not split them.
A speech that runs for two hundred words is one line; the tracker handles it, and cues
are anchored to lines, so splitting one silently moves every annotation attached to it.

Keep the text **exactly as written**, including punctuation, capitalisation, quotation
marks and typos. It is compared against what the actors actually say, and the comparison
already handles the difference. Do not tidy it, do not correct spelling, do not expand
abbreviations, do not translate.

## Who speaks

`character` holds a character **id**, and every id used must appear in `characters`.

Attribution is usually explicit — `NADIA :` or a name alone above the speech. Where the
script never names anybody, leave `character` empty on every line. That is a real and
common shape, especially in devised work, and an empty attribution costs nothing: the
tracker simply does not use speaker identity on that show. **Inventing speakers is much
worse than leaving them out.**

Watch for these, which are not speakers:

- A stage direction in capitals — `SILENCE`, `NOIR`, `PAUSE` — reads exactly like a name.
- A section title, especially a numbered one — `3.TOUGH COOKIES (TEXT boys)`.
- Single letters on consecutive lines. Somebody is spelling a word out.

A manner attached to a name — `NADIA, s'asseyant.` or `GAËL (en même temps)` — belongs
to the speaker, not to the text. The character is `NADIA`; the manner is dropped.

**Groups.** If a name covers several performers — a chorus, "the boys", two actors
sharing a line — give it its own entry in `characters` and list the ids it is made of in
`members`, but **only if the script tells you who they are**. If it does not, leave
`members` empty and let the operator fill it in. Never guess at a cast list.

## Stage directions

`kind: "stage"` marks a didascalie. It is shown differently on the page and, by default,
is not looked for in the audio.

`spoken` says whether somebody reads it out loud. Most directions are not spoken, but
some productions have a performer who reads them — so write `spoken` explicitly on every
stage line rather than leaving it to the default. If the script gives you no way to tell,
write `false`.

## Holds

`hold` marks a passage where the tracker should stop predicting and wait: `"silence"`,
`"music"`, or `"adlib"` for improvisation, overlapping speech, shouting, crowd noise —
anything off-script by nature. Tracking resumes at the next line it recognises.

Use it where the script says the stage is doing something other than delivering text.
Do **not** use it for a passage that is simply long, fast or difficult; the tracker is
built for those.

A line carrying a hold is never matched, so do not put a hold on a line whose words are
actually spoken.

## Structure

`act` and `scene` are ids, taken from the headings the script uses. Where a script has
numbered parts instead of acts and scenes — very common in devised work — use the part
number as the scene and put every line in one act.

Keep the section title itself as a line, with `kind: "stage"`, so the operator can see
where they are. It is often the only structure the page has.

## Language

`defaultLang` is the language of the show. Where a script moves between languages, tag
the lines that differ with their own `lang`. A line that mixes two languages within one
sentence gets both, most-spoken first.

## Output

```json
{
  "format": "choufleur-script",
  "formatVersion": "0.1",
  "title": "…",
  "defaultLang": ["fr"],
  "characters": [
    { "id": "char-nadia", "name": "NADIA", "channels": [], "members": [] }
  ],
  "lines": [
    {
      "id": "L-0001",
      "act": "act-1",
      "scene": "scene-1",
      "character": "char-nadia",
      "text": "Exactly as written in the source.",
      "kind": "dialogue",
      "spoken": true,
      "hold": null,
      "lang": null
    }
  ]
}
```

- `id` — `L-0001` upward, in order, never repeated.
- `character` — an id from `characters`, or `""`.
- `kind` — `"dialogue"` or `"stage"`.
- `spoken` — required on stage lines; omit on dialogue.
- `hold` — `"silence"`, `"music"`, `"adlib"`, or omitted.
- `lang` — omit unless this line differs from `defaultLang`.
- `channels` — always `[]`. Microphones are patched by the operator, never guessed.

## Before you answer

Count the paragraphs in the source. Count the lines in your output. If the second number
is smaller, you have dropped text — go back and find it. Section titles, single words,
stray fragments and things you were unsure about all count.
