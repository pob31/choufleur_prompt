//! Checking a script somebody else prepared.
//!
//! Prep is judgement work — who the chorus is, which passages are word lists, where a
//! didascalie stops and the dialogue starts — and the heuristic importer is deliberately
//! bad at it. The operator's answer is to hand that judgement to an AI, at prep time,
//! with the rules written down: *"Maybe we need to have the possibility to prep with an
//! AI either through MCP or plain file generation given the rules of the display."*
//!
//! That is a good division of labour and it needs one thing to be safe. A model reading
//! a five-hundred-paragraph script will occasionally lose a paragraph — summarise two
//! into one, skip a fragment it could not classify, tidy a line it thought was a typo —
//! and every one of those is the invisible failure. A wrong line is on the page and
//! costs ten seconds. A missing line is not, and the tracker sails past the place it
//! should have been.
//!
//! So the output is checked against the source it came from, mechanically, and the two
//! questions that matter are asked first:
//!
//! - **Is every paragraph of the source somewhere in the script?**
//! - **Did anything appear that was not in the source?**
//!
//! Everything else here — ids, characters, holds — is ordinary schema hygiene. Those
//! two are the reason the module exists, and they are equally worth asking of a human's
//! work, or of the heuristic importer's.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};
use choufleur_core::normalize::normalize_base;

/// Something wrong with a prepared script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub severity: Severity,
    pub what: String,
    /// The line it is about, where there is one.
    pub line: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The script is unusable, or text has gone missing.
    Fatal,
    /// Worth a look before the show.
    Warn,
}

#[derive(Debug, Clone, Default)]
pub struct Check {
    pub problems: Vec<Problem>,
    pub lines: usize,
    pub covered: usize,
    pub source_paragraphs: usize,
}

impl Check {
    pub fn fatal(&self) -> usize {
        self.problems.iter().filter(|p| p.severity == Severity::Fatal).count()
    }
    pub fn ok(&self) -> bool {
        self.fatal() == 0
    }
    fn add(&mut self, severity: Severity, what: impl Into<String>, line: Option<&str>) {
        self.problems.push(Problem {
            severity,
            what: what.into(),
            line: line.map(str::to_string),
        });
    }
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} lines", self.lines)?;
        if self.source_paragraphs > 0 {
            writeln!(
                f,
                "{} of {} source paragraphs accounted for",
                self.covered, self.source_paragraphs
            )?;
        }
        let mut sorted = self.problems.clone();
        sorted.sort_by_key(|p| p.severity);
        for p in sorted.iter().take(40) {
            let tag = match p.severity {
                Severity::Fatal => "REFUSED",
                Severity::Warn => "check  ",
            };
            match &p.line {
                Some(l) => writeln!(f, "  {tag}  {l:<10} {}", p.what)?,
                None => writeln!(f, "  {tag}  {:<10} {}", "", p.what)?,
            }
        }
        if self.problems.len() > 40 {
            writeln!(f, "  … and {} more", self.problems.len() - 40)?;
        }
        write!(
            f,
            "{}",
            if self.ok() {
                "nothing missing; safe to import".to_string()
            } else {
                format!("{} fatal — not safe to import", self.fatal())
            }
        )
    }
}

