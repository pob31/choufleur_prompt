#!/usr/bin/env python3
"""Draft ground truth for a Choufleur corpus by forced alignment.

Labelling an act by hand is a day's work. Aligning first and correcting the result
is an hour or two, which is the only reason a labelled corpus exists at all.

This is a *sidecar*: nothing in server/ imports it, and nothing it produces is
trusted without a human pass. Its output is explicitly named `gt-draft.jsonl`.

    python align.py ../corpus/seagull-act1
    python align.py ../corpus/seagull-act1 --from-labels gt-draft.labels.txt

The first form aligns the mixdown against the script and writes a draft plus an
Audacity label track. Correct the labels in Audacity, export them, and the second
form folds them back into a finished ground-truth file.
"""

from __future__ import annotations

import argparse
import json
import sys
import unicodedata
from pathlib import Path

# --- Text normalization -----------------------------------------------------
# Mirrors notation spec §3.2 and the Rust implementation in choufleur-core:
# NFC, lowercase, punctuation to a space, whitespace collapsed. Kept in step so
# alignment scores and tracker scores are talking about the same strings.


def normalize(text: str) -> str:
    out = []
    pending_space = False
    for ch in unicodedata.normalize("NFC", text).lower():
        if ch.isspace() or unicodedata.category(ch).startswith("P"):
            pending_space = bool(out)
        else:
            if pending_space:
                out.append(" ")
                pending_space = False
            out.append(ch)
    return "".join(out)


# --- Corpus I/O -------------------------------------------------------------


def load_corpus(corpus_dir: Path):
    manifest = json.loads((corpus_dir / "manifest.json").read_text())
    script = json.loads((corpus_dir / manifest["script"]).read_text())
    return manifest, script


def channel_of(manifest, character_id):
    for ch in manifest["channels"]:
        if ch.get("character") == character_id:
            return ch["index"]
    return None


def write_jsonl(path: Path, records):
    with path.open("w") as f:
        for r in records:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")


def write_audacity_labels(path: Path, records, script_by_id):
    """Audacity label track: start<TAB>end<TAB>text, one per line."""
    with path.open("w") as f:
        for r in records:
            line = script_by_id.get(r["lineId"], {})
            text = line.get("text", "")[:60].replace("\t", " ")
            f.write(f"{r['onset']:.3f}\t{r['end']:.3f}\t{r['lineId']} {text}\n")


# --- Alignment --------------------------------------------------------------


def align(corpus_dir: Path, model_size: str, device: str):
    try:
        import whisperx  # type: ignore
    except ImportError:
        sys.exit(
            "whisperx is not installed.\n"
            "    python3 -m venv .venv && . .venv/bin/activate\n"
            "    pip install -r requirements.txt"
        )

    manifest, script = load_corpus(corpus_dir)
    mixdown = manifest.get("mixdown", {}).get("file")
    if not mixdown:
        sys.exit("this corpus has no mixdown; alignment runs against the mixed feed")
    audio_path = corpus_dir / mixdown
    if not audio_path.exists():
        sys.exit(f"missing {audio_path} — is the audio on an external drive?")

    default_lang = (script.get("defaultLang") or ["en"])[0].split("-")[0]
    print(f"transcribing {audio_path.name} with whisper {model_size} ({default_lang})…")
    audio = whisperx.load_audio(str(audio_path))
    asr = whisperx.load_model(model_size, device, compute_type="int8")
    result = asr.transcribe(audio, language=default_lang, batch_size=8)

    print("aligning to word timestamps…")
    model_a, meta = whisperx.load_align_model(language_code=default_lang, device=device)
    aligned = whisperx.align(result["segments"], model_a, meta, audio, device)

    words = [
        w for w in aligned.get("word_segments", []) if "start" in w and "word" in w
    ]
    if not words:
        sys.exit("alignment produced no word timestamps")
    print(f"  {len(words)} word timestamps")

    return match_script_to_words(script, words, manifest)


