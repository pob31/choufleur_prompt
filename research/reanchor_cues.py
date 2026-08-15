#!/usr/bin/env python3
"""Move a cue sheet from the script it was extracted against onto the script in use.

A cue is anchored to a line *id*, and an id only means something inside one document.
The moment a script is prepped — lines merged, a struck passage removed, a speaker
label that the PDF turned into dialogue put back where it belongs — the ids stop
lining up, and every cue quietly points somewhere else.

Quietly is the problem. A cue whose anchor was deleted at least fails loudly. A cue
whose anchor still *exists*, and now holds a different line, fires in the wrong place
with nothing on screen to say so. Measured on Hécube: 3 cues dangling, 123 silently
displaced.

So the mapping is done by **alignment**, not by best-match. Matching each cue's text
against the whole new script independently is how a cue on "Pardon." ends up three
hundred lines away — the word is everywhere, and a perfect score means nothing. A
sequence alignment instead insists the order is preserved, which is the one thing that
is actually true of two versions of the same play, and lets a short ambiguous line be
placed by its neighbours rather than by itself.

Every cue lands in one of three states, and the third is the point of the tool:

    exact       the aligner matched its line; the anchor simply changes id
    shifted     its line was removed; anchored to the neighbour that follows
    review      no confident placement — printed in full for a human to decide

    python reanchor_cues.py cues.json --from script.json --to script.prepped.json
    python reanchor_cues.py cues.json --from script.json --to script.prepped.json --write
"""

from __future__ import annotations

import argparse
import json
import re
import unicodedata
from difflib import SequenceMatcher
from pathlib import Path


def normalize(text: str) -> str:
    """Fold to the form the aligner compares: letters and spaces, no case, no accents."""
    t = unicodedata.normalize("NFD", text.lower())
    t = "".join(c for c in t if unicodedata.category(c) != "Mn")
    return " ".join(re.findall(r"[a-z0-9']+", t))


def tokens(text: str) -> set[str]:
    return set(normalize(text).split())


def dice(a: set[str], b: set[str]) -> float:
    return 2 * len(a & b) / (len(a) + len(b)) if a and b else 0.0


def align(old: list[str], new: list[str]) -> dict[int, int]:
    """Map old line index → new line index, for lines the aligner paired up.

    `SequenceMatcher` on the normalized texts. Autojunk is off: it discards elements
    appearing in more than 1 % of the sequence, which in a play means every "Oui.",
    every "Un temps." — exactly the short lines that need their neighbours' help.
    """
    sm = SequenceMatcher(a=old, b=new, autojunk=False)
    out: dict[int, int] = {}
    for a0, b0, size in sm.get_matching_blocks():
        for k in range(size):
            out[a0 + k] = b0 + k
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cues", type=Path)
    ap.add_argument("--from", dest="old", type=Path, required=True, help="script the cues are keyed to")
    ap.add_argument("--to", dest="new", type=Path, required=True, help="script to move them onto")
    ap.add_argument("--write", action="store_true", help="rewrite the cue file in place")
    ap.add_argument("--review-below", type=float, default=0.55, help="text agreement under which a cue is sent for review")
    args = ap.parse_args()

    cues = json.loads(args.cues.read_text())
    old_lines = json.loads(args.old.read_text())["lines"]
    new_lines = json.loads(args.new.read_text())["lines"]

    old_by_id = {l["id"]: i for i, l in enumerate(old_lines)}
    mapping = align([normalize(l["text"]) for l in old_lines],
                    [normalize(l["text"]) for l in new_lines])

    # Where a line was removed, the cue belongs with whatever now follows it: an
    # operator's cue sits *before* the moment it fires, so the next surviving line is
    # the honest home for it, never the previous one.
    def nearest_after(i: int) -> int | None:
        for k in range(i, len(old_lines)):
            if k in mapping:
                return mapping[k]
        return mapping[max(mapping)] if mapping else None

    exact = shifted = review = unanchored = 0
    reviews: list[tuple] = []
    for cue in cues.get("cues", []):
        lid = cue.get("lineId")
        if not lid:
            unanchored += 1
            continue
        oi = old_by_id.get(lid)
        if oi is None:
            reviews.append((lid, cue["cue"], cue.get("lineText", ""), None, 0.0, "id not in the source script"))
            review += 1
            continue
        ni = mapping.get(oi)
        state = "exact"
        if ni is None:
            ni = nearest_after(oi)
            state = "shifted"
        if ni is None:
            reviews.append((lid, cue["cue"], cue.get("lineText", ""), None, 0.0, "nothing to align to"))
            review += 1
            continue
        # Judge the landing against what the *cue* recorded, not against the old
        # script's line. The two disagree on twenty of Hécube's cues — the PDF
        # extraction attributed some notes to the line above or below — and comparing
        # the old script with itself would send perfectly good placements to review
        # while saying nothing about whether the cue arrived where it belongs.
        recorded = cue.get("lineText") or old_lines[oi]["text"]
        agreement = dice(tokens(recorded), tokens(new_lines[ni]["text"]))
        if state == "shifted" and agreement < args.review_below:
            # Left on its old anchor on purpose. A flagged cue whose id no longer
            # resolves fails loudly, in the prep tool, in front of a human. A cue
            # quietly moved to the wrong line fails during the show.
            reviews.append((lid, cue["cue"], recorded, ni, agreement, "line was removed"))
            cue["needsReview"] = True
            cue["suggestedLineId"] = new_lines[ni]["id"]
            review += 1
            continue
        exact += state == "exact"
        shifted += state == "shifted"
        cue.pop("needsReview", None)
        cue.pop("suggestedLineId", None)
        cue["lineId"] = new_lines[ni]["id"]
        cue["lineText"] = new_lines[ni]["text"]

    # Cut markings travel the same way.
    if "cutLineIds" in cues:
        moved, lost = [], []
        for lid in cues["cutLineIds"]:
            oi = old_by_id.get(lid)
            ni = mapping.get(oi) if oi is not None else None
            (moved if ni is not None else lost).append(
                new_lines[ni]["id"] if ni is not None else lid
            )
        cues["cutLineIds"] = moved
        if lost:
            print(f"cutLineIds: {len(moved)} moved, {len(lost)} dropped (line gone): {lost[:8]}")

    total = exact + shifted + review
    print(f"\n{total} anchored cues, {unanchored} with no line anchor")
    print(f"  exact    {exact:>4}   the aligner paired the line; only the id changes")
    print(f"  shifted  {shifted:>4}   line removed; moved to the line that now follows")
    print(f"  review   {review:>4}   no confident placement")

    if reviews:
        print("\nThese need a human eye — each one is a cue that would otherwise fire in the wrong place:\n")
        for lid, name, recorded, ni, score, why in reviews:
            print(f"  {lid}  {name[:56]!r}  ({why})")
            print(f"      was: {recorded[:110]!r}")
            if ni is not None:
                print(f"      now: {new_lines[ni]['id']}  {new_lines[ni]['text'][:110]!r}  agreement {score:.2f}")
            print()

    if args.write:
        cues.setdefault("provenance", {})["reanchoredFrom"] = str(args.old)
        cues["provenance"]["reanchoredTo"] = str(args.new)
        args.cues.write_text(json.dumps(cues, ensure_ascii=False, indent=1) + "\n")
        print(f"written: {args.cues}")
    else:
        print("(dry run — pass --write to rewrite the cue file)")


if __name__ == "__main__":
    main()