/// Check a prepared script, optionally against the text it was made from.
pub fn script(path: &Path, source: Option<&str>) -> Result<Check> {
    let doc: serde_json::Value = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {} — is it valid JSON?", path.display()))?;

    let mut check = Check::default();

    // The first question, and the one this module somehow did not ask: does the file
    // load as a script at all?
    //
    // Everything below inspects a `serde_json::Value`, which is right for the content
    // checks — it lets a field this build does not model pass through untouched. But it
    // also means a file can satisfy every one of them and still be refused by the thing
    // that has to read it. That is not hypothetical: the text importer wrote `acts` and
    // `scenes` as bare strings where the type wants `{id, title}`, and every show it
    // made passed this check, listed cleanly, and died on startup with
    // `invalid type: string "act-1", expected struct SectionMeta` before it could bind
    // a port. A check that passes a script the show cannot open is worse than no check.
    if let Err(e) = serde_json::from_value::<choufleur_core::script::Script>(doc.clone()) {
        check.add(
            Severity::Fatal,
            format!("this does not load as a script: {e}"),
            None,
        );
    }

    let Some(lines) = doc.get("lines").and_then(|l| l.as_array()) else {
        check.add(Severity::Fatal, "the file has no `lines` array", None);
        return Ok(check);
    };
    check.lines = lines.len();
    if lines.is_empty() {
        check.add(Severity::Fatal, "the script has no lines at all", None);
    }

    let declared: HashSet<&str> = doc
        .get("characters")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|c| c.get("id").and_then(|i| i.as_str()))
                .collect()
        })
        .unwrap_or_default();

    // Members must name characters that exist, or the chorus resolves to nobody and
    // silently stops matching on every one of their channels.
    if let Some(chars) = doc.get("characters").and_then(|c| c.as_array()) {
        for c in chars {
            let id = c.get("id").and_then(|i| i.as_str()).unwrap_or("?");
            for m in c.get("members").and_then(|m| m.as_array()).unwrap_or(&vec![]) {
                if let Some(m) = m.as_str() {
                    if !declared.contains(m) {
                        check.add(
                            Severity::Fatal,
                            format!("{id} lists a member `{m}` that is not a character"),
                            None,
                        );
                    }
                }
            }
        }
    }

    let mut ids: HashSet<&str> = HashSet::new();
    for (i, l) in lines.iter().enumerate() {
        let id = l.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let at = if id.is_empty() { format!("#{}", i + 1) } else { id.to_string() };
        if id.is_empty() {
            check.add(Severity::Fatal, "line has no id", Some(&at));
        } else if !ids.insert(id) {
            // Two lines with one id means every cue anchored to it lands on whichever
            // comes first, for ever.
            check.add(Severity::Fatal, "id used more than once", Some(&at));
        }

        let text = l.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if text.trim().is_empty() {
            check.add(Severity::Warn, "line has no text", Some(&at));
        }

        match l.get("character").and_then(|v| v.as_str()) {
            None | Some("") => {}
            Some(c) if declared.contains(c) => {}
            // A warning, not a refusal. Hécube's own prepared script names 23 speakers
            // it never declares, on 81 lines — mostly shared ones like
            // `char-elissa,-eric,-sephora,-gael` — and it runs. The cost is real but
            // bounded: an undeclared speaker cannot be patched to a microphone and
            // earns no confidence from agreeing, which on a zone-mic show is nothing at
            // all. Refusing a script that works would make the check something people
            // learn to skip.
            Some(c) => check.add(
                Severity::Warn,
                format!("speaker `{c}` is not in `characters` — it cannot be patched to a mic"),
                Some(&at),
            ),
        }

        let kind = l.get("kind").and_then(|v| v.as_str()).unwrap_or("dialogue");
        if !matches!(kind, "dialogue" | "stage") {
            check.add(Severity::Fatal, format!("unknown kind `{kind}`"), Some(&at));
        }

        if let Some(h) = l.get("hold").and_then(|v| v.as_str()) {
            if !matches!(h, "silence" | "music" | "adlib") {
                check.add(Severity::Fatal, format!("unknown hold `{h}`"), Some(&at));
            }
            // A hold means "stop looking for text here". Saying the same line is also
            // spoken asks the tracker to match something it has been told to ignore.
            if l.get("spoken").and_then(|v| v.as_bool()) == Some(true) {
                check.add(
                    Severity::Warn,
                    "carries a hold but is marked spoken — the hold wins, and it will never match",
                    Some(&at),
                );
            }
        }
    }

    thin_characters(&mut check, lines, &declared);

    if let Some(src) = source {
        coverage(&mut check, lines, src);
    }
    Ok(check)
}

/// Characters holding almost nothing, which is how a mis-parse looks from here.
///
/// *Lazzi* is a two-hander — Philippe and Vincent, five hundred lines each — and its
/// prepared script declares seven characters. The other five are manners that became
/// people: `PHILIPPE(DISPARAISSANT)`, `VINCENT-(VOIX-OFF)`, `VOIX-DE-PHILIPPE`. Each
/// holds one or two lines, and each of those lines is now attributed to somebody who
/// cannot be patched to a microphone.
///
/// The importer already reports this on the scripts it makes itself; a script that
/// arrived some other way — from a preparer, from an AI, from last year — deserves the
/// same question. Reported and never merged: whether `VOIX-DE-PHILIPPE` is Philippe
/// off-stage or a separate voice is a fact about the production.
fn thin_characters(check: &mut Check, lines: &[serde_json::Value], declared: &HashSet<&str>) {
    // Meaningless on a short script, where everybody has one line.
    if lines.len() < 40 {
        return;
    }
    let mut held: HashMap<&str, usize> = declared.iter().map(|d| (*d, 0)).collect();
    for l in lines {
        if let Some(c) = l.get("character").and_then(|v| v.as_str()) {
            if let Some(n) = held.get_mut(c) {
                *n += 1;
            }
        }
    }
    let mut thin: Vec<(&str, usize)> = held.into_iter().filter(|(_, n)| *n <= 2).collect();
    thin.sort();
    for (id, n) in thin {
        check.add(
            Severity::Warn,
            format!(
                "`{id}` has {n} line{} — often a manner or a mode that became a person",
                if n == 1 { "" } else { "s" }
            ),
            None,
        );
    }
}

