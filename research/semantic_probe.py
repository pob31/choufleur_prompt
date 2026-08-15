#!/usr/bin/env python3
"""Would weighting words by how rare they are help with paraphrase?

The matcher counts every token equally, so `le`, `de`, `est`, `pas` carry the same
weight as `Polymestor` or `chienne`. That is backwards for the job: function words
survive paraphrase and carry almost no information about *where* we are, while the
content words are both the first thing a paraphrase keeps and the thing that
identifies a line uniquely.

Weighting by inverse document frequency across the show costs nothing — the weights
are computed once from the script itself — and needs no model, no download and no
extra millisecond in the hot path. So it is worth knowing whether it closes the gap
before reaching for sentence embeddings, which would mean an ONNX model, ~30 ms per
segment, and a per-language quality question the corpus cannot yet answer.

Measured on real material: every script line, against the passage in the recording
that best explains it. The interesting band is the middle — lines the company clearly
said in their own words rather than the playwright's.

    python semantic_probe.py script.json segments.jsonl
"""

from __future__ import annotations

import argparse
import json
import math
import unicodedata
from collections import Counter
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


def toks(text: str) -> list[str]:
    return [w for w in normalize(text).split(" ") if w]


def dice(a: set[str], b: set[str]) -> float:
    return 2 * len(a & b) / (len(a) + len(b)) if a and b else 0.0


def weighted_dice(a: set[str], b: set[str], w: dict[str, float]) -> float:
    """Dice with each token counted by its weight rather than as 1."""
    inter = sum(w.get(t, 1.0) for t in a & b)
    total = sum(w.get(t, 1.0) for t in a) + sum(w.get(t, 1.0) for t in b)
    return 2 * inter / total if total else 0.0


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("script", type=Path)
    ap.add_argument("segments", type=Path)
    ap.add_argument("--span", type=int, default=3)
    args = ap.parse_args()

    lines = json.loads(args.script.read_text())["lines"]
    line_toks = [set(toks(l["text"])) for l in lines]

    # IDF over the script's own lines: a word in every line tells you nothing about
    # which line you are on, and a word in one line tells you everything.
    df = Counter()
    for t in line_toks:
        df.update(t)
    n = len(lines)
    weight = {w: math.log(1 + n / (1 + c)) for w, c in df.items()}

    recs = [json.loads(l) for l in args.segments.read_text().splitlines() if l.strip()]
    heard = [r for r in recs if not r.get("interim") and not r.get("filtered")]
    ht = [set(toks(r["text"])) for r in heard]
    passages: list[set[str]] = []
    for i in range(len(ht)):
        merged: set[str] = set()
        for k in range(args.span):
            if i + k >= len(ht):
                break
            merged = merged | ht[i + k]
            passages.append(set(merged))

    plain_scores, weighted_scores = [], []
    improved = worsened = 0
    for want in line_toks:
        if len(want) < 4:
            continue
        best_p = max((dice(want, p) for p in passages), default=0.0)
        best_w = max((weighted_dice(want, p, weight) for p in passages), default=0.0)
        plain_scores.append(best_p)
        weighted_scores.append(best_w)
        if best_w > best_p + 0.02:
            improved += 1
        elif best_p > best_w + 0.02:
            worsened += 1

    def band(scores, lo, hi):
        return sum(1 for s in scores if lo <= s < hi)

    print(f"{len(plain_scores)} lines scored against the recording\n")
    print(f"{'band':<14}{'equal weights':>15}{'IDF weighted':>15}")
    for lo, hi, name in [
        (0.0, 0.30, "not found"),
        (0.30, 0.62, "paraphrased"),
        (0.62, 0.85, "recognisable"),
        (0.85, 1.01, "as written"),
    ]:
        print(f"{name:<14}{band(plain_scores, lo, hi):>15}{band(weighted_scores, lo, hi):>15}")

    mp = sum(plain_scores) / len(plain_scores)
    mw = sum(weighted_scores) / len(weighted_scores)
    print(f"\n  mean score        {mp:.3f} -> {mw:.3f}  ({mw - mp:+.3f})")
    print(f"  lines improved    {improved}")
    print(f"  lines worsened    {worsened}")
    print(
        "\n  What matters is the paraphrased band crossing 0.62. Weighting cannot invent\n"
        "  agreement — it can only stop function words from diluting the agreement that\n"
        "  is already there."
    )


if __name__ == "__main__":
    main()
