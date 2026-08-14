#!/usr/bin/env python3
"""Read an operator's marked-up conduite PDF and attach its cues to script lines.

The devplan assumes cues get typed in. They usually do not have to be. An operator
who has run a show already owns a conduite — the script as a PDF, with the cues
written in the margin and the pages marked up — and that document holds
the two things Choufleur needs: *what happens* and *when*. Typing all of it again is
a few hours of work with a fresh opportunity for error on every line.

    python conduite_to_cues.py conduite.pdf script.json -o cues.json

The margin notes come out as FreeText annotations and the highlights as Highlight
annotations, both carrying a position on the page. Anchoring is therefore geometric
first — a cue belongs to whatever it was written next to — and textual second: the
words near it are matched against the script to find the line.

**There is no standard colour scheme, and this tool ships no default.** *Some* marks
are trigger points — on a word or a phrase, or standing in for a visual cue. Some flag
a passage that must be ridden because it is about to get loud. Some warn that
something is coming without being a hard go. Some mark cuts. Which colour carries
which meaning is one operator's habit on one show, and the same person may mark the
next one differently.

So the palette is surveyed and printed with what each colour covers, and the meanings
come from `--colour-means` — as pairs, or as a path to a small JSON the show keeps,
so the convention travels with the production instead of living in somebody's memory.
The map is written into the output for the next run to inherit. One conduite's scheme
is recorded below as an illustration of the shape, never as a fallback:

    gold = trigger, pale yellow = warning, orange = loud, grey = cut

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
import colorsys
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


# A note's own grammar, which beats guessing at its vocabulary. Notes are written
# "<what I wait for> > <what I then do>", and the arrow also chains steps *within* an
# action — "2 Mute micros / lumière 11 / Musique > Ouverture micros" is one numbered
# cue in sequence, not a cue waiting on something called "Musique".
#
# The cue number separates the two senses. A note that opens with one is already the
# action, so every arrow in it is sequencing; a note whose leading clause carries no
# number is waiting on something, and that something is a thing the operator watches
# for — an entrance, a prop set down — because if the trigger were the text there
# would be nothing to write.
#
# This matters beyond bookkeeping: on a visual trigger Choufleur can only ever warn
# early, since it cannot see the cloth fall. Detecting them from the punctuation
# rather than from a list of French stage words also survives the next show, and the
# next language.
NUMBERED = re.compile(r"^\s*\d+(\.\d+)*\s")


def split_note(text: str) -> tuple[str | None, str]:
    """`(what is waited for, what is then done)` — the first is None on a text cue."""
    head, sep, tail = text.partition(">")
    if not sep or NUMBERED.match(text) or not head.strip() or not tail.strip():
        return None, text.strip()
    return head.strip(), tail.strip()


def colour_name(colour) -> str:
    """A stable, human label for a mark's colour, so it can be named on the command
    line without anybody typing RGB triples.

    Saturation matters as much as hue, and getting that wrong loses distinctions the
    operator is relying on. On the Hécube conduite gold (h=49, s=1.00) means a hard
    trigger and pale yellow (h=56, s=0.47) means a warning of something coming — two
    different instructions, sixteen degrees apart. Naming by hue alone called both
    "yellow" and merged 42 marks with 41 into one meaningless heap of 83.
    """
    if not colour or len(colour) < 3:
        return "none"
    h, s, v = colorsys.rgb_to_hsv(*colour[:3])
    if s < 0.12:
        return "white" if v > 0.95 else "black" if v < 0.2 else "grey"
    deg = h * 360
    base = (
        "red" if deg < 15 else "orange" if deg < 40 else "gold" if deg < 52
        else "yellow" if deg < 70 else "green" if deg < 170 else "cyan" if deg < 200
        else "blue" if deg < 250 else "purple" if deg < 330 else "pink" if deg < 345
        else "red"
    )
    return f"pale {base}" if s < 0.6 else base


def extract(pdf: Path):
    """Every cue note and every highlight, with the page text around it."""
    doc = fitz.open(pdf)
    cues, marks, tinted = [], [], []
    for pno in range(len(doc)):
        page = doc[pno]
        words = page.get_text("words")  # (x0, y0, x1, y1, word, block, line, wno)
        for a in page.annots() or []:
            r = a.rect
            kind = a.type[1]
            if kind == "FreeText":
                content = " ".join((a.info.get("content") or "").split())
                if content:
                    cues.append({"page": pno + 1, "y": r.y0, "x": r.x0,
                                 "text": content, "colour": None, "rgb": None})
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
    # Cue notes written as coloured text rather than as annotations, for the same
    # flattening reason. Their colour is the operator's coding for which system acts
    # — on this conduite blue for a QLab go and purple for a move on the desk — which
    # the annotation layer does not carry at all.
    for pno in range(len(doc)):
        # Merged by colour and baseline, because a run of coloured text arrives as
        # several spans — a bold word, a changed font — and each one is a fragment of
        # one note, not a note of its own.
        runs: dict[tuple, list] = {}
        for block in doc[pno].get_text("dict")["blocks"]:
            for line in block.get("lines", []):
                for span in line.get("spans", []):
                    rgb = span.get("color", 0)
                    if rgb in (0, 0xFFFFFF):
                        continue
                    text = span.get("text", "")
                    if not text.strip():
                        continue
                    c = ((rgb >> 16 & 255) / 255, (rgb >> 8 & 255) / 255, (rgb & 255) / 255)
                    key = (colour_name(c), round(span["bbox"][1] / 4))
                    runs.setdefault(key, []).append((span["bbox"][0], text, c))
        for (name, row), parts in sorted(runs.items(), key=lambda kv: kv[0][1]):
            parts.sort()
            text = " ".join("".join(t for _, t, _ in parts).split())
            if len(text) < 2:
                continue
            tinted.append({"page": pno + 1, "y": row * 4.0,
                           "x": parts[0][0], "text": text, "colour": name,
                           "rgb": tuple(round(x, 2) for x in parts[0][2])})

    # The two layers describe the same markup, not two sets of notes. A conduite
    # edited on a tablet keeps its live annotations *and* a flattened rendering of
    # them, so every note appears twice — 83 of 131 byte-identical here. Only the
    # flattened copy carries the text colour, which is where the operator's coding for
    # which system acts (QLab, or the desk) actually lives. So the annotation supplies
    # the note, its overlapping tinted twin supplies the colour, and only tinted text
    # with no annotation over it counts as a note in its own right.
    for t in tinted:
        twin = None
        for c in cues:
            if c["page"] == t["page"] and abs(c["y"] - t["y"]) <= 14 and c["colour"] is None:
                if normalize(t["text"]) in normalize(c["text"]) or abs(c["y"] - t["y"]) <= 6:
                    twin = c
                    break
        if twin:
            twin["colour"] = twin["colour"] or t["colour"]
            twin["rgb"] = twin["rgb"] or t["rgb"]
        else:
            cues.append(t)

    # Marks that are not annotations. A conduite gets flattened — re-exported, or
    # edited on a tablet — and its highlighting then lives in the page content as
    # filled rectangles that no annotation API will return. On the Hécube file the
    # live annotations held 17 grey marks and the page content held 82, so reading
    # only the annotation layer missed most of the cuts.
    for pno in range(len(doc)):
        page = doc[pno]
        words = page.get_text("words")
        known = [a.rect for a in (page.annots() or [])]
        for drawing in page.get_drawings():
            fill, rect = drawing.get("fill"), drawing.get("rect")
            if not fill or rect is None or rect.height > 40 or rect.height < 2:
                continue
            if rect.width < 4:
                continue
            name = colour_name(fill)
            if name in ("black", "white"):
                continue
            if any(abs(k.y0 - rect.y0) < 3 and abs(k.x0 - rect.x0) < 3 for k in known):
                continue  # the same mark, already taken from the annotation layer
            covered = " ".join(w[4] for w in words if fitz.Rect(w[:4]).intersects(rect))
            if covered.strip():
                marks.append(
                    {"page": pno + 1, "y": rect.y0, "text": " ".join(covered.split()),
                     "kind": "Shading", "colour": name,
                     "rgb": tuple(round(x, 2) for x in fill[:3])}
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
    ap.add_argument("--colour-means", nargs="*", default=[], metavar="NAME=MEANING",
                    help="what THIS operator's colours mean on THIS show — there is no "
                         "default and no standard. Either a path to a .json holding a "
                         "colourMeans object, or pairs: gold=trigger 'pale yellow=warning' "
                         "orange=loud grey=cut. A colour meaning 'cut' is treated as one, "
                         "and the map is written into the output to be reused")
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

    # The convention belongs to the show, not to this tool and not to a command line
    # somebody has to remember. Given a .json path it is read from there; given
    # name=meaning pairs it is taken from those, and either way it is written into the
    # output so the next run of this show inherits it.
    means: dict[str, str] = {}
    for spec in args.colour_means:
        if spec.endswith(".json"):
            f = Path(spec)
            if f.exists():
                loaded = json.loads(f.read_text())
                means.update(loaded.get("colourMeans", loaded))
            else:
                print(f"  (no colour map at {spec} — carrying on without one)")
            continue
        name, _, meaning = spec.partition("=")
        if meaning:
            means[name.strip()] = meaning.strip()
    unknown = [n for n in means if n not in palette]
    if unknown:
        print(f'  (no marks are {", ".join(unknown)} — check the names against the palette above)')

    chosen = {n for n, m in means.items() if m.lower() == "cut"} | set(args.cut_colour or [])
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
            mark_colour = near[0]["colour"]
        else:
            rows = [t for y, t in body.get(cue["page"], [])
                    if -args.band <= y - cue["y"] <= 12.0]
            evidence, source = " ".join(rows[-3:]), "page"
            mark_colour = None

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

        watch, action = split_note(cue["text"])
        rec = {
            "cue": cue["text"],
            "action": action,
            "page": cue["page"],
            "evidence": evidence[:120],
            "evidenceFrom": source,
            "noteColour": cue.get("colour"),
            "noteMeans": means.get(cue.get("colour") or ""),
            "markColour": mark_colour,
            "markMeans": means.get(mark_colour or ""),
            "score": round(best, 3),
            "trigger": "visual" if watch else "text",
        }
        if watch:
            # The line it is anchored to is where the operator starts watching, not
            # where the cue goes. Choufleur can bring it up in good time and no more.
            rec["waitsFor"] = watch
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
                        "source": args.pdf.name, "colourMeans": means,
                        "cues": out, "cutLineIds": cut_ids},
                       ensure_ascii=False, indent=1) + "\n"
        )
        print(f"\nwrote {args.out}")
        print("These are proposals. Check the anchors against the conduite before a run.")


if __name__ == "__main__":
    main()
