#!/usr/bin/env python3
"""Type the stage directions in a script, and mark the ones tracking should wait out.

A didascalie is not dialogue and should not be read as though it were. Two consequences
the operator asked for, and they are different: it should *look* different on the page,
and where it describes music, silence or an improvised passage it should stop the
tracker predicting what it is about to hear.

Detection comes from the source document rather than from guesswork wherever possible.
French scripts set didascalies in italic, and the Hécube DOCX does: 40 mostly-italic
paragraphs. That is evidence; a keyword list is not. Compare the two on this material
and the difference is stark — searching for "silence" finds Éric saying *"Silence, mes
amies"* twenty times, while the italics find the single stage direction that says
`Silence.` and nothing else.

Two things the naive version gets wrong, both learned from this script:

**Not every italic paragraph is a stage direction.** More than half are speaker
attributions carrying a manner — `NADIA, s'asseyant.`, `POLYMESTOR, toujours plus
rapidement`, `GAËL (en même temps)`. They are already handled as speakers and must not
become lines.

**Not every stage direction is unspoken.** The cast list contains `SÉPHORA, lit les
didascalies` — a performer whose job is to read them aloud, and she does, for the
quoted Euripides directions like *« Polymestor s'avance. »*. So marking a line `stage`
never excludes it from matching. That is `hold`'s job, and `hold` is only inferred
where the text says the stage is doing something other than speaking.

    python mark_didascalies.py script.json --docx source.docx
    python mark_didascalies.py script.json --docx source.docx --write
"""

from __future__ import annotations

import argparse
import json
import re
import unicodedata
import zipfile
from pathlib import Path

W = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"

# Set-piece directions that are their own whole line. Matched exactly, after folding —
# never as substrings, because "Silence, mes amies." is a line Éric says out loud.
FORMULAS = {
    "pause": None,
    "un temps": None,
    "un temps long": None,
    "silence": "silence",
    "long silence": "silence",
    "noir": None,
    "blackout": None,
}

# What the stage is doing, when the direction says so.
HOLDS = [
    ("music", r"\b(musique|chanson|chorégraphie|chante|danse|danser|orchestre)\b"),
    ("silence", r"\b(silence|sans un mot|personne ne parle)\b"),
    # Non-speech vocalisation belongs here too. It is off-script by definition, and it
    # is what the recogniser mangles worst: Nadia barking came back as "PAS PAS PAS".
    ("adlib", r"\b(improvis|en meme temps|parlent presque|se coupent la parole|brouhaha"
              r"|aboie|aboya|aboiement|hurle|crie|rit|pleure|chuchote"
              r"|plusieurs (autres )?disent|tous parlent)\b"),
]


def fold(text: str) -> str:
    t = unicodedata.normalize("NFD", text.lower())
    t = "".join(c for c in t if unicodedata.category(c) != "Mn")
    return " ".join(re.findall(r"[a-z']+", t))


def italic_paragraphs(docx: Path) -> list[str]:
    """Paragraphs whose text is mostly italic, in document order."""
    xml = zipfile.ZipFile(docx).read("word/document.xml").decode("utf8")
    out: list[str] = []
    for para in re.findall(r"<w:p(?: [^>]*)?>(.*?)</w:p>", xml, re.S):
        runs = re.findall(r"<w:r(?: [^>]*)?>(.*?)</w:r>", para, re.S)
        text = "".join("".join(re.findall(r"<w:t[^>]*>(.*?)</w:t>", r, re.S)) for r in runs)
        if not text.strip():
            continue
        italic = sum(
            len("".join(re.findall(r"<w:t[^>]*>(.*?)</w:t>", r, re.S)))
            for r in runs
            if "<w:i/>" in r
        )
        if italic >= 0.6 * len(text):
            out.append(text.strip())
    return out


def is_speaker_attribution(text: str) -> bool:
    """`NADIA, s'asseyant.` and `GAËL (en même temps)` are speakers, not directions.

    The tell is an opening run of capitals — a name — before any lower-case word.
    """
    head = re.split(r"[,(]", text, 1)[0].strip()
    letters = [c for c in head if c.isalpha()]
    return bool(letters) and all(c.isupper() for c in letters) and len(head.split()) <= 4


def infer_hold(text: str) -> str | None:
    folded = fold(text)
    if folded in FORMULAS:
        return FORMULAS[folded]
    for hold, pattern in HOLDS:
        if re.search(pattern, folded):
            return hold
    return None


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("script", type=Path)
    ap.add_argument("--docx", type=Path, help="source document, for its italics")
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args()

    script = json.loads(args.script.read_text())
    lines = script["lines"]

    marked: set[str] = set()
    if args.docx:
        italics = [p for p in italic_paragraphs(args.docx) if not is_speaker_attribution(p)]
        marked = {fold(p) for p in italics if fold(p)}
        print(f"{len(italics)} italic paragraphs are stage directions "
              f"({len(italic_paragraphs(args.docx)) - len(italics)} were speaker attributions)")

    stage = holds = 0
    report: list[tuple] = []
    for line in lines:
        folded = fold(line["text"])
        if not folded:
            continue
        # The line is a didascalie if the source set it in italic, or if it is one of
        # the set-piece formulas standing alone.
        if folded not in marked and folded not in FORMULAS:
            continue
        line["kind"] = "stage"
        stage += 1
        hold = infer_hold(line["text"])
        if hold:
            line["hold"] = hold
            holds += 1
        else:
            line.pop("hold", None)
        report.append((line["id"], hold, line["text"]))

    print(f"\n{stage} lines marked as stage directions, {holds} of them holding tracking\n")
    for lid, hold, text in report:
        tag = f"hold:{hold}" if hold else "—"
        print(f"  {lid}  {tag:<14}{text[:74]!r}")

    if args.write:
        args.script.write_text(json.dumps(script, ensure_ascii=False, indent=1) + "\n")
        print(f"\nwritten: {args.script}")
    else:
        print("\n(dry run — pass --write to update the script)")


if __name__ == "__main__":
    main()
