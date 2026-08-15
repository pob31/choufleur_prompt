//! Decode-biasing prompts.
//!
//! Whisper's `initial_prompt` conditions the decoder on text it treats as
//! *preceding* context. Feeding it the lines we expect next is a vocabulary and
//! style hint, not a claim about what was said — and it is the single cheapest
//! accuracy win available, because Choufleur aligns against a known script rather
//! than transcribing the unknown (PRD, *ASR Engine and Latency Budget*).
//!
//! The hazard is symmetric: a prompt full of expected text makes Whisper more
//! likely to *produce* that text on noise. That is why the hallucination filter
//! runs regardless of bias mode and why `confident-wrong` is the metric that
//! decides whether biasing earns its place.
//!
//! Prompts carry pristine text, never normalized text — Whisper wants natural
//! language with its punctuation intact.

use serde::{Deserialize, Serialize};

use crate::lang::LangCode;
use crate::script::PreparedScript;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BiasMode {
    /// No prompt at all — the control condition for the M0.4 sweep.
    None,
    /// A per-show constant: title and character names. Cheap, no feedback loop.
    Static,
    /// The lines the tracker expects next. Strongest, and the one that can bite.
    Tracker,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PromptConfig {
    /// How many upcoming lines to include.
    pub lines_ahead: usize,
    /// Hard cap on prompt length. whisper.cpp truncates the prompt to the last
    /// 224 tokens of context; staying well under that keeps the imminent lines
    /// from being the ones dropped.
    pub max_chars: usize,
}

impl Default for PromptConfig {
    fn default() -> Self {
        PromptConfig {
            lines_ahead: 6,
            max_chars: 600,
        }
    }
}

/// The show's own proper nouns, most-used first.
///
/// These are simultaneously the best landmarks a matcher could ask for and the words
/// the recogniser is worst at, because a language model asked to transcribe a name it
/// has no expectation of will reach for a common word that sounds like it. Measured
/// on one performance of *Hécube, pas Hécube*, where the title character is named 80
/// times in the script:
///
/// ```text
/// Hécube    -> heures x8, cubes x7, écube x3, écoute x2
/// Polyxène  -> problème x14, violence x5
/// Euripide  -> épisode x8, rapide x6, l'équipe x3, frigide
/// Agamemnon -> gamemnon, ramèneront, menton
/// ```
///
/// Not one correct instance of "Hécube" in two hours. The score therefore collapses
/// on exactly the lines that ought to be unmistakable, so these words are worth more
/// prompt budget than anything else available.
///
/// Capitalised mid-sentence is the test: it finds names without needing a dictionary
/// of the language, which matters because the corpus is French, Dutch and English.
pub fn proper_nouns(lines: &[crate::script::PreparedLine], max: usize) -> Vec<String> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for line in lines {
        let mut at_start = true;
        for word in line.text.split_whitespace() {
            let bare: String = word
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '\u{2019}')
                .to_string();
            let starts_upper = bare.chars().next().is_some_and(|c| c.is_uppercase());
            // A word that opens a sentence is capitalised for grammar, not identity.
            if !at_start && starts_upper && bare.chars().count() > 2 {
                *seen.entry(bare.clone()).or_default() += 1;
            }
            at_start = word.ends_with(['.', '!', '?', '…', ':']) || word.ends_with("»");
        }
    }
    let mut v: Vec<(String, usize)> = seen.into_iter().collect();
    // Frequency first, then alphabetical, so the prompt is stable across runs.
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.into_iter().take(max).map(|(w, _)| w).collect()
}

/// Per-show constant prompt: the title, the character names, and the show's own
/// proper nouns — the words Whisper is most likely to mangle.
///
/// Kept short deliberately. whisper.cpp gives the prompt a few hundred tokens at
/// most, and a prompt long enough to crowd that budget starts steering the decode
/// toward whatever it contains, which on a quiet channel is how a recogniser is
/// invited to recite rather than listen.
pub fn static_prompt(title: Option<&str>, character_names: &[String]) -> String {
    static_prompt_with(title, character_names, &[])
}

pub fn static_prompt_with(
    title: Option<&str>,
    character_names: &[String],
    proper: &[String],
) -> String {
    let mut s = String::new();
    if let Some(t) = title {
        s.push_str(t);
        s.push_str(". ");
    }
    if !character_names.is_empty() {
        s.push_str(&character_names.join(", "));
        s.push('.');
    }
    if !proper.is_empty() {
        s.push(' ');
        s.push_str(&proper.join(", "));
        s.push('.');
    }
    s
}