/// The question that matters: is all the source text still here?
///
/// Compared on normalised text, so punctuation and capitalisation may be reworked, but
/// a paragraph that has been summarised, merged away or quietly dropped shows up.
/// Matching is many-to-one in both directions — a paragraph may legitimately appear
/// once — and order is not required, because a preparer may reasonably move a section
/// title above the lines it introduces.
fn coverage(check: &mut Check, lines: &[serde_json::Value], source: &str) {
    let paragraphs: Vec<&str> = source
        .lines()
        .map(str::trim)
        .filter(|p| !p.is_empty() && p.chars().any(char::is_alphanumeric))
        .collect();
    check.source_paragraphs = paragraphs.len();

    let mut have: HashMap<String, usize> = HashMap::new();
    for l in lines {
        let key = normalize_base(l.get("text").and_then(|v| v.as_str()).unwrap_or(""));
        if !key.is_empty() {
            *have.entry(key).or_default() += 1;
        }
    }

    let mut missing = 0usize;
    for p in &paragraphs {
        let key = normalize_base(p);
        if key.is_empty() {
            check.covered += 1;
            continue;
        }
        if have.contains_key(&key) {
            check.covered += 1;
            continue;
        }
        // Not present whole. A preparer is allowed to have split a paragraph the source
        // ran together, so accept it if the text is accounted for in pieces.
        if split_across(&key, &have) {
            check.covered += 1;
            continue;
        }
        missing += 1;
        if missing <= 12 {
            check.add(
                Severity::Fatal,
                format!("source text is not in the script: {:?}", snippet(p)),
                None,
            );
        }
    }
    if missing > 12 {
        check.add(
            Severity::Fatal,
            format!("{} more source paragraphs are missing", missing - 12),
            None,
        );
    }

    // The other direction, as a warning rather than a refusal: a preparer may add a
    // section title the source only implied, and that is useful. Wholesale invention is
    // not, and this is where it shows.
    let source_keys: HashSet<String> = paragraphs.iter().map(|p| normalize_base(p)).collect();
    let invented = have
        .keys()
        .filter(|k| !source_keys.contains(*k) && !source_keys.iter().any(|s| s.contains(*k)))
        .count();
    if invented > 0 {
        check.add(
            Severity::Warn,
            format!("{invented} line(s) have text that is not in the source"),
            None,
        );
    }
}

/// Is this paragraph present as a run of consecutive shorter lines?
fn split_across(key: &str, have: &HashMap<String, usize>) -> bool {
    let mut rest = key;
    let mut steps = 0;
    while !rest.is_empty() && steps < 64 {
        let taken = have
            .keys()
            .filter(|k| !k.is_empty() && rest.starts_with(k.as_str()))
            .max_by_key(|k| k.len());
        let Some(t) = taken else { return false };
        rest = rest[t.len()..].trim_start();
        steps += 1;
    }
    rest.is_empty()
}

