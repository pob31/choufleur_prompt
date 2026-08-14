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

/// Per-show constant prompt: the title and the character names, which are the
/// proper nouns Whisper is most likely to mangle.
pub fn static_prompt(title: Option<&str>, character_names: &[String]) -> String {
    let mut s = String::new();
    if let Some(t) = title {
        s.push_str(t);
        s.push_str(". ");
    }
    if !character_names.is_empty() {
        s.push_str(&character_names.join(", "));
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
        let mk = |id: &str, ch: &str, text: &str| ScriptLine {
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
