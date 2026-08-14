#!/usr/bin/env python3
"""Read an operator's marked-up conduite PDF and attach its cues to script lines.

The devplan assumes cues get typed in. They usually do not have to be. An operator
who has run a show already owns a conduite — the script as a PDF, with the cues
written in the margin and the trigger words highlighted — and that document holds
the two things Choufleur needs: *what happens* and *when*. Typing all of it again is
a few hours of work with a fresh opportunity for error on every line.

    python conduite_to_cues.py conduite.pdf script.json -o cues.json

The margin notes come out as FreeText annotations and the trigger marks as
Highlights, both carrying a position on the page. Anchoring is therefore geometric
first — a cue belongs to whatever it was written next to — and textual second: the
words near it are matched against the script to find the line.

**Pages are aligned before cues are.** Anchoring each note by its own marked words
fails, and fails in a way worth recording: the marks are short — *"polymestor"*,
*"Je te remercie."* — and this script repeats whole passages, because the company
reads Euripides in scene 2 and performs it in scene 4. Matching short text against a
script that says the same thing three times puts cues anywhere, and a forward-only
rule then makes it worse: one wrong early anchor drags the floor past most of the
show and every later cue starves. Measured, that approach placed 11%.

So the page is aligned first. A page carries eight or ten lines of dialogue, which is
far more evidence than any single highlight, and page order is show order. Each page
is matched to the run of script lines printed on it; only then is a cue placed among
that page's lines using the words beside it. The ambiguity the mark cannot resolve,
its page can.

Every cue is emitted with the evidence for its anchor — the marked words and the
match score — because a cue on the wrong line is worse than a cue nobody entered.
Review before a run.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import unicodedata
from pathlib import Path

try:
    import fitz  # PyMuPDF
except ImportError:
    sys.exit("PyMuPDF is needed: python3 -m pip install pymupdf")


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


# A cue whose text names a visual event rather than a spoken one. These fire on what
# the operator sees, not on what is said, and Choufleur can only ever show them
# early — it cannot know that the cloth has fallen. Marked so the client can present
# them differently rather than implying a precision it does not have.
VISUAL = re.compile(
    r"^(.{0,40}?)\s*>\s*(.+)$|entre\b|sortie|assis|se retourne|enjambe|pose son|"
    r"tombe|approche|retour sur|hors plateau|en place",
    re.I,
)


def colour_name(colour) -> str:
    """A stable, human label for a highlight colour, so it can be named on the
    command line without anybody typing RGB triples."""
    if not colour or len(colour) < 3:
        return "none"
    r, g, b = colour[:3]
    if max(r, g, b) - min(r, g, b) < 0.08:
        light = (r + g + b) / 3
        return "white" if light > 0.95 else "black" if light < 0.2 else "grey"
    h = max(r, g, b)
    if r == h and g >= 0.75 * r:
        return "yellow" if b < 0.6 and g > 0.7 else "orange"
    if r == h:
        return "red" if b < 0.5 else "pink"
    if g == h:
        return "green"
    return "blue" if b > r else "purple"


def extract(pdf: Path):
    """Every cue note and every highlight, with the page text around it."""
    doc = fitz.open(pdf)
    cues, marks = [], []
    for pno in range(len(doc)):
        page = doc[pno]
        words = page.get_text("words")  # (x0, y0, x1, y1, word, block, line, wno)
        for a in page.annots() or []:
            r = a.rect
            kind = a.type[1]
            if kind == "FreeText":
                content = " ".join((a.info.get("content") or "").split())
                if content:
                    cues.append({"page": pno + 1, "y": r.y0, "x": r.x0, "text": content})
            elif kind in ("Highlight", "Underline", "Squiggly", "StrikeOut"):
                covered = " ".join(
                    w[4] for w in words if fitz.Rect(w[:4]).intersects(r)
                )
                if covered.strip():
                    c = a.colors.get("stroke") or a.colors.get("fill")
                    marks.append(
                        {"page": pno + 1, "y": r.y0, "text": " ".join(covered.split()),
                         "kind": kind, "colour": colour_name(c),
                         "rgb": tuple(round(x, 2) for x in c) if c else None}
                    )
    return doc, cues, marks


def page_lines(doc) -> dict[int, list[tuple[float, str]]]:
    """Each page's body text as (y, text) rows, so a margin note can be read against
    whatever it sits beside."""
    out: dict[int, list[tuple[float, str]]] = {}
    for pno in range(len(doc)):
        rows: dict[int, list[str]] = {}
        for x0, y0, x1, y1, word, *_ in doc[pno].get_text("words"):
            rows.setdefault(round(y0 / 4), []).append(word)
        out[pno + 1] = sorted((k * 4.0, " ".join(v)) for k, v in rows.items())
    return out


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("pdf", type=Path)
    ap.add_argument("script", type=Path)
    ap.add_argument("-o", "--out", type=Path, help="write cues.json")
    ap.add_argument("--anchor", type=float, default=0.34,
                    help="match score below which a cue is left unanchored")
    ap.add_argument("--band", type=float, default=90.0,
                    help="how far above a note to read the page, in points")
    ap.add_argument("--printed", type=float, default=0.65,
                    help="how much of a line must appear on a page to count as printed there")
    ap.add_argument("--behind", type=int, default=25,
                    help="lines to search behind the pointer when locating a page")
    ap.add_argument("--spread", type=int, default=30,
                    help="how many lines a single page may span")
    ap.add_argument("--lookahead", type=int, default=60,
                    help="lines to search ahead when locating a page")
    ap.add_argument("--segments", type=Path, nargs="*", default=[],
                    help="one segments.jsonl per run; lets the recording say which "
                         "highlight colour marks cuts rather than assuming a convention")
    ap.add_argument("--cut-colour", nargs="*",
                    help="name the colour(s) that mark cuts, e.g. --cut-colour grey. "
                         "Overrides whatever the recording suggests")
    ap.add_argument("--cut-rate", type=float, default=25.0,
                    help="a colour whose lines are performed this rarely marks cuts")
    ap.add_argument("--heard", type=float, default=0.6,
                    help="how much of a line must appear in the audio to count as performed")
    args = ap.parse_args()

    script = json.loads(args.script.read_text())
    lines = script["lines"]
    line_toks = [tokens(l["text"]) for l in lines]

    doc, cues, marks = extract(args.pdf)
    body = page_lines(doc)

    cues.sort(key=lambda c: (c["page"], c["y"], c["x"]))

    # Which script lines are printed on each page. Containment, not similarity: the
    # page holds many lines, so asking "how much of this line appears here" is the
    # question with signal, while asking how alike the two texts are is not.
    spans: dict[int, tuple[int, int]] = {}
    pointer = 0
    for pno in sorted(body):
        have = tokens(" ".join(t for _, t in body[pno]))
        if not have:
            continue
        hits = []
        # A little slack behind the pointer as well as ahead: a page that overshoots
        # would otherwise strand every page after it, since the lines they print sit
        # behind where the search now starts.
        for i in range(max(0, pointer - args.behind), min(len(lines), pointer + args.lookahead)):
            want = line_toks[i]
            if len(want) >= 3 and len(want & have) / len(want) >= args.printed:
                hits.append(i)
        if hits:
            # Keep the densest run of hits. A page that quotes a line printed
            # elsewhere gets one stray match far away, and taking first-to-last would
            # stretch its span over half the show.
            run: list[int] = []
            for a in range(len(hits)):
                here = [i for i in hits[a:] if i - hits[a] <= args.spread]
                if len(here) > len(run):
                    run = here
            spans[pno] = (run[0], run[-1])
            # Advance past what this page printed, less one line for the overlap a
            # page break creates. Advancing only to the page's *first* line leaves the
            # pointer barely moving, and the alignment then crawls: measured, it
            # reached line 672 of 984 by the last page and put page 16 before page 15.
            pointer = max(pointer, run[-1] - 1)
    if spans:
        print(f"page alignment: {len(spans)}/{len(body)} page(s) located, "
              f"L-{min(s for s, _ in spans.values()) + 1:04d} … "
              f"L-{max(e for _, e in spans.values()) + 1:04d}")

    located = sorted(spans)

    def window(pno: int) -> tuple[int, int]:
        """The lines a cue on this page may anchor to, widened to the neighbouring
        pages: a note sits beside the line it fires on, which may be the last line of
        the page before."""
        near = [spans[p] for p in (pno - 1, pno, pno + 1) if p in spans]
        if near:
            return min(s for s, _ in near), max(e for _, e in near)
        # A page nothing could be located on is still *between* two that could. Left
        # unbounded it searches the whole script, and short marks — "Ça va ?", which
        # this play says many times — then anchor anywhere: one such cue landed 900
        # lines out. Bounding by the neighbours costs nothing and cannot do that.
        before = [spans[p][0] for p in located if p < pno]
        after = [spans[p][1] for p in located if p > pno]
        return (before[-1] if before else 0), (after[0] if after else len(lines) - 1)

    # Which lines each mark lies over. Containment bounded to the mark's page —
    # unbounded, a marked epilogue matched seventeen lines scattered through the
    # show, because this play repeats itself and a short mark carries no location.
    for m in sorted(marks, key=lambda m: (m["page"], m["y"])):
        want = tokens(m["text"])
        lo, hi = window(m["page"])
        m["lines"] = [
            i for i in range(lo, hi + 1)
            if len(line_toks[i]) >= 3
            and len(line_toks[i] & want) / len(line_toks[i]) >= args.printed
        ]

    # What each colour *means* is one operator's habit on one show, so it is not
    # guessed from the colour. It is measured, when a recording is offered: a mark
    # over text that is still performed is a trigger, and a mark over text nobody
    # says is a cut. That reads the convention off the evidence instead of assuming
    # this conduite's grey is anyone else's.
    heard: set[int] = set()
    if args.segments:
        passages: list[set[str]] = []
        for path in args.segments:
            recs = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
            kept = [tokens(r["text"]) for r in recs
                    if not r.get("interim") and not r.get("filtered")]
            for i in range(len(kept)):
                merged = set()
                for k in range(3):
                    if i + k >= len(kept):
                        break
                    merged |= kept[i + k]
                    passages.append(set(merged))
        for i, want in enumerate(line_toks):
            if len(want) >= 3 and any(
                len(want & p) / len(want) >= args.heard for p in passages
            ):
                heard.add(i)

    palette: dict[str, dict] = {}
    for m in marks:
        p = palette.setdefault(m["colour"], {"marks": 0, "lines": set(), "multi": 0})
        p["marks"] += 1
        p["lines"].update(m["lines"])
        if len(m["lines"]) > 1:
            p["multi"] += 1

    print("\nHIGHLIGHT COLOURS — what each one turned out to cover")
    for name, p in sorted(palette.items(), key=lambda kv: -kv[1]["marks"]):
        n = len(p["lines"])
        if args.segments and n:
            rate = 100 * len(p["lines"] & heard) / n
            verdict = "CUT?" if rate <= args.cut_rate else "trigger"
            note = f"{rate:3.0f}% still performed   {verdict}"
        else:
            note = f'{p["multi"]} mark(s) span several lines'
        print(f'  {name:<8} {p["marks"]:3d} mark(s), {n:3d} line(s)   {note}')
    if not args.segments:
        print("  (pass --segments to have the recording say which colour means cut)")

    chosen = set(args.cut_colour or [])
    if not chosen and args.segments:
        chosen = {
            name for name, p in palette.items()
            if p["lines"] and 100 * len(p["lines"] & heard) / len(p["lines"]) <= args.cut_rate
        }
    cut_ids, cut_marks = [], []
    for m in sorted(marks, key=lambda m: (m["page"], m["y"])):
        if m["colour"] not in chosen:
            continue
        cut_marks.append(m)
        for i in m["lines"]:
            if lines[i]["id"] not in cut_ids:
                cut_ids.append(lines[i]["id"])

    # A cut is not a trigger: the text under it is precisely what is no longer said,
    # so it must not be offered as the words a cue fires on.
    by_page: dict[int, list[dict]] = {}
    for m in marks:
        if m["colour"] not in chosen:
            by_page.setdefault(m["page"], []).append(m)

    out, anchored = [], 0
    for cue in cues:
        # What was marked, or failing that what is printed just above the note.
        near = [m for m in by_page.get(cue["page"], [])
                if -args.band <= m["y"] - cue["y"] <= 24.0]
        if near:
            near.sort(key=lambda m: abs(m["y"] - cue["y"]))
            evidence, source = near[0]["text"], "highlight"
        else:
            rows = [t for y, t in body.get(cue["page"], [])
                    if -args.band <= y - cue["y"] <= 12.0]
            evidence, source = " ".join(rows[-3:]), "page"

        want = tokens(evidence)
        lo, hi = window(cue["page"])
        best, best_i = 0.0, None
        for i in range(lo, hi + 1):
            sc = dice(want, line_toks[i])
            if sc > best:
                best, best_i = sc, i
        # No usable mark, but the page is located: the cue still belongs somewhere on
        # it. Falling back to the page's first line is a coarse answer, and saying so
        # is better than dropping a cue the operator wrote down.
        if best_i is None or best < args.anchor:
            if cue["page"] in spans:
                # Place it by where on the page it was written. A note two thirds of
                # the way down belongs two thirds of the way through the page's lines
                # — coarse, but far better than dropping it or pinning every such cue
                # to the top of the page.
                s, e = spans[cue["page"]]
                rows = body.get(cue["page"]) or []
                top, bot = (rows[0][0], rows[-1][0]) if rows else (0.0, 1.0)
                frac = (cue["y"] - top) / (bot - top) if bot > top else 0.0
                best_i = s + round(max(0.0, min(1.0, frac)) * (e - s))
                best, rec_page_only = 0.0, True
            else:
                rec_page_only = False
        else:
            rec_page_only = False

        rec = {
            "cue": cue["text"],
            "page": cue["page"],
            "evidence": evidence[:120],
            "evidenceFrom": source,
            "score": round(best, 3),
        }
        if VISUAL.search(cue["text"]):
            rec["trigger"] = "visual"
        if best_i is not None:
            rec["lineId"] = lines[best_i]["id"]
            rec["lineText"] = lines[best_i]["text"][:90]
            rec["anchor"] = "page" if rec_page_only else "text"
            anchored += 1
        out.append(rec)

    print(f"{len(cues)} cue note(s) in {len(doc)} pages, {len(marks)} mark(s)")
    print(f"anchored to a script line: {anchored}/{len(cues)} "
          f"({100 * anchored / max(1, len(cues)):.0f}%)\n")
    for r in out[:30]:
        where = r.get("lineId", "  —   ")
        flag = "V" if r.get("trigger") == "visual" else " "
        print(f'  p{r["page"]:<4}{flag} {where}  {r["score"]:.2f}  {r["cue"][:58]}')
        if "lineText" in r:
            print(f'             on: {r["lineText"][:70]}')

    weak = [r for r in out if "lineId" not in r]
    if weak:
        print(f"\nunanchored ({len(weak)}) — no script line resembles what they sit beside:")
        for r in weak[:12]:
            print(f'  p{r["page"]:<4} {r["cue"][:58]}')

    if chosen:
        how = "named on the command line" if args.cut_colour else "identified from the recording"
        print(f'\nCUTS — {"/".join(sorted(chosen))} marks, {how} '
              f"({len(cut_marks)} mark(s), {len(cut_ids)} line(s))")
        for m in cut_marks:
            got = ", ".join(lines[i]["id"] for i in m["lines"]) or "no line located"
            print(f'  p{m["page"]:<4} {got}')
            print(f'         {m["text"][:78]}')
    else:
        print("\nNo colour identified as cuts. Name one with --cut-colour, or pass")
        print("--segments and let the recording decide.")

    if args.out:
        args.out.write_text(
            json.dumps({"format": "choufleur-cues", "formatVersion": "0.1",
                        "source": args.pdf.name, "cues": out, "cutLineIds": cut_ids},
                       ensure_ascii=False, indent=1) + "\n"
        )
        print(f"\nwrote {args.out}")
        print("These are proposals. Check the anchors against the conduite before a run.")


if __name__ == "__main__":
    main()
