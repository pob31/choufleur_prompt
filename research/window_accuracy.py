#!/usr/bin/env python3
"""How often is the reported position inside the operator's window?

The display centres the current line and the operator reads a handful of lines
around it, so the number that matters is not median error in percent of show —
it is: *when I glance at the screen, is the truth inside the window I can see?*

There is no labelled ground truth yet, so this uses the recording's own
unambiguous moments as spot checks. A kept segment that matches exactly one
script line strongly — and no other line comes close — pins the show to that
line at that instant, repeats and choruses excluded by the uniqueness test
itself. At each such anchor the trace's reported position (the last update at
or before that time) either is or is not within the window.

The caveat, stated rather than hidden: anchors exist only where the ASR heard
something clean, which correlates with the tracker doing well. Moments of mush
are under-sampled, so these figures are an optimistic bound — but an honest one
for the question "when the show is *knowable*, is the display right?"

    python window_accuracy.py script.json segments.jsonl trace.jsonl
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


def tokens(text: str) -> set[str]:
    return {w for w in normalize(text).split(" ") if w}


def dice(a: set[str], b: set[str]) -> float:
    return 2 * len(a & b) / (len(a) + len(b)) if a and b else 0.0


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("script", type=Path)
    ap.add_argument("segments", type=Path)
    ap.add_argument("trace", type=Path)
    ap.add_argument("--strong", type=float, default=0.75,
                    help="similarity a segment must reach to pin a line")
    ap.add_argument("--margin", type=float, default=0.20,
                    help="how far behind the runner-up must be for the pin to be unambiguous")
    ap.add_argument("--windows", default="3,5,12",
                    help="half-widths to report, in lines")
    args = ap.parse_args()

    script = json.loads(args.script.read_text())
    lines = script["lines"]
    toks = [tokens(l["text"]) for l in lines]

    heard = [json.loads(l) for l in args.segments.read_text().splitlines() if l.strip()]
    heard = [r for r in heard if not r.get("interim") and not r.get("filtered")]

    # Anchors: segments that pin exactly one line.
    anchors = []
    for r in heard:
        want = tokens(r["text"])
        if len(want) < 4:
            continue
        scored = sorted(
            ((dice(want, t), i) for i, t in enumerate(toks) if t),
            reverse=True,
        )
        best, second = scored[0], scored[1] if len(scored) > 1 else (0.0, -1)
        if best[0] >= args.strong and best[0] - second[0] >= args.margin:
            anchors.append((r["tEnd"], best[1]))

    # Reported position over time, as a step function.
    trace = [json.loads(l) for l in args.trace.read_text().splitlines() if l.strip()]
    steps = [(r["t"], r["lineIndex"]) for r in trace if r.get("lineIndex") is not None]
    steps.sort()

    def reported_at(t: float):
        pos = None
        for tt, idx in steps:
            if tt > t:
                break
            pos = idx
        return pos

    widths = [int(w) for w in args.windows.split(",")]
    inside = {w: 0 for w in widths}
    errs, misses = [], []
    used = 0
    for t, truth in anchors:
        pos = reported_at(t)
        if pos is None:
            continue  # before the first fix
        used += 1
        err = abs(pos - truth)
        errs.append(err)
        for w in widths:
            if err <= w:
                inside[w] += 1
        if err > max(widths):
            misses.append((t, truth, pos))

    print(f"{len(anchors)} unambiguous anchor(s); {used} after the first fix\n")
    if not used:
        return
    errs.sort()
    for w in widths:
        print(f"  within ±{w:>2} lines ({2 * w}-line window): "
              f"{inside[w]}/{used}  = {100 * inside[w] / used:.0f}%")
    print(f"\n  median |error|  {errs[len(errs) // 2]} line(s)")
    print(f"  p90    |error|  {errs[int(len(errs) * 0.9)]} line(s)")
    if misses:
        print(f"\n  worst misses ({len(misses)} beyond ±{max(widths)}):")
        for t, truth, pos in misses[:8]:
            print(f"    {t:7.1f}s  truth L{truth + 1:04d}  shown L{pos + 1:04d}  "
                  f"off by {abs(pos - truth)}")


if __name__ == "__main__":
    main()
