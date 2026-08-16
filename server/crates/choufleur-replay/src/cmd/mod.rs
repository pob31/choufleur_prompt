pub mod eval;
pub mod make_fixture;
pub mod serve;
pub mod show;
pub mod ui;
pub mod track;
pub mod transcribe;
pub mod verify;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use choufleur_core::lang::{LangCode, NormalizerRegistry};
use choufleur_core::script::{PreparedScript, Script};

pub const DEFAULT_WHISPER_MODEL: &str = "ggml-small.bin";
pub const DEFAULT_VAD_MODEL: &str = "silero_vad.onnx";

/// Load and index a corpus's script — the step every subcommand starts from.
pub fn load_script(path: &Path) -> Result<(Script, PreparedScript)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading script {}", path.display()))?;
    let script: Script = serde_json::from_str(&text)
        .with_context(|| format!("parsing script {}", path.display()))?;
    let mut reg = NormalizerRegistry::with_defaults();
    let prepared = PreparedScript::build(&script, &mut reg);
    Ok((script, prepared))
}

/// Find a model file without making the caller think about where they are.
///
/// An explicit `--model` always wins. Otherwise look in `$CHOUFLEUR_MODELS`, then
/// in the places the repository actually puts them, so the command works the same
/// from the workspace root and from `server/`.
pub fn resolve_model(explicit: Option<&Path>, filename: &str) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.exists() {
            anyhow::bail!("{} not found", p.display());
        }
        return Ok(p.to_path_buf());
    }
    let mut tried = Vec::new();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("CHOUFLEUR_MODELS") {
        candidates.push(PathBuf::from(dir).join(filename));
    }
    candidates.push(PathBuf::from("models").join(filename));
    candidates.push(PathBuf::from("server/models").join(filename));
    candidates.push(PathBuf::from("../models").join(filename));
    if let Ok(exe) = std::env::current_exe() {
        // target/release/choufleur-replay -> ../../../models
        if let Some(root) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            candidates.push(root.join("models").join(filename));
        }
    }
    for c in candidates {
        if c.exists() {
            return Ok(c);
        }
        tried.push(c.display().to_string());
    }
    anyhow::bail!(
        "could not find {filename}. Run scripts/fetch-models.sh, pass an explicit \
         path, or set CHOUFLEUR_MODELS.\nLooked in:\n  {}",
        tried.join("\n  ")
    )
}

/// Every language each character speaks, most-used first.
///
/// The script is the authority on language, never audio detection (PRD, *Language
/// comes from the script, not detection*). Taking only a character's *first*
/// language was a real bug caught by real material: an actor who opens a monologue
/// in Dutch and then quotes Hamlet in English is one channel with two languages,
/// and forcing Dutch on the English made Whisper *translate* it — "I am thy
/// father's spirit" came back as "Ik ben jouw vader Spirit", fluent and wrong,
/// with no error anywhere to notice.
/// A language must account for at least this share of a character's lines before it
/// is offered as a decode candidate for their whole channel.
///
/// Offering a language is not free. On marginal audio Whisper is *more* confident
/// producing familiar English filler — "Thank you.", "Uh..." — than producing correct
/// French, so a candidate set containing English wins the confidence comparison and
/// the transcript fills with hallucinations. Measured: two lines of a 44-line French
/// scene were mis-tagged English by the script converter, which was enough to put 23
/// invented English segments into one transcript.
///
/// The genuinely rare foreign line is not lost by this. `track --from-audio` decodes
/// against the language of the line it *expects*, so a single English line inside a
/// French scene is still decoded as English when the tracker reaches it — the share
/// rule only governs what is offered blindly, with no idea where the show is.
const MIN_LANGUAGE_SHARE: f64 = 0.15;

pub fn character_languages(script: &PreparedScript) -> HashMap<String, Vec<LangCode>> {
    let mut counts: HashMap<String, Vec<(LangCode, usize)>> = HashMap::new();
    for line in &script.lines {
        for lang in line.langs() {
            let entry = counts.entry(line.character.clone()).or_default();
            match entry.iter_mut().find(|(l, _)| l == lang) {
                Some((_, n)) => *n += 1,
                None => entry.push((lang.clone(), 1)),
            }
        }
    }
    counts
        .into_iter()
        .map(|(ch, mut v)| {
            // Most-used first, so a single-language caller taking `.first()` gets
            // the character's usual language rather than an accident of ordering.
            v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let total: usize = v.iter().map(|(_, n)| n).sum();
            let langs: Vec<LangCode> = v
                .iter()
                .enumerate()
                .filter(|(i, (_, n))| {
                    // Always keep the most common one, whatever its share.
                    *i == 0 || *n as f64 / total.max(1) as f64 >= MIN_LANGUAGE_SHARE
                })
                .map(|(_, (l, _))| l.clone())
                .collect();
            (ch, langs)
        })
        .collect()
}

/// Every language appearing anywhere in the script, in first-seen order.
pub fn languages_used(script: &PreparedScript) -> Vec<LangCode> {
    let mut out: Vec<LangCode> = Vec::new();
    for line in &script.lines {
        for l in line.langs() {
            if !out.contains(l) {
                out.push(l.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_model_explains_where_it_looked() {
        let err = resolve_model(None, "definitely-not-a-model.bin").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("fetch-models.sh"), "{msg}");
        assert!(msg.contains("Looked in"), "{msg}");
    }

    #[test]
    fn an_explicit_path_that_does_not_exist_is_an_error_not_a_fallback() {
        let err = resolve_model(Some(Path::new("/nope/model.bin")), "ggml-small.bin").unwrap_err();
        assert!(format!("{err}").contains("/nope/model.bin"));
    }
}