/// The lines the tracker expects to hear next, in the requested language.
///
/// When `character` is given (a per-actor channel), that character's upcoming
/// lines come first — they are what this channel will actually carry — followed
/// by the surrounding dialogue for context.
pub fn tracker_prompt(
    script: &PreparedScript,
    position: usize,
    lang: &LangCode,
    character: Option<&str>,
    cfg: &PromptConfig,
) -> String {
    let end = (position + cfg.lines_ahead * 2).min(script.len());
    let mut picked: Vec<&str> = Vec::new();

    // Two passes: this channel's own upcoming lines first, then the surrounding
    // dialogue as context, each capped at `lines_ahead` total.
    for pass in 0..2 {
        for i in position..end {
            if picked.len() >= cfg.lines_ahead {
                break;
            }
            let l = &script.lines[i];
            let wanted = match (pass, character) {
                (0, Some(c)) => l.character == c,
                (0, None) => continue,
                _ => true,
            };
            if !wanted || l.text.trim().is_empty() || !l.langs().any(|x| x == lang) {
                continue;
            }
            if !picked.contains(&l.text.as_str()) {
                picked.push(&l.text);
            }
        }
    }

    // Truncate from the front: whisper keeps the *tail* of an over-long prompt,
    // so the most imminent lines must be last.
    let mut out = String::new();
    for text in picked.iter().rev() {
        if out.len() + text.len() + 1 > cfg.max_chars {
            break;
        }
        out.insert_str(0, &format!("{text} "));
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::NormalizerRegistry;
    use crate::script::{Character, Script, ScriptLine};

    fn script() -> Script {
        let mk = |id: &str, ch: &str, text: &str| ScriptLine { cut: false,
            spoken: None,
            kind: Default::default(),
            hold: None,
            hold_seconds: None,
            id: id.into(),
            act: "act-1".into(),
            scene: "sc-1".into(),
            character: ch.into(),
            text: text.into(),
            lang: None,
            landmark: 0,
            alternates: Vec::new(),
        };
        Script {
            format: "choufleur-script".into(),
            format_version: "0.1".into(),
            title: Some("Toy".into()),
            default_lang: vec![LangCode::new("fr")],
            acts: vec![],
            scenes: vec![],
            characters: vec![Character {
                id: "char-a".into(),
                name: "A".into(),
                lang: None,
                channels: vec![1],
            }],
            lines: vec![
                mk("L-0001", "char-a", "Bonjour."),
                mk("L-0002", "char-b", "Salut."),
                mk("L-0003", "char-a", "Alors pars."),
            ],
        }
    }

    #[test]
    fn tracker_prompt_lists_upcoming_lines() {
        let s = script();
        let mut reg = NormalizerRegistry::with_defaults();
        let p = PreparedScript::build(&s, &mut reg);
        let out = tracker_prompt(&p, 0, &LangCode::new("fr"), None, &PromptConfig::default());
        assert!(out.contains("Bonjour."), "{out}");
        assert!(out.contains("Alors pars."), "{out}");
    }

    #[test]
    fn character_lines_lead_but_context_follows() {
        let s = script();
        let mut reg = NormalizerRegistry::with_defaults();
        let p = PreparedScript::build(&s, &mut reg);
        let out = tracker_prompt(
            &p,
            0,
            &LangCode::new("fr"),
            Some("char-a"),
            &PromptConfig::default(),
        );
        assert!(out.contains("Bonjour."));
        assert!(out.contains("Alors pars."));
        assert!(out.contains("Salut."));
    }

    #[test]
    fn respects_the_length_cap_keeping_the_imminent_end() {
        let s = script();
        let mut reg = NormalizerRegistry::with_defaults();
        let p = PreparedScript::build(&s, &mut reg);
        let cfg = PromptConfig {
            lines_ahead: 6,
            max_chars: 20,
        };
        let out = tracker_prompt(&p, 0, &LangCode::new("fr"), None, &cfg);
        assert!(out.len() <= 20, "{out:?}");
    }

    #[test]
    fn static_prompt_is_a_per_show_constant() {
        let names = vec!["MARIE".to_string(), "JEAN".to_string()];
        assert_eq!(
            static_prompt(Some("La Mouette"), &names),
            "La Mouette. MARIE, JEAN."
        );
    }
}
