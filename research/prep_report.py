#!/usr/bin/env python3
"""What the show says about the script: a prep review built from a run.

Entering a script is the least interesting work in the theatre and the easiest to
get wrong. An assistant types it under time pressure, in languages they may not
speak, from a copy whose cuts are crossed out in pencil — and every mistake surfaces
weeks later as the tracker apparently failing.

Almost none of it has to be done by hand. A recording of the show already contains
the answers, and this asks it three questions:

  cuts        which lines does the audio never contain? Probably cut.
  languages   which lines were tagged in a language nobody spoke them in?
  landmarks   which lines are distinctive enough to relocate the show from?

The third matters more than it looks. Re-anchoring after a loss depends on
landmarks, and a script imported from a document has none at all — which is why
locating a show unaided currently takes a minute or more. A handful of good ones,
proposed here and confirmed by a human, does that job better than any amount of
matcher tuning.

    python prep_report.py script.json run1.jsonl [run2.jsonl ...] -o script.prepped.json

Every output is a **proposal**. Nothing is deleted and no text is rewritten: cuts are
marked, not removed, so a line restored next week is still there (notation §2,
principle 5). Review before use — a wrong landmark is worse than no landmark.
"""

from __future__ import annotations

import argparse
import json
import unicodedata
from pathlib import Path

LANG_MARKERS = {
    "nl": set("de het een ik je niet dat en van is zijn maar ze wij hij ook nog wat heb "
              "heeft was werd om te op met voor door naar als dan want er dit die deze".split()),
    "fr": set("le la les un une des et de du au aux je tu il elle nous vous ils elles que "
              "qui pas ne est sont était avec pour dans sur par plus mais donc alors".split()),
    "en": set("the a an and of to in is are was were i you he she we they that this these "
              "with for on by not but so as at from have has had will would can could".split()),
}


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


def load_run(path: Path, span: int):
    records = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
    heard = [r for r in records if not r.get("interim") and not r.get("filtered")]
    toks = [set(tokens(r["text"])) for r in heard]
    passages = []
    for i in range(len(heard)):
        merged = set()
        for k in range(span):
            if i + k >= len(heard):
                break
            merged |= toks[i + k]
            passages.append((set(merged), heard[i + k]))
    return heard, passages


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("script", type=Path)
    ap.add_argument("runs", type=Path, nargs="+", help="one segments.jsonl per run")
    ap.add_argument("-o", "--out", type=Path, help="write a script with the proposals applied")
    ap.add_argument("--heard", type=float, default=0.35, help="score at which a line counts as heard")
    ap.add_argument("--landmark", type=float, default=0.75, help="score a landmark candidate must reach")
    ap.add_argument("--span", type=int, default=3)
    args = ap.parse_args()

    script = json.loads(args.script.read_text())
    lines = script["lines"]
    runs = [load_run(p, args.span) for p in args.runs]
    n_runs = len(runs)

    # Per line: best score in each run, and what was heard there.
    best: list[list[tuple[float, dict | None]]] = []
    for line in lines:
        want = set(tokens(line["text"]))
        row = []
        for _, passages in runs:
            if not want:
                row.append((0.0, None))
                continue
            sc, rec = max(((dice(want, p), r) for p, r in passages), key=lambda x: x[0], default=(0.0, None))
            row.append((sc, rec))
        best.append(row)

    cuts, langs, marks = [], [], []
    for i, line in enumerate(lines):
        scores = [s for s, _ in best[i]]
        heard_in = sum(1 for s in scores if s >= args.heard)

        # A line no run contains. With several runs this is strong: a line missing
        # every night is cut, where a line missing once may just have been fluffed.
        if heard_in == 0 and len(tokens(line["text"])) >= 3:
            cuts.append((i, line))
            continue

        # Tagged one language, spoken in another. The importer guesses language from
        # the text, and on short lines it guesses wrong; what was actually heard is
        # better evidence than what the words looked like.
        tagged = (line.get("lang") or script.get("defaultLang", ["?"]))[0]
        spoken = [r["langs"][0] for s, r in best[i] if r and s >= args.heard]
        if spoken and all(l != tagged for l in spoken):
            heard_words = [w for s, r in best[i] if r and s >= args.heard for w in tokens(r["text"])]
            scored = {
                lang: sum(w in mk for w in heard_words) for lang, mk in LANG_MARKERS.items()
            }
            better = max(scored, key=lambda k: scored[k])
            if scored[better] >= 2 and better != tagged:
                langs.append((i, line, tagged, better))

        # A landmark must be recognised strongly *and* be unlike everything else in
        # the script, or re-anchoring on it will land in the wrong place.
        if min(scores) >= args.landmark and len(tokens(line["text"])) >= 6:
            mine = set(tokens(line["text"]))
            rival = max(
                (dice(mine, set(tokens(o["text"]))) for j, o in enumerate(lines) if j != i),
                default=0.0,
            )
            if rival < 0.45:
                marks.append((i, line, min(scores), rival))

    print(f"{len(lines)} lines, {n_runs} run(s)\n")

    print(f"PROBABLY CUT — no run contains them ({len(cuts)})")
    for i, l in cuts[:40]:
        print(f'    {l["id"]}  {l["text"][:74]}')
    if len(cuts) > 40:
        print(f"    ... and {len(cuts) - 40} more")

    print(f"\nLANGUAGE LOOKS WRONG — tagged one way, spoken another ({len(langs)})")
    for i, l, was, now in langs[:20]:
        print(f'    {l["id"]}  {was} -> {now}   {l["text"][:60]}')

    print(f"\nLANDMARK CANDIDATES — recognised every run, unlike any other line ({len(marks)})")
    for i, l, sc, rival in sorted(marks, key=lambda m: -m[2])[:20]:
        print(f'    {l["id"]}  heard {sc:.2f}, nearest other line {rival:.2f}   {l["text"][:52]}')

    if not args.out:
        print("\n(pass -o to write a script with these applied)")
        return

    for i, _ in cuts:
        # Marked, never removed: a line cut this week may be restored next week, and
        # anything anchored to it must survive in the meantime.
        lines[i]["cut"] = True
    for i, _, _, now in langs:
        lines[i]["lang"] = [now]
    for i, _, _, _ in marks:
        lines[i]["landmark"] = 3
    args.out.write_text(json.dumps(script, ensure_ascii=False, indent=1) + "\n")
    print(f"\nwrote {args.out}")
    print("Review it before a run. These are proposals from one recording, not facts.")


if __name__ == "__main__":
    main()
