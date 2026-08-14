#!/usr/bin/env python3
"""Read a cue sheet written as a table — the other kind of conduite.

`conduite_to_cues.py` reads a marked-up script: the operator's notes live in the
margins of the pages, as PDF annotations. Plenty of operators do not work that way.
They keep a separate sheet, one page for the whole show, and reference the script by
page number:

    Q2
    p.29
    Cut up la route
    « Il était une fois dans l'Ouest maintenant ce serait parfait » > respiration puis top

Nothing about the annotation reader applies here — this document has no annotations at
all. Two things do carry across, and they are the two that matter.

**The grammar is the same.** *What I wait for* `>` *what I then do*, with triggers
either quoted verbatim from the text or watched for (*"Vincent face contre terre >"*,
*"Étreinte >"*). Worth being precise about how much that proves: both conduites in
this corpus are by the **same operator**, two years and two productions apart. So the
arrow is one person's habit holding across their own shows — enough to rely on for
them, and not evidence about anybody else. Whether the trade shares it is untested,
and the honest position until a third operator's sheet turns up.

**The page reference does the locating.** The marked-up script needed its pages
aligned to the script by matching text, because a margin note only knows where it sits.
A cue sheet states the page outright, so if the script was imported from that same PDF
— carrying each line's page number — the search narrows to a dozen lines before any
matching happens. A quoted trigger then lands exactly.

    python cuesheet_to_cues.py "conduite son.pdf" script.json -o cues.json
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import unicodedata
from pathlib import Path

from conduite_to_cues import colour_name

try:
    import fitz  # PyMuPDF
except ImportError:
    sys.exit("PyMuPDF is needed: python3 -m pip install pymupdf")

CUE = re.compile(r"^\s*(Q\s*\d+[a-z]?)\s*$", re.I)
PAGE = re.compile(r"\bp\.?\s*(\d{1,3})(?!\d)", re.I)
# Text the operator quoted as the thing to listen for. Guillemets are the French
# convention; straight and curly doubles are accepted so this is not one locale wide.
QUOTED = re.compile(r"[«\"“]\s*(.+?)\s*[»\"”]", re.S)


def normalize(text: str) -> str:
    out: list[str] = []
    pending = False
    for ch in unicodedata.normalize("NFC", text).lower():
        if ch.isspace() or unicodedata.category(ch).startswith("P"):
            pending = bool(out)
        else:
            if pending:
                out.append(" ")
                pending = False
            out.append(ch)
    return "".join(out)


def tokens(text: str) -> set[str]:
    return {w for w in normalize(text).split(" ") if w}


def dice(a: set[str], b: set[str]) -> float:
    return 2 * len(a & b) / (len(a) + len(b)) if a and b else 0.0


def split_note(text: str) -> tuple[str | None, str]:
    """`(what is waited for, what is then done)`, on the arrow."""
    head, sep, tail = text.partition(">")
    if not sep or not head.strip() or not tail.strip():
        return None, text.strip()
    return head.strip(), tail.strip()


def read_cues(pdf: Path, means: dict[str, str]) -> list[dict]:
    """Cue blocks, split on the cue numbers, with each run of text kept under the
    colour it was written in.

    The sheet is colour-coded, and the coding separates three different instructions
    that a plain text extraction runs together: what to set up beforehand, what the
    cue is, and what to do when it fires. On this one red is a preset, black the cue
    and blue the action — stated by the operator, not inferred here, for the same
    reason as everywhere else.

    White is a special case that is not a fourth meaning: parameter values are set in
    white on a coloured chip (`full`, `-30dB`, `-∞`), so they belong to whatever run
    they interrupt and are folded back into it.
    """
    doc = fitz.open(pdf)
    spans = []
    for pno in range(len(doc)):
        for block in doc[pno].get_text("dict")["blocks"]:
            for line in block.get("lines", []):
                for s in line.get("spans", []):
                    t = s["text"]
                    if not t.strip():
                        continue
                    rgb = s.get("color", 0)
                    c = ((rgb >> 16 & 255) / 255, (rgb >> 8 & 255) / 255, (rgb & 255) / 255)
                    spans.append((pno, round(s["bbox"][1] / 3), s["bbox"][0],
                                  t, colour_name(c)))
    spans.sort(key=lambda s: (s[0], s[1], s[2]))

    cues, current, colour, row = [], None, None, None
    for _, this_row, _, text, name in spans:
        if CUE.match(text.strip()):
            if current:
                cues.append(current)
            current = {"id": text.strip().upper().replace(" ", ""), "runs": []}
            colour, row = None, None
            continue
        if current is None:
            continue
        if name == "white" and current["runs"]:
            name = current["runs"][-1][0]  # a value on a chip, not a meaning
        if name != colour or not current["runs"]:
            current["runs"].append([name, text])
            colour = name
        else:
            # A line break inside a run is still a space. Without it "p.4" and
            # "Falling" join into "p.4Falling" and the page reference stops parsing —
            # which silently cost every page number on this sheet.
            gap = " " if this_row != row else ""
            current["runs"][-1][1] += gap + text
        row = this_row
    if current:
        cues.append(current)

    for c in cues:
        by_meaning: dict[str, list[str]] = {}
        for name, text in c["runs"]:
            t = " ".join(text.split())
            if t:
                by_meaning.setdefault(means.get(name, name), []).append(t)
        c["parts"] = {k: " ".join(v) for k, v in by_meaning.items()}
        c["text"] = " ".join(" ".join(t for _, t in c["runs"]).split())
        m = PAGE.search(c["text"])
        c["page"] = int(m.group(1)) if m else None
    return cues


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("pdf", type=Path)
    ap.add_argument("script", type=Path)
    ap.add_argument("-o", "--out", type=Path)
    ap.add_argument("--slack", type=int, default=1, help="pages either side of the reference")
    ap.add_argument("--anchor", type=float, default=0.34)
    ap.add_argument("--colour-means", nargs="*", default=[], metavar="NAME=MEANING",
                    help="what this sheet's text colours mean — no default and no "
                         "standard, e.g. --colour-means red=preset black=cue blue=action")
    args = ap.parse_args()

    means: dict[str, str] = {}
    for spec in args.colour_means:
        name, _, meaning = spec.partition("=")
        if meaning:
            means[name.strip()] = meaning.strip()

    script = json.loads(args.script.read_text())
    lines = script["lines"]
    line_toks = [tokens(l["text"]) for l in lines]
    paged = any("page" in l for l in lines)
    if not paged:
        print("This script carries no page numbers, so page references cannot be used.")
        print("Re-import it with pdf_to_script.py to get them.")

    cues = read_cues(args.pdf, means)
    print(f"{len(cues)} cue(s) on the sheet\n")

    # A cue sheet is in show order and not every row repeats the page — a cue on the
    # same page as the one before it, or a fade closing the cue above, simply says
    # nothing. Carrying the last page forward keeps those from searching the whole
    # script, which is how "on attend quoi..." matched "Quoi ?" four hundred lines
    # early.
    last_page = None
    for c in cues:
        if c["page"] is None:
            c["page"], c["pageInherited"] = last_page, True
        else:
            last_page = c["page"]

    out, anchored, visual, floor = [], 0, 0, 0
    for c in cues:
        # Every quoted fragment is a candidate trigger; the operator may quote the
        # line before the cue and another further on in the same block.
        quotes = [q for q in QUOTED.findall(c["text"]) if len(q.split()) >= 2]
        watch, action = split_note(c["text"])
        window = range(len(lines))
        if paged and c["page"] is not None:
            near = [i for i, l in enumerate(lines)
                    if l.get("page") is not None
                    and abs(l["page"] - c["page"]) <= args.slack]
            if near:
                window = range(near[0], near[-1] + 1)

        best, best_i, on = 0.0, None, None
        for q in quotes or ([watch] if watch else []):
            want = tokens(q or "")
            for i in window:
                if i < floor - 4:
                    continue
                sc = dice(want, line_toks[i])
                if sc > best:
                    best, best_i, on = sc, i, q
        rec = {"cue": c["id"], "page": c["page"], "text": c["text"][:180],
               "parts": c["parts"], "quoted": quotes, "score": round(best, 3)}
        if c.get("pageInherited"):
            rec["pageInherited"] = True
        if quotes:
            rec["trigger"] = "text"
        else:
            rec["trigger"] = "visual"
            visual += 1
            if watch:
                rec["waitsFor"] = watch
        if best_i is not None and best >= args.anchor:
            rec["lineId"] = lines[best_i]["id"]
            rec["lineText"] = lines[best_i]["text"][:80]
            rec["matched"] = on
            floor = best_i
            anchored += 1
        elif paged and c["page"] is not None and window:
            rec["lineId"] = lines[window[0]]["id"]
            rec["anchor"] = "page"
            anchored += 1
        out.append(rec)

    print(f"anchored: {anchored}/{len(cues)}   "
          f"text-triggered {len(cues) - visual}, watched-for {visual}\n")
    for r in out:
        print(f'  {r["cue"]:<4} p.{str(r["page"] or "-"):<4} {r.get("lineId","—"):<8} '
              f'{r["score"]:.2f} {r["trigger"]}')
        if r.get("matched"):
            print(f'         heard:  « {r["matched"][:64]} »')
            print(f'         script: {r.get("lineText","")[:64]}')
        elif r.get("waitsFor"):
            print(f'         watch:  {r["waitsFor"][:64]}')

    if args.out:
        args.out.write_text(json.dumps(
            {"format": "choufleur-cues", "formatVersion": "0.1",
             "source": args.pdf.name, "cues": out}, ensure_ascii=False, indent=1) + "\n")
        print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
