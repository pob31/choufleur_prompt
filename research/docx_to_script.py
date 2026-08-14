#!/usr/bin/env python3
"""Convert a rehearsal .docx into the Phase 0 script.json.

A *Regiefassung* is not a clean script. It is the script plus everything the
production has written on top of it: lighting cues, camera cues, stage directions,
situation notes. Those must not reach the tracker — a note like "Johan sits down at
the table" is never spoken, so as a script line it can only ever be a false match
competing with real dialogue.

The separating signal is colour. In the La Reprise Regiefassung, and in most
rehearsal scripts kept this way, **black text is spoken and coloured text is not**.
That is a convention, not a guarantee, so this tool prints what it decided and
`--report` shows every judgement for eyeballing before you trust it.

    python docx_to_script.py script.docx -o script.json --report
    python docx_to_script.py script.docx --colour-map        # what colours exist

This is corpus preparation, deliberately not the real importer. The .docx importer
with the full inline-cue grammar of notation spec §5 is Phase 1 (M1.2) and belongs
in Rust; this is the sidecar that gets a corpus built today, and it is expected to
be thrown away when M1.2 lands.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import unicodedata
import zipfile
from collections import Counter
from pathlib import Path
from xml.etree import ElementTree as ET

W = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"

# A speaker attribution: "Johan:", "JOHAN (mike not amplified):", "Sara & Fabian:".
SPEAKER = re.compile(
    r"^\s*([A-Za-zÀ-ÿ][A-Za-zÀ-ÿ'’.-]*(?:\s*[&/+]\s*[A-Za-zÀ-ÿ][A-Za-zÀ-ÿ'’.-]*)*)"
    r"\s*(\([^)]*\))?\s*:\s*(.*)$"
)


def norm_name(s: str) -> str:
    """Fold a speaker name to a comparable key: NFC, casefold, no accents."""
    s = unicodedata.normalize("NFD", s.strip())
    s = "".join(c for c in s if unicodedata.category(c) != "Mn")
    return s.casefold()


def attribution(p: "Paragraph"):
    """`(name, parenthetical, rest)` if this paragraph opens with `Name:`."""
    m = SPEAKER.match(p.text)
    if not m:
        return None
    name = m.group(1).strip()
    if len(name) > 32 or len(name.split()) > 5:
        return None
    return name, m.group(2), m.group(3)


class Paragraph:
    __slots__ = ("index", "text", "colours", "bold")

    def __init__(self, index: int, text: str, colours: set, bold: bool):
        self.index = index
        self.text = text
        self.colours = colours
        self.bold = bold

    @property
    def spoken_colour(self) -> bool:
        """True when nothing in this paragraph is coloured (i.e. plain black)."""
        return not self.colours


def read_paragraphs(path: Path) -> list[Paragraph]:
    z = zipfile.ZipFile(path)
    root = ET.fromstring(z.read("word/document.xml"))
    out = []
    for i, p in enumerate(root.iter(f"{W}p")):
        runs = []
        for r in p.iter(f"{W}r"):
            t = "".join(n.text or "" for n in r.iter(f"{W}t"))
            if not t:
                continue
            rpr = r.find(f"{W}rPr")
            colour, bold = None, False
            if rpr is not None:
                c = rpr.find(f"{W}color")
                if c is not None:
                    colour = (c.get(f"{W}val") or "").upper()
                    # "auto" means the theme default, i.e. black.
                    if colour in ("AUTO", "000000"):
                        colour = None
                bold = rpr.find(f"{W}b") is not None
            runs.append((t, colour, bold))
        text = "".join(t for t, _, _ in runs)
        # Word uses non-breaking spaces liberally in French typography.
        text = text.replace("\xa0", " ").strip()
        if not text:
            continue
        out.append(
            Paragraph(
                i,
                text,
                {c for _, c, _ in runs if c},
                any(b for _, _, b in runs),
            )
        )
    return out


def colour_map(paras: list[Paragraph]) -> None:
    counts = Counter()
    for p in paras:
        counts[tuple(sorted(p.colours)) or ("<black>",)] += 1
    print(f"{len(paras)} non-empty paragraphs\n")
    for cols, n in counts.most_common():
        label = ",".join(cols)
        sample = next(
            (p.text for p in paras if (tuple(sorted(p.colours)) or ("<black>",)) == cols),
            "",
        )
        print(f"  {label:<22} {n:>4}  {sample[:78]!r}")


def find_speakers(paras: list[Paragraph], keep_colours: set[str]) -> dict[str, str]:
    """Decide which `Name:` labels are speakers, and fold the misspellings together.

    Two problems, both present in any real rehearsal script.

    *False speakers.* `Situation:`, `Dialogue:` and `Image:` look exactly like
    attributions. The rule that separates them: a speaker is a label that at least
    once introduces **spoken** text. A section label only ever introduces notes.

    *Misspellings.* The La Reprise Regiefassung contains Sébastien and Sébsatien,
    Fabian and Fabien, Wintgens and Wingtens, Leukeu and Lekeu. Left alone each
    typo becomes a separate character with no channel, and every line under it is
    unmatchable. Names within one edit of a much more frequent name are folded into
    it; the fold is reported so a wrong guess is visible rather than silent.
    """
    introduces_speech: Counter[str] = Counter()
    seen: Counter[str] = Counter()
    pending = None
    for p in paras:
        m = attribution(p)
        if m:
            name = m[0]
            seen[name] += 1
            rest = m[2].strip()
            if rest and (p.spoken_colour or p.colours & keep_colours):
                introduces_speech[name] += 1
                pending = None
            else:
                pending = name
            continue
        if pending and (p.spoken_colour or p.colours & keep_colours):
            introduces_speech[pending] += 1
            pending = None

    speakers = {n for n, c in introduces_speech.items() if c > 0}
    # Fold rare misspellings into the frequent name they are one small edit from.
    canonical: dict[str, str] = {}
    ranked = sorted(speakers, key=lambda n: (-introduces_speech[n], n))
    for name in ranked:
        target = None
        for other in ranked:
            if other == name or introduces_speech[other] <= introduces_speech[name] * 2:
                continue
            if one_edit_apart(norm_name(name), norm_name(other)):
                target = other
                break
        canonical[name] = target or name
    return canonical


def one_edit_apart(a: str, b: str) -> bool:
    """Within one transposition or edit — enough for a typed name, not for two names."""
    if a == b:
        return True
    if abs(len(a) - len(b)) > 1 or min(len(a), len(b)) < 4:
        return False
    if sorted(a) == sorted(b):  # transposition: sebastien / sebsatien
        return True
    # single insertion, deletion or substitution
    if len(a) == len(b):
        return sum(x != y for x, y in zip(a, b)) == 1
    short, long = (a, b) if len(a) < len(b) else (b, a)
    for i in range(len(long)):
        if long[:i] + long[i + 1 :] == short:
            return True
    return False


# Function words that are common in one language and rare in the others. Whole
# sentences are being classified, so a handful of markers each is ample; this is
# not a general language identifier and does not need to be.
LANG_MARKERS = {
    "nl": set("de het een ik je niet dat en van is zijn maar ze wij hij ook nog wat "
              "heb heeft was werd om te op met voor door naar als dan want er dit "
              "die deze mijn jouw zo veel altijd nooit misschien omdat".split()),
    "fr": set("le la les un une des et de du au aux je tu il elle nous vous ils "
              "elles que qui pas ne est sont était avec pour dans sur par plus "
              "mais donc alors comme très tout tous cette ces mon ton son".split()),
    "en": set("the a an and of to in is are was were i you he she we they that "
              "this these those with for on by not but so as at from have has "
              "had will would can could there their what when".split()),
}


def detect_lang(text: str) -> str | None:
    """Which of nl / fr / en a line is in, or `None` when it cannot be called.

    Language is the single most expensive thing to get wrong: whisper decodes a
    French line forced to Dutch as confident nonsense, with no error anywhere. So
    this reports uncertainty rather than guessing, and the caller resolves it from
    context — see `smooth_languages`.
    """
    words = re.findall(r"[a-zà-ÿ']+", text.casefold())
    if len(words) < 4:
        return None
    scores = {lang: sum(w in markers for w in words) for lang, markers in LANG_MARKERS.items()}
    best = max(scores, key=lambda k: scores[k])
    runner = max((v for k, v in scores.items() if k != best), default=0)
    # Demand a clear win: French and English share a great deal of vocabulary.
    if scores[best] >= 2 and scores[best] > runner:
        return best
    return None


def smooth_languages(langs: list[str | None], default: str) -> list[str]:
    """Fill in the lines that could not be called, from their neighbours.

    Short lines carry too few function words to classify — and a verse line of
    Hamlet carries almost no *modern* ones at all. Falling back to the show default
    is exactly wrong for them: in an excerpt with no French in it, every uncertain
    line would be tagged French. A line's neighbours are far better evidence, since
    a speaker does not usually change language mid-speech. The show default is used
    only when nothing anywhere in the script could be called.
    """
    out = list(langs)
    last = None
    for i, l in enumerate(out):
        if l is not None:
            last = l
        elif last is not None:
            out[i] = last
    # Anything before the first confident line takes the first one that follows.
    nxt = None
    for i in range(len(out) - 1, -1, -1):
        if out[i] is not None:
            nxt = out[i]
        elif nxt is not None:
            out[i] = nxt
    return [l or default for l in out]


def convert(
    paras: list[Paragraph],
    keep_colours: set[str],
    default_lang: str,
    title: str | None,
    speakers: dict[str, str],
    detect: bool,
) -> tuple[dict, list[tuple[str, Paragraph]]]:
    """Walk the paragraphs, emitting a script line per spoken paragraph.

    A speaker attribution sets the current speaker and, if the same paragraph
    carries text after the colon, that text is the speaker's first line.
    """
    lines: list[dict] = []
    decisions: list[tuple[str, Paragraph]] = []
    speaker = None
    seq = 1

    for p in paras:
        spoken = p.spoken_colour or bool(p.colours & keep_colours)

        m = attribution(p)
        if m and m[0] in speakers:
            speaker = speakers[m[0]]
            rest = m[2].strip()
            decisions.append((f"speaker={speaker}", p))
            if not rest:
                continue
            text = rest
            spoken = True  # text after an attribution is dialogue whatever its colour
        elif m:
            # A `Name:` label that never introduces speech: a section heading.
            decisions.append((f"label={m[0]}", p))
            continue
        else:
            text = p.text

        if not spoken:
            decisions.append(("note", p))
            continue
        if speaker is None:
            # Front matter: title, epigraph, version line.
            decisions.append(("preamble", p))
            continue
        if len(text) < 2:
            decisions.append(("too-short", p))
            continue

        entry = {
            "id": f"L-{seq:04d}",
            "act": "act-1",
            "scene": "sc-1",
            "character": f"char-{norm_name(speaker).replace(' ', '-')}",
            "text": text,
        }
        if detect:
            entry["_detected"] = detect_lang(text)
        lines.append(entry)
        decisions.append((f"line {lines[-1]['id']}", p))
        seq += 1

    if detect:
        smoothed = smooth_languages([ln.pop("_detected") for ln in lines], default_lang)
        for ln, lang in zip(lines, smoothed):
            if lang != default_lang:
                ln["lang"] = [lang]

    names = {}
    for ln in lines:
        names.setdefault(ln["character"], ln["character"].removeprefix("char-"))
    script = {
        "format": "choufleur-script",
        "formatVersion": "0.1",
        "title": title,
        "defaultLang": [default_lang],
        "characters": [
            {"id": cid, "name": nm.upper(), "lang": None, "channels": []}
            for cid, nm in sorted(names.items())
        ],
        "lines": lines,
    }
    return script, decisions


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("docx", type=Path)
    ap.add_argument("-o", "--out", type=Path)
    ap.add_argument("--default-lang", default="fr")
    ap.add_argument("--title")
    ap.add_argument(
        "--keep-colour",
        action="append",
        default=[],
        metavar="RRGGBB",
        help="treat this colour as spoken text too (repeatable)",
    )
    ap.add_argument("--colour-map", action="store_true", help="report colours and exit")
    ap.add_argument("--report", action="store_true", help="print every paragraph decision")
    ap.add_argument(
        "--alias",
        action="append",
        default=[],
        metavar="FROM=TO",
        help="fold a speaker name into another (repeatable), for names too short to guess",
    )
    ap.add_argument(
        "--not-speaker",
        action="append",
        default=[],
        metavar="NAME",
        help="a label that introduces text but is not spoken aloud, e.g. a projected "
        "caption tagged `Image:` (repeatable)",
    )
    ap.add_argument(
        "--no-detect-lang",
        action="store_true",
        help="tag every line with the show default instead of detecting nl/fr/en",
    )
    args = ap.parse_args()

    paras = read_paragraphs(args.docx)
    if args.colour_map:
        colour_map(paras)
        return

    keep = {c.upper().lstrip("#") for c in args.keep_colour}
    speakers = find_speakers(paras, keep)
    for n in args.not_speaker:
        speakers.pop(n, None)
        for k, v in list(speakers.items()):
            if v == n:
                speakers.pop(k)
    for a in args.alias:
        frm, _, to = a.partition("=")
        if not to:
            sys.exit(f"--alias expects FROM=TO, got {a!r}")
        speakers[frm] = to
    folded = {k: v for k, v in speakers.items() if k != v}
    if folded:
        print("folded misspelled speakers:")
        for k, v in sorted(folded.items()):
            print(f"    {k!r} -> {v!r}")
    script, decisions = convert(
        paras,
        keep,
        args.default_lang,
        args.title or args.docx.stem,
        speakers,
        not args.no_detect_lang,
    )

    if args.report:
        for what, p in decisions:
            print(f"  {what:<16} [{','.join(sorted(p.colours)) or '-':<14}] {p.text[:88]}")
        print()

    kinds = Counter(w.split()[0] for w, _ in decisions)
    print(f"{len(paras)} paragraphs -> {len(script['lines'])} spoken line(s)")
    print("  " + ", ".join(f"{k}: {v}" for k, v in kinds.most_common()))
    langs = Counter(tuple(l.get("lang", [args.default_lang])) for l in script["lines"])
    print("languages: " + ", ".join(f"{l[0]}: {n}" for l, n in langs.most_common()))
    print(f"characters: {', '.join(c['name'] for c in script['characters'])}")
    if not script["lines"]:
        sys.exit("no spoken lines found — check --colour-map and the speaker pattern")

    out = args.out or args.docx.with_suffix(".script.json")
    out.write_text(json.dumps(script, ensure_ascii=False, indent=1) + "\n")
    print(f"wrote {out}")
    print("next: assign channels in the manifest, then `choufleur-replay verify`")


if __name__ == "__main__":
    main()