def match_script_to_words(script, words, manifest):
    """Walk the script and the word stream together, monotonically.

    Both are in performance order, so a greedy forward walk is enough — and it is
    the right shape anyway, since the output is a *draft* a human will correct.
    """
    tokens = [(normalize(w["word"]).strip(), w) for w in words]
    tokens = [(t, w) for t, w in tokens if t]

    records = []
    cursor = 0
    for line in script["lines"]:
        want = normalize(line["text"]).split()
        if not want:
            continue
        best_i, best_hits = None, 0
        # Look ahead a bounded distance; a line is somewhere near where the last
        # one ended, or the alignment has gone wrong in a way a wider search
        # would only disguise.
        for i in range(cursor, min(cursor + 80, len(tokens))):
            window = [t for t, _ in tokens[i : i + len(want)]]
            hits = sum(1 for a, b in zip(window, want) if a == b)
            if hits > best_hits:
                best_i, best_hits = i, hits
            if hits == len(want):
                break

        if best_i is None or best_hits < max(1, len(want) // 3):
            # Not confidently found: emit it as omitted so the line is visible in
            # the draft and a human decides, rather than silently dropping it.
            records.append(
                {
                    "lineId": line["id"],
                    "onset": tokens[cursor][1]["start"] if cursor < len(tokens) else 0.0,
                    "end": tokens[cursor][1]["start"] if cursor < len(tokens) else 0.0,
                    "omitted": True,
                    "note": "not found by alignment — check or delete",
                }
            )
            continue

        end_i = min(best_i + len(want), len(tokens)) - 1
        rec = {
            "lineId": line["id"],
            "onset": round(float(tokens[best_i][1]["start"]), 3),
            "end": round(float(tokens[end_i][1].get("end", tokens[end_i][1]["start"])), 3),
        }
        ch = channel_of(manifest, line["character"])
        if ch is not None:
            rec["channel"] = ch
        records.append(rec)
        cursor = end_i + 1

    return records


# --- Label round-trip -------------------------------------------------------


def from_labels(corpus_dir: Path, labels_path: Path):
    manifest, script = load_corpus(corpus_dir)
    by_id = {l["id"]: l for l in script["lines"]}
    records = []
    for n, raw in enumerate(labels_path.read_text().splitlines(), 1):
        if not raw.strip():
            continue
        parts = raw.split("\t")
        if len(parts) < 3:
            sys.exit(f"{labels_path}:{n}: expected start<TAB>end<TAB>text")
        start, end, text = float(parts[0]), float(parts[1]), parts[2]
        line_id = text.split()[0] if text.split() else ""
        if line_id not in by_id:
            sys.exit(f"{labels_path}:{n}: {line_id!r} is not a line in the script")
        rec = {"lineId": line_id, "onset": round(start, 3), "end": round(end, 3)}
        ch = channel_of(manifest, by_id[line_id]["character"])
        if ch is not None:
            rec["channel"] = ch
        records.append(rec)
    records.sort(key=lambda r: r["onset"])
    return records


# --- Entry point ------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("corpus", type=Path)
    ap.add_argument("--model", default="medium", help="whisper size for alignment")
    ap.add_argument("--device", default="cpu", help="cpu, cuda; mps is unsupported by whisperx")
    ap.add_argument("--from-labels", type=Path, help="fold a corrected Audacity label track back in")
    ap.add_argument("-o", "--out", type=Path, help="output file")
    args = ap.parse_args()

    if args.from_labels:
        labels = args.from_labels
        if not labels.is_absolute():
            labels = args.corpus / labels
        records = from_labels(args.corpus, labels)
        out = args.out or args.corpus / "ground-truth.jsonl"
        write_jsonl(out, records)
        print(f"wrote {len(records)} labelled line(s) to {out}")
        print("now run: choufleur-replay verify", args.corpus)
        return

    records = align(args.corpus, args.model, args.device)
    out = args.out or args.corpus / "gt-draft.jsonl"
    write_jsonl(out, records)

    _, script = load_corpus(args.corpus)
    by_id = {l["id"]: l for l in script["lines"]}
    labels_out = out.with_suffix(".labels.txt")
    write_audacity_labels(labels_out, records, by_id)

    found = sum(1 for r in records if not r.get("omitted"))
    print(f"\nwrote {out} ({found}/{len(records)} lines located)")
    print(f"wrote {labels_out}")
    print(
        "\nNext: open the mixdown in Audacity, File > Import > Labels, correct the\n"
        "onsets (±200 ms is the target), export the labels, then run:\n"
        f"    python align.py {args.corpus} --from-labels {labels_out.name}"
    )


if __name__ == "__main__":
    main()
