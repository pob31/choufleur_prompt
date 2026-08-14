#!/usr/bin/env python3
"""Would matching on sound instead of spelling fix the errors we actually see?

Every mishearing collected from watching Hécube run is a *homophone*. The recogniser
hears the sounds correctly and writes them down wrongly, usually by putting the word
boundaries somewhere else:

    heard                    script
    "le fils des cubes"      "le fils d'Hécube"
    "Polyme Store"           "Polymestor"
    "en Tassé-le-Bitain"     "entassé le butin"
    "Écube de Ripi"          "Hécube d'Euripide"
    "Jean Tracour"           "J'entre à cour"

Read those aloud and they are the same sound. So the matcher is failing on a
representation problem, not a recognition problem — the information it needs arrived
intact and was thrown away by comparing letters.

This is a probe, not an implementation: a deliberately coarse French phonetic folding,
run over the real pairs, to find out whether the idea earns a place in `choufleur-core`
before anything is built there. Two things it must show — the true pairs collapse, and
*unrelated* lines do not. The second matters more: a folding aggressive enough to make
everything match everything would score wonderfully here and destroy the tracker.

    python phonetic_probe.py
"""

from __future__ import annotations

import re
import unicodedata


def fold_fr(text: str) -> str:
    """A crude French orthography → sound mapping.

    Not a G2P model and not trying to be. It encodes the handful of rules that cause
    the confusions above: silent letters, the several spellings of one vowel, and
    consonants that are written differently and said identically.
    """
    t = unicodedata.normalize("NFD", text.lower())
    t = "".join(c for c in t if unicodedata.category(c) != "Mn")
    t = re.sub(r"[^a-z\s]", " ", t)

    # Word boundaries are exactly what the recogniser gets wrong, so they go last of
    # all — but elisions have to be opened up first or "d'Hécube" and "des cubes"
    # start from different letters.
    t = re.sub(r"\s+", " ", t).strip()

    rules = [
        (r"ph", "f"), (r"gu(?=[eiy])", "g"), (r"qu", "k"), (r"q", "k"),
        (r"c(?=[eiy])", "s"), (r"c", "k"), (r"ck", "k"),
        (r"g(?=[eiy])", "j"),
        (r"eau", "o"), (r"au", "o"), (r"ou", "u"), (r"oi", "wa"),
        (r"ai", "e"), (r"ei", "e"), (r"eu", "e"), (r"oe", "e"),
        (r"ille", "iy"), (r"ll", "l"),
        (r"tion", "sion"),
        (r"h", ""),                    # always silent in French
        (r"y", "i"), (r"w", "v"),
        (r"(.)\1", r"\1"),             # double consonants say one sound
        (r"e(?=\b)", ""),              # final e is mute
        (r"[stdxzp](?=\b)", ""),       # final consonants generally are too
        (r"er\b", "e"), (r"ez\b", "e"),
    ]
    for pat, rep in rules:
        t = re.sub(pat, rep, t)
    return re.sub(r"\s+", " ", t).strip()


def keyed(text: str) -> str:
    """The folded form with word boundaries removed — the comparison that survives a
    recogniser splitting words in the wrong places."""
    return fold_fr(text).replace(" ", "")


def trigrams(t: str) -> set[str]:
    t = f" {t} "
    return {t[i:i + 3] for i in range(len(t) - 2)}


def dice(a: set[str], b: set[str]) -> float:
    return 2 * len(a & b) / (len(a) + len(b)) if a and b else 0.0


TRUE_PAIRS = [
    ("le fils des cubes", "le fils d'Hécube"),
    ("Polyme Store", "Polymestor"),
    ("en Tassé-le-Bitain", "entassé le butin"),
    ("Écube de Ripi", "Hécube d'Euripide"),
    ("Jean Tracour", "J'entre à cour"),
    ("Athéna III-Yenne", "Athéna Troyenne"),
    ("Polymédor", "Polymestor"),
    ("les heures", "l'Hécube"),
]

# The control. These must NOT collapse, or the folding has simply made the script
# uniform and the tracker would match anything to anywhere.
FALSE_PAIRS = [
    ("C'est notre premier jour de répétition", "Nous jouons aussi d'autres personnages"),
    ("Je vais continuer d'aboyer", "Pensez aux familles qui n'ont pas les moyens"),
    ("Silence, mes amies", "Quelle heure est-il ?"),
    ("Un temps", "Je n'ai pas encore la nationalité"),
    ("Nous sommes le Chœur", "Il faut des figurants ?"),
]


def main() -> None:
    print("TRUE pairs — a mishearing and the line it was trying to be")
    print(f"{'heard':<24}{'script':<24}{'letters':>9}{'sound':>8}")
    gains = []
    for heard, script in TRUE_PAIRS:
        by_letter = dice(trigrams(heard.lower()), trigrams(script.lower()))
        by_sound = dice(trigrams(keyed(heard)), trigrams(keyed(script)))
        gains.append(by_sound - by_letter)
        print(f"{heard[:23]:<24}{script[:23]:<24}{by_letter:>9.2f}{by_sound:>8.2f}")

    print("\nFALSE pairs — unrelated lines, which must stay apart")
    worst = 0.0
    for a, b in FALSE_PAIRS:
        by_letter = dice(trigrams(a.lower()), trigrams(b.lower()))
        by_sound = dice(trigrams(keyed(a)), trigrams(keyed(b)))
        worst = max(worst, by_sound)
        print(f"{a[:23]:<24}{b[:23]:<24}{by_letter:>9.2f}{by_sound:>8.2f}")

    print(f"\n  mean gain on true pairs      {sum(gains) / len(gains):+.2f}")
    print(f"  worst score on a false pair  {worst:.2f}")
    print("\n  accept_threshold is 0.62; follow_threshold 0.45.")
    print("  The idea is worth building only if the true column clears those and the")
    print("  false column stays well below them.")


if __name__ == "__main__":
    main()
