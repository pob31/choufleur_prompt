//! Per-language normalization applied *on top of* the §3.2 base pipeline.
//!
//! This layer exists for matching only — never for the line-ID hash of §3.1,
//! which is defined over [`normalize_base`](crate::normalize::normalize_base)
//! alone. The invariant that makes the whole matcher work: script lines and ASR
//! hypotheses go through the *identical* pipeline, so most orthographic quirks
//! cancel out instead of being modelled.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::normalize::{fold_diacritics, normalize_base, tokens};

/// A BCP-47 language tag, stored lowercase (`"fr"`, `"en"`, `"pt-br"`).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LangCode(String);

impl LangCode {
    pub fn new(tag: &str) -> Self {
        LangCode(tag.trim().to_lowercase())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Primary subtag: `pt-br` → `pt`. Used for normalizer lookup fallback.
    pub fn primary(&self) -> &str {
        self.0.split('-').next().unwrap_or(&self.0)
    }
}

impl fmt::Debug for LangCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl fmt::Display for LangCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl From<&str> for LangCode {
    fn from(s: &str) -> Self {
        LangCode::new(s)
    }
}

/// Text prepared for matching: the folded string plus its tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchText {
    pub folded: String,
    pub tokens: Vec<String>,
    /// The same text as sound, boundary-free. Precomputed here so the hot path never
    /// folds anything: a script line is prepared once and compared thousands of times.
    pub sound: String,
}

impl MatchText {
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
    pub fn token_refs(&self) -> Vec<&str> {
        self.tokens.iter().map(String::as_str).collect()
    }
}

/// A per-language folding policy, layered over the §3.2 base normalization.
pub trait LangNormalizer: Send + Sync {
    fn lang(&self) -> &LangCode;
    /// Input is the output of [`normalize_base`]; output is match-ready text.
    fn fold(&self, base_norm: &str) -> String;

    /// The same text folded toward **sound**, with word boundaries removed.
    ///
    /// Every mishearing collected from watching real performances is a homophone: the
    /// recogniser hears correctly and writes it down wrongly, usually by putting the
    /// word boundaries somewhere else. "Polyme Store" for "Polymestor", "le fils des
    /// cubes" for "le fils d'Hécube", "Jean Tracour" for "J'entre à cour". Compared as
    /// letters those range from mediocre to invisible; compared as sound they are the
    /// same utterance, which is what they were.
    ///
    /// Boundaries go because they are precisely what is unreliable — a segmentation
    /// that splits one word into two is the commonest failure and the one that costs
    /// the most, since it zeroes the token overlap the matcher multiplies by.
    ///
    /// Default is the folded text with spaces stripped: no phonetic claim for a
    /// language nobody has studied here, but still boundary-free, which is the half of
    /// the benefit that needs no linguistics.
    fn phonetic(&self, base_norm: &str) -> String {
        self.fold(base_norm).replace(' ', "")
    }
}

/// Passthrough — the honest default for a language we have not studied.
pub struct NullNormalizer(LangCode);
impl NullNormalizer {
    pub fn new(lang: LangCode) -> Self {
        NullNormalizer(lang)
    }
}
impl LangNormalizer for NullNormalizer {
    fn lang(&self) -> &LangCode {
        &self.0
    }
    fn fold(&self, base_norm: &str) -> String {
        base_norm.to_string()
    }
}

/// English: base normalization already split contractions on the apostrophe
/// (`don't` → `don t`), so the remaining job is normalizing the orphaned
/// fragments that ASR and script spell differently.
pub struct EnNormalizer(LangCode);
impl Default for EnNormalizer {
    fn default() -> Self {
        EnNormalizer(LangCode::new("en"))
    }
}
impl LangNormalizer for EnNormalizer {
    fn lang(&self) -> &LangCode {
        &self.0
    }
    fn fold(&self, base_norm: &str) -> String {
        let mut out: Vec<&str> = Vec::new();
        for t in tokens(base_norm) {
            // Apostrophe fragments left behind by §3.2 step 3, plus the stems
            // whose spelling the apostrophe swallowed (`don` → `do`).
            // Deliberately excluded: `s` (possessive vs. *is*) and `d`
            // (*would* vs. *had*) — ambiguous, and symmetric on both sides
            // of the comparison anyway.
            out.push(match t {
                "t" => "not",
                "re" => "are",
                "ve" => "have",
                "ll" => "will",
                "m" => "am",
                "don" => "do",
                "doesn" => "does",
                "didn" => "did",
                "won" => "will",
                "isn" => "is",
                "aren" => "are",
                "wasn" => "was",
                "weren" => "were",
                "hasn" => "has",
                "haven" => "have",
                "hadn" => "had",
                "couldn" => "could",
                "wouldn" => "would",
                "shouldn" => "should",
                other => other,
            });
        }
        out.join(" ")
    }
}

