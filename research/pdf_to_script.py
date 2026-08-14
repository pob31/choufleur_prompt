#!/usr/bin/env python3
"""Convert a typeset script PDF into the Phase 0 script.json.

The third script convention this corpus has produced, and the one that carries the
most information. A .docx from rehearsals separates spoken text from notes by
*colour*; a Comédie-Française script does it by *capitals*; a script typeset for
publication does it by **typography**, and does it far more reliably than either:

    Optima-Bold     Philippe            the speaker
    Optima-Regular  Moi, je suis prêt.  what they say
    Optima-Italic   Silence.            what happens

That matters because the alternative is guesswork. `Philippe` and `Sublime.` are both
short lines starting with a capital, and no rule about case can separate them; the
weight can. So this reads font weight and slant, and hands the result to the same
speaker-folding, language-detection and structure logic the .docx importer uses —
including the misspelling fold, which is not academic here: this script names its own
character `Philipe` ten times.

    python pdf_to_script.py script.pdf -o script.json --report

Stage directions are kept out of the dialogue rather than deleted, on the same
principle as everywhere else: a line that is not spoken must never compete for a
match, but throwing it away loses the operator information they may want later.

Like its .docx sibling this is corpus preparation, not the real importer (Phase 1,
M1.2), and is expected to be thrown away when that lands.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import docx_to_script as base

try:
    import fitz  # PyMuPDF
except ImportError:
    sys.exit("PyMuPDF is needed: python3 -m pip install pymupdf")


# A page number sitting alone at the top of the page, and the separators a typesetter
# uses between movements. Neither is spoken and neither belongs to anybody.
def furniture(text: str) -> bool:
    t = text.strip()
    return not t or t.isdigit() or set(t) <= set("*-–—_ .")


def read_pdf_paragraphs(path: Path, min_size: float = 0.0) -> list[base.Paragraph]:
    """The PDF's lines as Paragraphs, with weight and slant read off the fonts.

    Lines are merged into a paragraph while the style holds and the text keeps
    flowing: a speech wrapping over three lines is one paragraph, and the bold name
    above it is another. Without that, every visual line becomes a script line and a
    long speech arrives as three unmatchable fragments.
    """
    doc = fitz.open(path)
    out: list[base.Paragraph] = []
    for pno in range(len(doc)):
        for block in doc[pno].get_text("dict")["blocks"]:
            for line in block.get("lines", []):
                spans = [s for s in line.get("spans", []) if s.get("text", "").strip()]
                if not spans:
                    continue
                text = "".join(s["text"] for s in spans)
                if furniture(text):
                    continue
                lead = spans[0]
                if lead["size"] < min_size:
                    continue
                bold = "bold" in lead["font"].lower() or bool(lead["flags"] & 2 ** 4)
                italic = "italic" in lead["font"].lower() or bool(lead["flags"] & 2 ** 1)
                # Italic is the typesetter saying "this is not speech". Modelled as a
                # colour so the existing spoken/not-spoken machinery applies unchanged.
                colours = {"ITALIC"} if italic else set()
                # Continue the previous paragraph when the style matches and neither
                # side looks finished — a wrapped speech, not a new one.
                if (
                    out
                    and not bold
                    and not out[-1].bold
                    and out[-1].colours == colours
                    and not out[-1].text.rstrip().endswith(("!", "?", ".", "…", ":", "»"))
                ):
                    out[-1].text = f"{out[-1].text.rstrip()} {text.strip()}"
                    continue
                out.append(
                    base.Paragraph(len(out), " ".join(text.split()), colours, bold, pno + 1)
                )
    return out


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("pdf", type=Path)
    ap.add_argument("-o", "--out", type=Path)
    ap.add_argument("--default-lang", default="fr")
    ap.add_argument("--title")
    ap.add_argument("--report", action="store_true", help="print every paragraph decision")
    ap.add_argument("--alias", action="append", default=[], metavar="FROM=TO")
    ap.add_argument("--not-speaker", action="append", default=[], metavar="NAME")
    ap.add_argument("--no-detect-lang", action="store_true")
    ap.add_argument("--min-size", type=float, default=0.0,
                    help="ignore text smaller than this, e.g. running heads")
    args = ap.parse_args()

    # Speakers here are named by weight, not by shouting.
    base.TYPOGRAPHIC_SPEAKERS = True

    paras = read_pdf_paragraphs(args.pdf, args.min_size)
    print(f"{len(paras)} paragraph(s) read")

    speakers = base.find_speakers(paras, keep_colours=set())
    for spec in args.alias:
        frm, _, to = spec.partition("=")
        if to:
            speakers[frm.strip()] = to.strip()
    for name in args.not_speaker:
        speakers.pop(name, None)

    folded = {k: v for k, v in speakers.items() if k != v}
    if folded:
        print("folded misspellings:")
        for k, v in sorted(folded.items()):
            print(f"    {k!r} -> {v!r}")

    script, decisions = base.convert(
        paras,
        keep_colours=set(),
        default_lang=args.default_lang,
        title=args.title or args.pdf.stem,
        speakers=speakers,
        detect=not args.no_detect_lang,
    )

    if args.report:
        for what, p in decisions:
            print(f"  {what:24} {p.text[:76]}")

    lines = script["lines"]
    print(f"\n{len(paras)} paragraph(s) -> {len(lines)} spoken line(s)")
    counts = {}
    for l in lines:
        counts[l["character"]] = counts.get(l["character"], 0) + 1
    print("lines per character: " + ", ".join(
        f"{k}: {v}" for k, v in sorted(counts.items(), key=lambda kv: -kv[1])
    ))
    for l in lines[:6]:
        print(f'   {l["character"]:<22} {l["text"][:70]}')

    if args.out:
        args.out.write_text(json.dumps(script, ensure_ascii=False, indent=1) + "\n")
        print(f"\nwrote {args.out}")
        print("next: assign channels in the manifest, then `choufleur-replay verify`")


if __name__ == "__main__":
    main()
