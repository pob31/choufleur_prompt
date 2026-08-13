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
        let folded = self.get(lang).fold(&base);
        let tokens = tokens(&folded).map(str::to_string).collect();
        MatchText { folded, tokens }
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