fn snippet(s: &str) -> String {
    s.chars().take(56).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a fixture, filling in the envelope a real script always has.
    ///
    /// Without this every fixture fails the type check on boilerplate and none of them
    /// reach the content checks they were written to exercise.
    fn write(dir: &Path, mut doc: serde_json::Value) -> std::path::PathBuf {
        let o = doc.as_object_mut().unwrap();
        o.entry("defaultLang").or_insert(serde_json::json!(["fr"]));
        o.entry("characters").or_insert(serde_json::json!([]));
        let p = dir.join("script.json");
        std::fs::write(&p, serde_json::to_string(&doc).unwrap()).unwrap();
        p
    }

    fn good() -> serde_json::Value {
        serde_json::json!({
            "characters": [{"id": "char-nadia", "name": "NADIA"}],
            "lines": [
                {"id": "L-0001", "act": "a", "scene": "s",
                 "character": "char-nadia", "text": "Bonjour."},
                {"id": "L-0002", "act": "a", "scene": "s",
                 "character": "", "text": "Elle sort.", "kind": "stage",
                 "spoken": false},
            ],
        })
    }

    #[test]
    fn a_file_the_show_cannot_open_is_refused_however_tidy_it_looks() {
        let tmp = tempfile::tempdir().unwrap();
        // Every content check below passes on this. Only the type refuses it.
        let doc = serde_json::json!({
            "characters": [],
            "acts": ["act-1"],
            "scenes": ["scene-1"],
            "lines": [{"id": "L-0001", "act": "act-1", "scene": "scene-1",
                       "character": "", "text": "Bonjour."}],
        });
        let c = script(&write(tmp.path(), doc), None).unwrap();
        assert!(!c.ok(), "{c}");
        assert!(c.problems.iter().any(|p| p.what.contains("does not load as a script")));
    }

    #[test]
    fn a_sound_script_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let c = script(&write(tmp.path(), good()), Some("Bonjour.\nElle sort.")).unwrap();
        assert!(c.ok(), "{c}");
        assert_eq!(c.covered, 2);
    }

    #[test]
    fn a_dropped_paragraph_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let c = script(
            &write(tmp.path(), good()),
            Some("Bonjour.\nUne phrase que personne n'a gardée.\nElle sort."),
        )
        .unwrap();
        assert!(!c.ok(), "a missing paragraph must be fatal");
        assert_eq!(c.covered, 2);
        assert!(c.problems.iter().any(|p| p.what.contains("personne n'a gardée")));
    }

    #[test]
    fn punctuation_and_case_may_be_reworked() {
        let tmp = tempfile::tempdir().unwrap();
        let c = script(&write(tmp.path(), good()), Some("bonjour\n« Elle sort ! »")).unwrap();
        assert!(c.ok(), "{c}");
    }

    #[test]
    fn a_paragraph_split_into_several_lines_still_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({
            "characters": [],
            "lines": [
                {"id": "L-1", "act": "a", "scene": "s", "character": "", "text": "Bonjour."},
                {"id": "L-2", "act": "a", "scene": "s", "character": "", "text": "Au revoir."},
            ],
        });
        let c = script(&write(tmp.path(), doc), Some("Bonjour. Au revoir.")).unwrap();
        assert!(c.ok(), "{c}");
    }

    #[test]
    fn invented_text_is_a_warning_not_a_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({
            "characters": [],
            "lines": [
                {"id": "L-1", "act": "a", "scene": "s", "character": "", "text": "Bonjour."},
                {"id": "L-2", "act": "a", "scene": "s", "character": "",
                 "text": "3. A SECTION TITLE", "kind": "stage", "spoken": false},
            ],
        });
        let c = script(&write(tmp.path(), doc), Some("Bonjour.")).unwrap();
        assert!(c.ok(), "adding a title is allowed: {c}");
        assert!(c.problems.iter().any(|p| p.severity == Severity::Warn));
    }

    #[test]
    fn a_repeated_id_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({
            "characters": [],
            "lines": [
                {"id": "L-1", "act": "a", "scene": "s", "character": "", "text": "Un."},
                {"id": "L-1", "act": "a", "scene": "s", "character": "", "text": "Deux."},
            ],
        });
        let c = script(&write(tmp.path(), doc), None).unwrap();
        assert!(!c.ok());
        assert!(c.problems.iter().any(|p| p.what.contains("more than once")));
    }

    #[test]
    fn a_speaker_who_is_not_a_character_is_flagged_not_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({
            "characters": [{"id": "char-nadia", "name": "NADIA"}],
            "lines": [{"id": "L-1", "act": "a", "scene": "s",
                       "character": "char-eric", "text": "Un."}],
        });
        let c = script(&write(tmp.path(), doc), None).unwrap();
        // Hécube's own prepared script names 23 speakers it never declares, on 81
        // lines, and it runs. Refusing it would make the check something people skip.
        assert!(c.ok(), "{c}");
        assert!(c.problems.iter().any(|p| p.what.contains("char-eric")
            && p.severity == Severity::Warn));
    }

    #[test]
    fn a_chorus_naming_nobody_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({
            "characters": [
                {"id": "char-choeur", "name": "LE CHŒUR", "members": ["char-ghost"]}
            ],
            "lines": [{"id": "L-1", "act": "a", "scene": "s",
                       "character": "char-choeur", "text": "Un."}],
        });
        let c = script(&write(tmp.path(), doc), None).unwrap();
        assert!(!c.ok());
        assert!(c.problems.iter().any(|p| p.what.contains("char-ghost")));
    }

    #[test]
    fn a_hold_that_is_also_spoken_is_flagged() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({
            "characters": [],
            "lines": [{"id": "L-1", "act": "a", "scene": "s", "character": "",
                       "text": "Musique.", "kind": "stage",
                       "hold": "music", "spoken": true}],
        });
        let c = script(&write(tmp.path(), doc), None).unwrap();
        assert!(c.ok(), "a contradiction is worth seeing, not worth refusing");
        assert!(c.problems.iter().any(|p| p.what.contains("never match")));
    }

    #[test]
    fn nonsense_is_reported_rather_than_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("script.json");
        std::fs::write(&p, r#"{"title":"no lines here"}"#).unwrap();
        let c = script(&p, None).unwrap();
        assert!(!c.ok());
    }
}
