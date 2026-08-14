#!/usr/bin/env python3
"""How much of the script was actually said? A ceiling, before blaming the tracker.

When coverage comes out low there are three possible culprits and they need
different fixes:

1. **The recogniser** mis-heard — fix the model, the language, the levels.
2. **The script** does not match the performance — the actor paraphrased, the draft
   is stale, or whoever typed it made mistakes. Nothing downstream can fix this.
3. **The tracker** heard it, it was there, and it still failed to follow.

Only the third is an engine problem, and it is the rarest. This tool separates
them by asking a question the tracker never asks: for each script line, is there
*anywhere* in the transcript a passage that resembles it? That is the ceiling. A
tracker scoring near it is doing its job; the remaining loss is material, not code.

    python script_vs_audio.py script.json segments.jsonl
    python script_vs_audio.py script.json segments.jsonl --threshold 0.55 --quiet

Run it before tuning anything. Tuning a matcher against lines that were never
spoken is how thresholds get quietly wrecked.
"""

from __future__ import annotations

import argparse
import json
import unicodedata
from pathlib import Path


def normalize(text: str) -> str:
    """Notation spec §3.2, mirroring `choufleur-core::normalize`."""
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
    """Token-set overlap, the same term the tracker weights its score by."""
    return 2 * len(a & b) / (len(a) + len(b)) if a and b else 0.0


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("script", type=Path)
    ap.add_argument("segments", type=Path, help="segments.jsonl from `transcribe`")
    ap.add_argument("--threshold", type=float, default=0.55, help="what counts as findable")
    ap.add_argument("--span", type=int, default=2, help="consecutive segments a line may span")
    ap.add_argument("--quiet", action="store_true", help="summary only")
    args = ap.parse_args()

    script = json.loads(args.script.read_text())
    records = [json.loads(l) for l in args.segments.read_text().splitlines() if l.strip()]
    # Interim hypotheses are prefixes of segments already present; counting them
    # would inflate the ceiling with duplicates of what was already there.
    heard = [r for r in records if not r.get("interim") and not r.get("filtered")]
    spoken = [(r, set(tokens(r["text"]))) for r in heard]

    findable = 0
    per_lang: dict[str, list[int]] = {}
    for line in script["lines"]:
        want = set(tokens(line["text"]))
        lang = (line.get("lang") or script.get("defaultLang", ["?"]))[0]
        best, who = 0.0, ""
        for i, (rec, got) in enumerate(spoken):
            # A line may have been split across consecutive segments by a pause.
            merged = set(got)
            for k in range(1, args.span):
                if i + k < len(spoken):
                    merged |= spoken[i + k][1]
                    score = dice(want, merged)
                    if score > best:
                        best, who = score, rec["text"]
            score = dice(want, got)
            if score > best:
                best, who = score, rec["text"]

        ok = best >= args.threshold
        findable += ok
        per_lang.setdefault(lang, [0, 0])
        per_lang[lang][0] += ok
        per_lang[lang][1] += 1
        if not args.quiet:
            mark = "OK" if ok else ("~ " if best >= args.threshold * 0.6 else "XX")
            print(f'  {mark} {line["id"]} [{lang}] {best:.2f}  {line["text"][:56]!r}')
            if not ok:
                print(f"            heard: {who[:64]!r}")

    total = len(script["lines"])
    print(f"\n{findable}/{total} script lines are findable in the audio "
          f"({100 * findable / max(total, 1):.0f}%) at threshold {args.threshold}")
    for lang, (ok, n) in sorted(per_lang.items()):
        print(f"    {lang}: {ok}/{n} ({100 * ok / n:.0f}%)")
    print(
        "\nThis is the ceiling. A tracker scoring near it is working; the rest is\n"
        "the script and the performance disagreeing, which no threshold can fix."
    )


if __name__ == "__main__":
    main()
