//! Text normalization, normative per notation spec §3.2.
//!
//! | Step | Rule |
//! |------|------|
//! | 1 | Unicode NFC normalization |
//! | 2 | Lowercase (locale-independent) |
//! | 3 | Strip punctuation (Unicode category P) |
//! | 4 | Collapse runs of whitespace to a single space, trim |
//!
//! # Interpretation decision (load-bearing — Phase 1 line-ID hashes depend on it)
//!
//! Step 3 *replaces* each punctuation character with a space rather than deleting it.
//! `"well-known"` normalizes to `"well known"`, not `"wellknown"`; `"j'suis"` to
//! `"j suis"`. Deleting would fuse tokens across hyphens and apostrophes and make
//! token-set matching lumpier than word-level ASR output ever is. The line-ID hash
//! defined in notation §3.1 is computed over this output, so changing this rule
//! later changes every line ID: don't.

use unicode_normalization::UnicodeNormalization;
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};

/// Normalize per notation spec §3.2. Idempotent.
pub fn normalize_base(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.nfc().flat_map(|c| c.to_lowercase()) {
        let is_sep =
            ch.is_whitespace() || ch.general_category_group() == GeneralCategoryGroup::Punctuation;
        if is_sep {
            // Only remember that a separator occurred; emitted lazily so runs
            // collapse and leading/trailing separators are trimmed for free.
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Whitespace-separated tokens of an already-normalized string.
pub fn tokens(normalized: &str) -> impl Iterator<Item = &str> {
    normalized.split(' ').filter(|t| !t.is_empty())
}

/// Strip combining marks (Unicode category Mn) — used by language normalizers
/// whose policy is to fold diacritics. Not part of §3.2.
pub fn fold_diacritics(s: &str) -> String {
    s.nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .nfc()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_table_examples() {
        assert_eq!(
            normalize_base("Tu ne devrais pas être ici."),
            "tu ne devrais pas être ici"
        );
        assert_eq!(normalize_base("  Hello,   WORLD!  "), "hello world");
        assert_eq!(
            normalize_base("To be, or not to be — that is the question."),
            "to be or not to be that is the question"
        );
    }

    #[test]
    fn punctuation_becomes_a_separator_not_a_deletion() {
        assert_eq!(normalize_base("well-known"), "well known");
        assert_eq!(normalize_base("j'suis"), "j suis");
        // Curly and straight apostrophes are both category P and behave identically.
        assert_eq!(normalize_base("j\u{2019}suis"), normalize_base("j'suis"));
    }

    #[test]
    fn nfc_composed_and_decomposed_agree() {
        let composed = "\u{ea}tre"; // ê as U+00EA
        let decomposed = "e\u{302}tre"; // e + combining circumflex
        assert_eq!(normalize_base(decomposed), normalize_base(composed));
        assert_eq!(normalize_base(composed), "être");
    }

    #[test]
    fn is_idempotent() {
        for s in ["Tu ne devrais pas être ici.", "  a,,,b  ", "«Bonjour!»", ""] {
            let once = normalize_base(s);
            assert_eq!(normalize_base(&once), once, "not idempotent for {s:?}");
        }
    }

    #[test]
    fn empty_and_punctuation_only() {
        assert_eq!(normalize_base(""), "");
        assert_eq!(normalize_base("..."), "");
        assert_eq!(normalize_base(" — "), "");
    }

    #[test]
    fn diacritic_folding() {
        assert_eq!(fold_diacritics("être forcé où"), "etre force ou");
        assert_eq!(fold_diacritics("ça"), "ca");
    }

    #[test]
    fn token_split() {
        let n = normalize_base("Yes, of course!");
        assert_eq!(tokens(&n).collect::<Vec<_>>(), vec!["yes", "of", "course"]);
        assert_eq!(tokens("").count(), 0);
    }
}
