#!/usr/bin/env python3
"""Where are we, and how sure are we? — position as a distribution, not a match.

The tracker decides. Each segment is scored against the candidate lines, and if the
best score clears a threshold the position moves, otherwise the segment is thrown
away. On a close mic that works. On the *Lazzi* ambient capture it threw away 1984
segments out of 2199 and reached 6 % of the script, because no single segment ever
looked good enough to act on.

But the operator does not need the line. They need the page, they need to know when to
stop trusting it, and they need prodding when their attention has drifted. All three
are answered by a *distribution* over position, and a distribution can be maintained
from evidence far too weak to decide on:

    p(line | everything heard so far)

which is a forward filter. Two inputs, both cheap:

**Observation.** How much each script line looks like what was just heard. Scored on
character trigrams rather than words, because a recogniser that mishears "ruine" as
"romine" has still delivered five of the six trigrams — the word is wrong and the
evidence is not. Word matching scores that zero and is why the hard tracker starves.

**Motion.** A show advances through its script at a roughly knowable rate, so the
prior for the next moment is the current belief pushed forward by however many lines
the elapsed seconds should have bought, smeared to admit that nobody is exact. A small
uniform leak keeps a jump — a skipped scene, a restart — from being impossible.

Neither observation alone would move anything. Twenty of them agreeing move a lot,
and that is the whole point: this converts a stream of unusable transcripts into a
usable position, and reports honestly when it cannot.

    python position_filter.py script.json segments.jsonl --duration 6822
"""

from __future__ import annotations

import argparse
import json
import math
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


def trigrams(text: str) -> set[str]:
    """Character trigrams, which survive a misheard word where tokens do not."""
    t = f" {normalize(text)} "
    return {t[i:i + 3] for i in range(len(t) - 2)}


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("script", type=Path)
    ap.add_argument("segments", type=Path)
    ap.add_argument("--duration", type=float, required=True, help="length of the recording, s")
    ap.add_argument("--beta", type=float, default=14.0,
                    help="how sharply similarity becomes belief. Low trusts nothing, "
                         "high acts on one lucky match")
    ap.add_argument("--spread", type=float, default=6.0,
                    help="lines of uncertainty added per step, for a company that "
                         "does not keep to the clock")
    ap.add_argument("--leak", type=float, default=0.02,
                    help="probability mass left everywhere, so a jump stays possible")
    ap.add_argument("--band", type=int, default=12,
                    help="half-width of the band reported as the answer")
    args = ap.parse_args()

    script = json.loads(args.script.read_text())
    lines = script["lines"]
    n = len(lines)
    line_grams = [trigrams(l["text"]) for l in lines]

    records = [json.loads(l) for l in args.segments.read_text().splitlines() if l.strip()]
    heard = [r for r in records if not r.get("interim") and not r.get("filtered")]

    # Uniform to begin with: the show has not started and nothing is known.
    belief = [1.0 / n] * n
    rate = n / args.duration  # lines per second, the show's own pace
    last_t = 0.0
    trace = []

    for r in heard:
        want = trigrams(r["text"])
        if len(want) < 6:
            continue

        # --- motion: push the belief forward by what the clock bought -------------
        dt = max(0.0, r["tStart"] - last_t)
        last_t = r["tStart"]
        shift = rate * dt
        sigma = max(1.0, args.spread * math.sqrt(max(dt, 1.0)))
        # A short gaussian kernel around the expected advance.
        half = int(min(3 * sigma, 60))
        kernel = [math.exp(-((k - shift) ** 2) / (2 * sigma * sigma))
                  for k in range(-half, half + 1)]
        ksum = sum(kernel) or 1.0
        moved = [0.0] * n
        for i, p in enumerate(belief):
            if p < 1e-9:
                continue
            for k, w in enumerate(kernel):
                j = i + k - half
                if 0 <= j < n:
                    moved[j] += p * w / ksum
        total = sum(moved) or 1.0
        belief = [p / total * (1 - args.leak) + args.leak / n for p in moved]

        # --- observation: how much each line looks like what was heard ------------
        post, total = [0.0] * n, 0.0
        for i, grams in enumerate(line_grams):
            if not grams:
                continue
            sim = 2 * len(want & grams) / (len(want) + len(grams))
            p = belief[i] * math.exp(args.beta * sim)
            post[i] = p
            total += p
        if total <= 0:
            continue
        belief = [p / total for p in post]

        best = max(range(n), key=lambda i: belief[i])
        lo, hi = max(0, best - args.band), min(n - 1, best + args.band)
        mass = sum(belief[lo:hi + 1])
        trace.append((r["tStart"], best, mass))

    print(f"{len(trace)} update(s) over {n} lines\n")
    print("  time      MAP line     belief within ±%d      true-ish position" % args.band)
    step = max(1, len(trace) // 24)
    for t, best, mass in trace[::step]:
        expected = int(rate * t)
        flag = "ok" if abs(best - expected) <= 40 else ""
        print(f"  {t:7.1f}s  L{best + 1:04d}/{n}   {mass * 100:5.1f}%          "
              f"~L{expected + 1:04d}  {flag}")

    # How often is the belief both confident and roughly right? Position is compared
    # against the show's own average pace, which is not ground truth but is honest
    # about being a proxy — a real onset labelling would replace it.
    conf = [m for _, _, m in trace]
    conf.sort()
    near = sum(1 for t, b, _ in trace if abs(b - int(rate * t)) <= 40)
    sure = sum(1 for _, _, m in trace if m >= 0.9)
    both = sum(1 for t, b, m in trace if m >= 0.9 and abs(b - int(rate * t)) <= 40)
    print(f"\n  median belief in the band   {conf[len(conf) // 2] * 100:.1f}%")
    print(f"  within 40 lines of pace     {near}/{len(trace)} ({100 * near / max(1, len(trace)):.0f}%)")
    print(f"  at least 90% sure           {sure}/{len(trace)} ({100 * sure / max(1, len(trace)):.0f}%)")
    print(f"  sure AND near               {both}/{max(1, sure)} of the confident ones")


if __name__ == "__main__":
    main()
