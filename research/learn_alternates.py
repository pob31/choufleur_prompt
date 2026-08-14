#!/usr/bin/env python3
"""Propose how a script's lines were actually performed, from a run's transcript.

A production's text keeps moving after the premiere; the script usually does not.
Measured on real touring material that drift was the largest single source of
tracking loss — larger than recognition, accents or far-field capture — and it is
indistinguishable from the engine failing unless somebody looks.

This looks. It aligns a run's transcript against the script *offline*, where the
whole recording is available in both directions with no forward-only constraint and
no latency budget, and proposes an **alternate form** for each line whose delivery
has clearly diverged from its text.

    python learn_alternates.py script.json segments.jsonl -o script.learned.json

Two rules, both load-bearing.

**Learn from the recording, never from the tracker's own trace.** Learning from the
live tracker's confident matches is confirmation bias: it would reinforce whatever
the tracker already believed and drift the script toward its own mistakes. Offline
alignment is a genuinely better observer, and that is what makes this safe.

**Propose, never overwrite.** `text` is what the playwright wrote and what every
client displays (notation §2, principle 1). An alternate is an *additional* thing to
match against. Nothing is deleted, nothing is rewritten, and the operator confirms
in prep — principle 5, no silent data loss.
"""

from __future__ import annotations

import argparse
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


def tokens(text: str) -> list[str]:
    return [w for w in normalize(text).split(" ") if w]


def dice(a: set[str], b: set[str]) -> float:
    return 2 * len(a & b) / (len(a) + len(b)) if a and b else 0.0


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("script", type=Path)
    ap.add_argument(
        "segments",
        type=Path,
        nargs="+",
        help="one segments.jsonl per run. Several runs is the point: see --min-runs",
    )
    ap.add_argument("-o", "--out", type=Path)
    ap.add_argument(
        "--min",
        type=float,
        default=0.30,
        help="below this the passage is a different line, not a reworded one",
    )
    ap.add_argument(
        "--max",
        type=float,
        default=0.85,
        help="above this the line was performed as written; nothing to learn",
    )
    ap.add_argument("--span", type=int, default=3, help="segments a line may span")
    ap.add_argument(
        "--min-runs",
        type=int,
        default=1,
        help="how many runs must agree on a variant before it is proposed. One run "
        "cannot tell a real text change from an ASR slip or a one-off improvisation; "
        "two or three can. This is what makes learning night after night worth more "
        "than learning once",
    )
    ap.add_argument("--quiet", action="store_true")
    args = ap.parse_args()

    script = json.loads(args.script.read_text())
    lines = script["lines"]
    line_tokens = [set(tokens(l["text"])) for l in lines]

    # variant text -> how many runs proposed it, for that line
    votes: list[dict[str, int]] = [dict() for _ in lines]
    already = 0

    for path in args.segments:
        records = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
        heard = [r for r in records if not r.get("interim") and not r.get("filtered")]
        spoken = [(r, set(tokens(r["text"]))) for r in heard]

        # Every candidate passage: a run of up to --span consecutive segments.
        passages = []
        for i in range(len(spoken)):
            merged, texts = set(), []
            for k in range(args.span):
                if i + k >= len(spoken):
                    break
                merged = merged | spoken[i + k][1]
                texts.append(spoken[i + k][0]["text"])
                passages.append((set(merged), " ".join(texts)))

        # Score everything once, then keep only MUTUAL best matches.
        #
        # Without this the learner steals its neighbours' text. On real material a
        # line was proposed the alternate "Ik ben het toneel opgelopen. Opkomen." —
        # and the first half of that is the *previous line*, so the alternate made
        # that line match passages that were never it, and tracking got worse.
        # A proposal is only trustworthy when the line's best passage also has this
        # line as *its* best line: they have to choose each other.
        best_for_line = [(0.0, "", -1)] * len(lines)
        best_for_passage = [(0.0, -1)] * len(passages)
        for li, want in enumerate(line_tokens):
            if len(want) < 3:
                continue
            for pi, (got, text) in enumerate(passages):
                sc = dice(want, got)
                if sc > best_for_line[li][0]:
                    best_for_line[li] = (sc, text, pi)
                if sc > best_for_passage[pi][0]:
                    best_for_passage[pi] = (sc, li)

        for li, (sc, text, pi) in enumerate(best_for_line):
            if pi < 0:
                continue
            if sc >= args.max:
                already += 1
                continue
            if sc < args.min:
                # Nothing resembles this line: cut, or not performed in this run.
                # An orphan question for the operator, not something to learn.
                continue
            if best_for_passage[pi][1] != li:
                # Some other line explains this passage better. Learning it here
                # would be stealing that line's text.
                continue
            if normalize(text) == normalize(lines[li]["text"]):
                continue
            votes[li][text] = votes[li].get(text, 0) + 1

    proposed = 0
    for li, line in enumerate(lines):
        for text, n in sorted(votes[li].items(), key=lambda kv: -kv[1]):
            if n < args.min_runs:
                continue
            alts = line.setdefault("alternates", [])
            if text in alts:
                continue
            alts.append(text)
            proposed += 1
            if not args.quiet:
                seen = f"{n} run(s)" if len(args.segments) > 1 else ""
                print(f'  {line["id"]} {seen}')
                print(f'    written:   {line["text"][:78]}')
                print(f'    performed: {text[:78]}')

    total = len(lines)
    print(f"\n{proposed} alternate(s) proposed over {total} line(s); "
          f"{already} already performed as written")
    print(
        "\nThese are PROPOSALS. `text` is untouched and is still what the client\n"
        "displays; an alternate only gives the matcher another thing to recognise.\n"
        "Review them before a run — a wrong alternate is a false match every night."
    )
    if len(args.segments) == 1:
        print(
            "\nOnly one run was given, so --min-runs cannot filter anything. A single\n"
            "night cannot distinguish a real change in the text from an ASR slip or a\n"
            "one-off improvisation. Collect two or three and raise --min-runs: that is\n"
            "where learning night after night earns its keep."
        )
    if args.out:
        args.out.write_text(json.dumps(script, ensure_ascii=False, indent=1) + "\n")
        print(f"wrote {args.out}")


if __name__ == "__main__":
    main()