/// French: fold diacritics (ASR is unreliable about accents), expand the elided
/// forms base normalization has already separated, and unpack the two ligatures.
pub struct FrNormalizer(LangCode);
impl Default for FrNormalizer {
    fn default() -> Self {
        FrNormalizer(LangCode::new("fr"))
    }
}
/// French orthography folded toward the sounds it stands for.
///
/// Deliberately coarse. This is not grapheme-to-phoneme and does not want to be: it
/// encodes the handful of regularities behind the confusions actually observed —
/// silent letters, one sound spelled several ways, and consonants written differently
/// and said identically. Measured on the real pairs before it was written, mean
/// similarity rose 0.14 while unrelated lines stayed at 0.17 against a 0.45 follow
/// threshold. That second number is the one that mattered: a folding aggressive enough
/// to collapse everything would have scored beautifully on the true pairs and made the
/// tracker match anything to anywhere.
///
/// Order is not arbitrary. `qu`→`k` must precede the `c`→`s`/`k` split, and doubled
/// consonants collapse late so that `ll` in `ille` is handled as part of the vowel.
fn fold_sound_fr(folded: &str) -> String {
    let mut out = String::with_capacity(folded.len());
    let chars: Vec<char> = folded.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied().unwrap_or(' ');
        let after = chars.get(i + 2).copied().unwrap_or(' ');
        let front = matches!(next, 'e' | 'i' | 'y');
        match c {
            // Silent in every position in French; the single highest-value rule here,
            // because it is what separates "Hécube" from "des cubes".
            'h' => {}
            ' ' => {}
            'p' if next == 'h' => {
                out.push('f');
                i += 1;
            }
            'q' if next == 'u' => {
                out.push('k');
                i += 1;
            }
            'q' => out.push('k'),
            'c' if next == 'h' => {
                out.push('s');
                i += 1;
            }
            'c' if front => out.push('s'),
            'c' => out.push('k'),
            'g' if next == 'u' && matches!(after, 'e' | 'i' | 'y') => {
                out.push('g');
                i += 1;
            }
            'g' if front => out.push('j'),
            'e' if next == 'a' && after == 'u' => {
                out.push('o');
                i += 2;
            }
            'a' if next == 'u' => {
                out.push('o');
                i += 1;
            }
            'o' if next == 'u' => {
                out.push('u');
                i += 1;
            }
            'o' if next == 'i' => {
                out.push_str("wa");
                i += 1;
            }
            'a' | 'e' if next == 'i' => {
                out.push('e');
                i += 1;
            }
            'e' if next == 'u' => {
                out.push('e');
                i += 1;
            }
            'y' => out.push('i'),
            'w' => out.push('v'),
            'z' => out.push('s'),
            other => out.push(other),
        }
        i += 1;
    }
    // Doubled letters say one sound.
    let mut squeezed = String::with_capacity(out.len());
    for c in out.chars() {
        if !squeezed.ends_with(c) {
            squeezed.push(c);
        }
    }
    squeezed
}

impl LangNormalizer for FrNormalizer {
    fn lang(&self) -> &LangCode {
        &self.0
    }
    fn fold(&self, base_norm: &str) -> String {
        let folded = fold_diacritics(base_norm)
            .replace('œ', "oe")
            .replace('æ', "ae");
        let mut out: Vec<&str> = Vec::new();
        for t in tokens(&folded) {
            // `j'suis` → `j suis` → `je suis`; the script may spell it either way
            // and so may Whisper.
            let expanded = match t {
                "j" => "je",
                "l" => "le",
                "d" => "de",
                "c" => "ce",
                "n" => "ne",
                "s" => "se",
                "t" => "te",
                "m" => "me",
                "qu" => "que",
                "jusqu" => "jusque",
                "lorsqu" => "lorsque",
                "puisqu" => "puisque",
                "quelqu" => "quelque",
                other => other,
            };
            out.push(expanded);
        }
        // `out` borrows from `folded`, so materialize before it drops.
        out.join(" ")
    }

    fn phonetic(&self, base_norm: &str) -> String {
        fold_sound_fr(&self.fold(base_norm))
    }
}


/// Registry of language normalizers; EN and FR are implemented for real, every
/// other tag degrades to [`NullNormalizer`] rather than pretending.
pub struct NormalizerRegistry {
    by_lang: HashMap<String, Box<dyn LangNormalizer>>,
    fallback: HashMap<String, Box<dyn LangNormalizer>>,
}

impl NormalizerRegistry {
    pub fn with_defaults() -> Self {
        let mut by_lang: HashMap<String, Box<dyn LangNormalizer>> = HashMap::new();
        by_lang.insert("en".into(), Box::new(EnNormalizer::default()));
        by_lang.insert("fr".into(), Box::new(FrNormalizer::default()));
        NormalizerRegistry {
            by_lang,
            fallback: HashMap::new(),
        }
    }

