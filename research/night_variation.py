#!/usr/bin/env python3
"""Is this production's text stable enough to be worth learning?

Learning a show's real wording from its own run logs only pays if the wording is
*consistent*. If the actors differ from each other night to night by as much as they
differ from the script, there is nothing stable to learn and no amount of data will
help — the right answer then is to track loosely and accept it, not to keep tuning.

This measures both quantities on the same scale, per script line:

  fidelity     how close a night's delivery is to the written line
  consistency  how close two nights' deliveries are to *each other*

The interesting number is the gap. Consistency well above fidelity means the company
has settled on a wording that simply is not the one in the script — precisely the
case learned alternates exist for, and the case where they will work. Consistency
near fidelity means the delivery is improvised afresh each night, and no stored
variant will match tomorrow.

    python night_variation.py script.json night1.jsonl night2.jsonl [night3.jsonl ...]
"""

from __future__ import annotations

import argparse
import itertools
import json
import unicodedata
from pathlib import Path


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


def passages(path: Path, span: int) -> list[tuple[set[str], str]]:
    """Every run of up to `span` consecutive kept segments, as a bag of tokens."""
    records = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
    heard = [r for r in records if not r.get("interim") and not r.get("filtered")]
    toks = [tokens(r["text"]) for r in heard]
    out = []
    for i in range(len(heard)):
        merged, texts = set(), []
        for k in range(span):
            if i + k >= len(heard):
                break
            merged = merged | toks[i + k]
            texts.append(heard[i + k]["text"])
            out.append((set(merged), " ".join(texts)))
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("script", type=Path)
    ap.add_argument("nights", type=Path, nargs="+", help="one segments.jsonl per night")
    ap.add_argument("--span", type=int, default=3)
    ap.add_argument("--min", type=float, default=0.30, help="below this, the line was not found at all")
    args = ap.parse_args()

    if len(args.nights) < 2:
        ap.error("at least two nights are needed to measure consistency")

    script = json.loads(args.script.read_text())
    lines = [l for l in script["lines"] if len(tokens(l["text"])) >= 3]
    per_night = [passages(p, args.span) for p in args.nights]

    # For each line, the passage each night best explains it with.
    best: list[list[tuple[float, set[str]]]] = []
    for line in lines:
        want = tokens(line["text"])
        row = []
        for ps in per_night:
            sc, got = max(((dice(want, g), g) for g, _ in ps), key=lambda x: x[0], default=(0.0, set()))
            row.append((sc, got))
        best.append(row)

    names = [p.stem for p in args.nights]
    print(f"{len(lines)} script lines, {len(args.nights)} nights\n")

    print("fidelity — how close each night is to the written script")
    for i, name in enumerate(names):
        vals = [b[i][0] for b in best]
        found = sum(1 for v in vals if v >= args.min)
        print(f"    {name:28} mean {sum(vals) / len(vals):.2f}   {found}/{len(vals)} lines found")

    print("\nconsistency — how close two nights are to each other")
    pairs = []
    for i, j in itertools.combinations(range(len(names)), 2):
        vals = [
            dice(b[i][1], b[j][1])
            for b in best
            if b[i][0] >= args.min and b[j][0] >= args.min
        ]
        if not vals:
            continue
        m = sum(vals) / len(vals)
        pairs.append(m)
        print(f"    {names[i]:24} vs {names[j]:24} {m:.2f}  (over {len(vals)} lines)")

    fid = sum(b[i][0] for b in best for i in range(len(names))) / (len(best) * len(names))
    con = sum(pairs) / len(pairs) if pairs else 0.0
    print(f"\n    mean fidelity to script  {fid:.2f}")
    print(f"    mean consistency between nights  {con:.2f}")
    gap = con - fid
    print(f"    gap  {gap:+.2f}")
    if gap > 0.15:
        print(
            "\n    The company is far more consistent with itself than with the script:\n"
            "    they have settled on a wording that is not the written one. That is\n"
            "    exactly what learned alternates are for, and they should work here."
        )
    elif gap > 0.05:
        print(
            "\n    Somewhat more consistent with itself than with the script. Learned\n"
            "    alternates should help, but expect the gain to be modest and to need\n"
            "    several nights of agreement before a variant is trustworthy."
        )
    else:
        print(
            "\n    The delivery varies between nights about as much as it varies from\n"
            "    the script: it is being improvised afresh, not settled. No stored\n"
            "    variant will match tomorrow, so track loosely and do not tune for it."
        )


if __name__ == "__main__":
    main()
