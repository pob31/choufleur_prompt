#!/usr/bin/env python3
"""Build one corpus manifest per (scene, date) for the La Reprise recordings.

The same scene recorded on several nights is the whole point of the exercise — a
variant learned on Monday is only worth anything if it still helps on Thursday — so
a take, not a scene, is the unit a manifest describes. Manifests are written as
`manifest-<date>.json` beside the audio, and the tools accept a manifest path
directly wherever they accept a corpus directory.

    python make_manifests.py test_Choufleur/LaReprise_MiloRau 20190111

Audio files are matched by their stem plus a `_<date>` suffix, so nothing has to be
moved or renamed. What cannot be derived from a filename — which actor plays which
role, and which mics carry no identity at all — lives in the table below.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

# scene directory -> file stem -> character id, or None for a zone channel.
#
# A zone channel is a mic with no speaker identity: an area or boom mic, matched
# against any expected speaker (PRD, *Ambient / area microphones*). Order here is
# the channel order in the manifest.
SCENES: dict[str, dict[str, str | None]] = {
    "Johan_1": {"Johan_1": "char-johan"},
    "sebastien_2": {"sebastien_2": "char-sebastien"},
    "saraFabian_3": {"Sara_3": "char-sara", "Fabian_3": "char-fabian"},
    "sebastienTom_4": {"Sebastien_4": "char-sebastien", "Tom_4": "char-tom"},
    # A single boom covering Johan and Suzy: no identity to attach.
    "Perche_5": {"Perche_5": None},
    # The script names roles, the files name actors. Ihsane is played by Tom, who
    # wears no mic — he is stripped and beaten in the scene — so his lines exist
    # only on the two car mics. This is the PRD's hybrid-cast case exactly.
    "tutti_6": {
        "Fabian_6": "char-wintgens",
        "Seb_6": "char-parmentier",
        "Sara_6": "char-leukeu",
        "Suzy_6": "char-suzy",
        "VoiturePassager_6": None,
        "VoitureCoffre_6": None,
    },
}

NOTES = {
    "Johan_1": "Johan, Dutch and English, single close mic.",
    "sebastien_2": "Sebastien, French documentary monologue, very long paragraphs.",
    "saraFabian_3": "Sara and Fabian, French with heavy Flemish and Liege accents.",
    "sebastienTom_4": "Sebastien and Tom, French with some adlib in gibberish.",
    "Perche_5": "Johan and Suzy on one boom mic. Zone channel: no speaker identity.",
    "tutti_6": "The car scene. Cross talk, fighting, grunting. Ihsane has no mic of "
    "his own; his lines exist only on the two car zone mics.",
}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", type=Path, help="the LaReprise_MiloRau directory")
    ap.add_argument("date", help="the take's date suffix, e.g. 20190111")
    ap.add_argument("--show", default="la-reprise")
    args = ap.parse_args()

    if not re.fullmatch(r"\d{6,8}", args.date):
        sys.exit(f"date {args.date!r} does not look like a date suffix")

    written, missing = 0, []
    for scene, mapping in SCENES.items():
        d = args.root / scene
        if not d.is_dir():
            missing.append(f"{scene}: no such directory")
            continue

        channels, absent = [], []
        for index, (stem, character) in enumerate(mapping.items(), start=1):
            wav = d / f"{stem}_{args.date}.wav"
            if not wav.exists():
                absent.append(wav.name)
                continue
            entry: dict = {"index": index, "file": wav.name}
            if character:
                entry["character"] = character
            else:
                entry["note"] = "zone channel: no speaker identity"
            channels.append(entry)

        if not channels:
            missing.append(f"{scene}: no audio for {args.date}")
            continue
        if absent:
            print(f"  {scene}: missing {', '.join(absent)}", file=sys.stderr)

        script = d / "script.json"
        if not script.exists():
            missing.append(f"{scene}: no script.json — run docx_to_script.py first")
            continue

        manifest = {
            "format": "choufleur-corpus",
            "formatVersion": "0.1",
            "show": args.show,
            "act": f"{scene}-{args.date}",
            "note": f"{NOTES.get(scene, '')} Take of {args.date}.",
            "sampleRate": 48000,
            "script": "script.json",
            "channels": channels,
            "provenance": {
                "production": "La Reprise - Histoire(s) du theatre (I), Milo Rau",
                "date": args.date,
                "scene": scene,
            },
        }
        out = d / f"manifest-{args.date}.json"
        out.write_text(json.dumps(manifest, ensure_ascii=False, indent=1) + "\n")
        print(f"  {out}  ({len(channels)} channel(s))")
        written += 1

    print(f"\n{written} manifest(s) written for {args.date}")
    for m in missing:
        print(f"  skipped {m}")
    if written:
        print("\nnext: choufleur-replay verify <manifest> --update-hashes")


if __name__ == "__main__":
    main()