    /// Look up by full tag, then by primary subtag, then install a passthrough.
    pub fn get(&mut self, lang: &LangCode) -> &dyn LangNormalizer {
        let key = if self.by_lang.contains_key(lang.as_str()) {
            lang.as_str().to_string()
        } else if self.by_lang.contains_key(lang.primary()) {
            lang.primary().to_string()
        } else {
            let k = lang.as_str().to_string();
            self.fallback
                .entry(k.clone())
                .or_insert_with(|| Box::new(NullNormalizer::new(lang.clone())));
            return self.fallback[&k].as_ref();
        };
        self.by_lang[&key].as_ref()
    }

    /// Full pipeline: §3.2 base normalization, then the language fold, then tokens.
    pub fn prepare(&mut self, text: &str, lang: &LangCode) -> MatchText {
        let base = normalize_base(text);
        let n = self.get(lang);
        let folded = n.fold(&base);
        let sound = n.phonetic(&base);
        let tokens = tokens(&folded).map(str::to_string).collect();
        MatchText {
            folded,
            tokens,
            sound,
        }
    }
}

impl Default for NormalizerRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prep(text: &str, lang: &str) -> MatchText {
        NormalizerRegistry::with_defaults().prepare(text, &LangCode::new(lang))
    }

    #[test]
    fn french_elision_and_accents_cancel_between_script_and_asr() {
        // Script spells the elision, Whisper writes it out (or vice versa).
        assert_eq!(
            prep("J'suis là !", "fr").folded,
            prep("je suis la", "fr").folded
        );
        assert_eq!(
            prep("T'as vu ?", "fr").folded,
            prep("te as vu", "fr").folded
        );
        assert_eq!(
            prep("Tu ne devrais pas être ici.", "fr").folded,
            "tu ne devrais pas etre ici"
        );
    }

    #[test]
    fn french_ligatures() {
        assert_eq!(prep("Le cœur", "fr").folded, "le coeur");
    }

    #[test]
    fn english_contractions() {
        assert_eq!(
            prep("Don't go", "en").folded,
            prep("do not go", "en").folded
        );
        assert_eq!(
            prep("We're here", "en").folded,
            prep("we are here", "en").folded
        );
        assert_eq!(
            prep("I've seen it", "en").folded,
            prep("i have seen it", "en").folded
        );
    }

    #[test]
    fn unknown_language_passes_through_unharmed() {
        let mt = prep("Jag förstår inte.", "sv");
        assert_eq!(mt.folded, "jag förstår inte");
        assert_eq!(mt.tokens, vec!["jag", "förstår", "inte"]);
    }

    #[test]
    fn region_subtag_falls_back_to_primary() {
        assert_eq!(prep("Não sei", "pt-BR").folded, "não sei");
        let mut reg = NormalizerRegistry::with_defaults();
        assert_eq!(reg.get(&LangCode::new("fr-CA")).lang().as_str(), "fr");
    }

    #[test]
    fn lang_code_normalizes_case() {
        assert_eq!(LangCode::new("FR"), LangCode::new("fr"));
        assert_eq!(LangCode::new("pt-BR").primary(), "pt");
    }
}

#[cfg(test)]
mod phonetic_tests {
    use super::*;

    fn sound(text: &str) -> String {
        let n = FrNormalizer::default();
        n.phonetic(&crate::normalize::normalize_base(text))
    }

    /// The real mishearings this exists for, each with the line it was trying to be.
    #[test]
    fn homophones_collapse() {
        for (heard, script) in [
            ("Polyme Store", "Polymestor"),
            ("le fils des cubes", "le fils d'Hécube"),
            ("Écube de Ripi", "Hécube d'Euripide"),
            ("Polymédor", "Polymestor"),
        ] {
            let (a, b) = (sound(heard), sound(script));
            let sim = crate::matcher::char_trigram_dice(&a, &b);
            assert!(sim > 0.55, "{heard:?} vs {script:?}: {a} / {b} = {sim:.2}");
        }
    }

    /// The check that decides whether the idea is safe at all. A folding that made
    /// everything match everything would pass the test above and ruin the tracker.
    #[test]
    fn unrelated_lines_stay_apart() {
        for (a, b) in [
            ("C'est notre premier jour de répétition", "Nous jouons aussi d'autres personnages"),
            ("Je vais continuer d'aboyer", "Pensez aux familles qui n'ont pas les moyens"),
            ("Nous sommes le Chœur", "Il faut des figurants ?"),
        ] {
            let sim = crate::matcher::char_trigram_dice(&sound(a), &sound(b));
            assert!(sim < 0.45, "{a:?} vs {b:?} = {sim:.2} — too close");
        }
    }

    #[test]
    fn h_is_silent_and_boundaries_are_gone() {
        assert_eq!(sound("Hécube"), sound("écube"));
        assert!(!sound("le fils d'Hécube").contains(' '));
    }

    /// A language nobody has studied here still gets the boundary-free half.
    #[test]
    fn the_default_is_boundary_free_and_makes_no_phonetic_claim() {
        let n = NullNormalizer::new(LangCode::new("xx"));
        assert_eq!(n.phonetic("two words"), "twowords");
    }
}
