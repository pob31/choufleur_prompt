//! Turning a pasted or dropped script into lines the tracker can follow.
//!
//! This is the plain-text importer. It is not the DOCX one — that is M1.2, it needs the
//! notation spec's inline-cue grammar, and the Python sidecar in `research/` is doing
//! the job until it exists. Text is worth having first anyway: it is the format everyone
//! can produce, it is what you get from pasting out of any word processor, and it is the
//! only one that works when the file you were sent is a format nothing here reads.
//!
//! What it recognises, and why each rule is here rather than a cleverer one:
//!
//! **`NOM :` prefixes**, with or without the French space. The commonest layout by far.
//!
//! **A name alone on its line**, with the speech beneath it. Detected by the line being
//! short and having no lower-case letters — a name is capitalised and a sentence is not.
//! Testing for capitals alone would swallow a shouted line, so the length cap matters.
//!
//! **Parentheticals after a name** — `NADIA, s'asseyant.` — which are manner, not
//! dialogue. The Hécube DOCX has forty of these and treating them as speech was the
//! single biggest fault in the first importer.
//!
//! **Whole lines in brackets** become stage directions. In a word processor these are
//! italic; pasted as text the italics are gone and brackets are what survives.
//!
//! **ACTE / SCÈNE / TABLEAU headings**, which give the lines their act and scene.
//!
//! Everything it is unsure about becomes dialogue attributed to whoever spoke last,
//! which is the recoverable failure: a mis-attributed line is visible on the page and
//! one click to fix, whereas a line silently dropped is one nobody will find until the
//! tracker sails past the place it should have been.

use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::store::Store;

/// What the import did, for the operator to check before trusting it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub lines: usize,
    pub stage: usize,
    pub characters: Vec<String>,
    pub scenes: usize,
    /// Lines that arrived before any speaker was named.
    pub unattributed: usize,
    /// Paragraphs skipped as blank or punctuation-only.
    pub skipped: usize,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} lines — {} spoken, {} stage direction{}",
            self.lines,
            self.lines - self.stage,
            self.stage,
            if self.stage == 1 { "" } else { "s" }
        )?;
        writeln!(
            f,
            "{} character{}: {}",
            self.characters.len(),
            if self.characters.len() == 1 { "" } else { "s" },
            self.characters.join(", ")
        )?;
        if self.scenes > 0 {
            writeln!(f, "{} scene{}", self.scenes, if self.scenes == 1 { "" } else { "s" })?;
        }
        if self.unattributed > 0 {
            writeln!(
                f,
                "{} line{} before any speaker was named — check the top of the script",
                self.unattributed,
                if self.unattributed == 1 { "" } else { "s" }
            )?;
        }
        write!(f, "nothing was dropped silently; {} blank paragraphs skipped", self.skipped)
    }
}

struct Line {
    id: String,
    act: String,
    scene: String,
    character: String,
    text: String,
    stage: bool,
}

/// Parse text into lines, without touching any file.
fn parse(text: &str) -> (Vec<Line>, Report) {
    let mut out: Vec<Line> = Vec::new();
    let mut report = Report::default();
    let mut characters: Vec<String> = Vec::new();
    let mut act = "act-1".to_string();
    let mut scene = "scene-1".to_string();
    let mut speaker = String::new();
    // A name on its own line applies to what follows, not to itself.
    let mut pending: Option<String> = None;

    for para in text.split('\n') {
        let para = para.trim();
        if para.is_empty() || para.chars().all(|c| !c.is_alphanumeric()) {
            if !para.is_empty() {
                report.skipped += 1;
            }
            continue;
        }

        if let Some((kind, label)) = heading(para) {
            match kind {
                Heading::Act => {
                    act = label;
                    // A new act restarts the scene numbering, and a heading that names
                    // no scene still ends the one before it.
                    scene = format!("{act}-scene-1");
                }
                Heading::Scene => {
                    scene = label;
                    report.scenes += 1;
                }
            }
            continue;
        }

        if let Some(inner) = bracketed(para) {
            push(&mut out, &act, &scene, "", inner, true);
            report.stage += 1;
            continue;
        }

        if let Some((name, rest)) = speaker_prefix(para) {
            remember(&mut characters, &name);
            speaker = name;
            if rest.trim().is_empty() {
                // `NADIA :` with the speech on the next line.
                pending = Some(speaker.clone());
                continue;
            }
            push(&mut out, &act, &scene, &speaker, rest.trim(), false);
            continue;
        }

        if let Some(name) = standalone_speaker(para) {
            remember(&mut characters, &name);
            speaker = name.clone();
            pending = Some(name);
            continue;
        }

        if let Some(who) = pending.take() {
            speaker = who;
        }
        if speaker.is_empty() {
            report.unattributed += 1;
        }
        push(&mut out, &act, &scene, &speaker, para, false);
    }

    report.lines = out.len();
    report.characters = characters;
    (out, report)
}

fn push(out: &mut Vec<Line>, act: &str, scene: &str, who: &str, text: &str, stage: bool) {
    out.push(Line {
        id: format!("L-{:04}", out.len() + 1),
        act: act.to_string(),
        scene: scene.to_string(),
        character: if stage { String::new() } else { slug(who) },
        text: text.to_string(),
        stage,
    });
}

fn remember(seen: &mut Vec<String>, name: &str) {
    if !name.is_empty() && !seen.iter().any(|s| s == name) {
        seen.push(name.to_string());
    }
}

enum Heading {
    Act,
    Scene,
}

/// `ACTE II`, `Scène 3`, `TABLEAU 4` — and their English spellings.
fn heading(line: &str) -> Option<(Heading, String)> {
    let folded = fold(line);
    let mut words = folded.split_whitespace();
    let first = words.next()?;
    // A heading is the whole line, not the opening of a sentence.
    if folded.split_whitespace().count() > 4 {
        return None;
    }
    let rest: String = words.collect::<Vec<_>>().join("-");
    match first {
        "acte" | "act" => Some((Heading::Act, format!("act-{}", or_next(&rest)))),
        "scene" | "tableau" | "part" => Some((Heading::Scene, format!("scene-{}", or_next(&rest)))),
        _ => None,
    }
}

fn or_next(rest: &str) -> String {
    if rest.is_empty() {
        "1".into()
    } else {
        rest.to_string()
    }
}

/// A whole line inside brackets is a stage direction.
fn bracketed(line: &str) -> Option<&str> {
    for (open, close) in [('(', ')'), ('[', ']'), ('（', '）')] {
        if let Some(inner) = line.strip_prefix(open).and_then(|l| l.strip_suffix(close)) {
            // Only when the brackets wrap everything: `Il entre (enfin) et parle` is a
            // line somebody says.
            if !inner.contains(close) {
                return Some(inner.trim());
            }
        }
    }
    None
}

/// `NADIA : bonjour` or `Nadia, s'asseyant : bonjour`.
fn speaker_prefix(line: &str) -> Option<(String, String)> {
    let at = line.find(':')?;
    let (head, rest) = line.split_at(at);
    let head = head.trim();
    // A colon deep into a sentence is punctuation, not an attribution.
    if head.is_empty() || head.chars().count() > 40 || head.split_whitespace().count() > 6 {
        return None;
    }
    let name = strip_manner(head);
    if name.is_empty() || !looks_like_name(&name) {
        return None;
    }
    Some((name, rest[1..].to_string()))
}

/// A name alone on its line.
fn standalone_speaker(line: &str) -> Option<String> {
    if line.chars().count() > 40 || line.split_whitespace().count() > 6 {
        return None;
    }
    // A sentence ends in a full stop; a name does not. `NADIA, s'asseyant.` is the
    // exception the manner-stripping below handles.
    let name = strip_manner(line);
    if name.is_empty() || !looks_like_name(&name) {
        return None;
    }
    Some(name)
}

/// `NADIA, s'asseyant.` and `GAËL (en même temps)` are the same speaker as `NADIA`.
fn strip_manner(head: &str) -> String {
    let cut = head
        .find(',')
        .into_iter()
        .chain(head.find('('))
        .min()
        .unwrap_or(head.len());
    head[..cut].trim().trim_end_matches('.').trim().to_string()
}

/// Capitalised, and not a sentence.
///
/// The test is the absence of lower-case letters rather than the presence of upper-case
/// ones, so `ÉLISSA` and `LOÏC` pass without a table of accented capitals.
fn looks_like_name(s: &str) -> bool {
    let letters: Vec<char> = s.chars().filter(|c| c.is_alphabetic()).collect();
    !letters.is_empty()
        && letters.len() <= 30
        && letters.iter().all(|c| !c.is_lowercase())
        && !s.ends_with('!')
        && !s.ends_with('?')
}

fn slug(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let folded = fold(name).replace(' ', "-");
    format!("char-{folded}")
}

fn fold(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'à' | 'â' | 'ä' | 'á' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' | 'í' => 'i',
            'ô' | 'ö' | 'ó' => 'o',
            'û' | 'ü' | 'ù' | 'ú' => 'u',
            'ç' => 'c',
            c => c,
        })
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '-')
        .collect()
}

/// Build the script document from parsed lines, keeping the show's own title.
fn document(lines: &[Line], report: &Report, title: Option<&str>, lang: &str) -> serde_json::Value {
    let characters: Vec<serde_json::Value> = report
        .characters
        .iter()
        .map(|name| {
            serde_json::json!({
                "id": slug(name),
                "name": name,
                "lang": serde_json::Value::Null,
                // Assigned in the Audio screen, not guessable from text.
                "channels": [],
            })
        })
        .collect();
    let scenes: Vec<String> = {
        let mut seen: Vec<String> = Vec::new();
        for l in lines {
            if !seen.contains(&l.scene) {
                seen.push(l.scene.clone());
            }
        }
        seen
    };
    let acts: Vec<String> = {
        let mut seen: Vec<String> = Vec::new();
        for l in lines {
            if !seen.contains(&l.act) {
                seen.push(l.act.clone());
            }
        }
        seen
    };
    serde_json::json!({
        "format": "choufleur-script",
        "formatVersion": "0.1",
        "title": title.unwrap_or("Untitled"),
        "defaultLang": [lang],
        "acts": acts,
        "scenes": scenes,
        "characters": characters,
        "lines": lines.iter().map(|l| {
            let mut o = serde_json::Map::new();
            o.insert("id".into(), l.id.clone().into());
            o.insert("act".into(), l.act.clone().into());
            o.insert("scene".into(), l.scene.clone().into());
            o.insert("character".into(), l.character.clone().into());
            o.insert("text".into(), l.text.clone().into());
            if l.stage {
                o.insert("kind".into(), "stage".into());
                // Whether a direction is read aloud is a decision, not a guess — Hécube
                // has performers who read Euripides' directions out. Written explicitly
                // so the operator can see which way round each one is and flip it.
                o.insert("spoken".into(), false.into());
            }
            serde_json::Value::Object(o)
        }).collect::<Vec<_>>(),
    })
}

/// Replace a show's script with text, snapshotting whatever was there.
///
/// Re-importing over a prepared script is the dangerous case — it discards every
/// didascalie typed, every line cut, every hold placed — so it goes through the store
/// and is refused if the snapshot cannot be written. Reattaching those decisions across
/// a re-import is the four-pass re-anchoring of notation §3.3 and is not built yet;
/// until it is, the backup is the whole safety net and it has to be real.
pub fn text(script_path: &Path, body: &str, lang: Option<&str>) -> Result<Report> {
    let (lines, report) = parse(body);
    anyhow::ensure!(report.lines > 0, "there was nothing in that text to import");

    let dir = script_path.parent().unwrap_or(Path::new("."));
    let store = Store::new(dir, vec![script_path.to_path_buf()]);
    store.arm();

    let existing: Option<serde_json::Value> = std::fs::read(script_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let title = existing
        .as_ref()
        .and_then(|d| d.get("title"))
        .and_then(|t| t.as_str())
        .map(str::to_string);
    let lang = lang
        .map(str::to_string)
        .or_else(|| {
            existing
                .as_ref()
                .and_then(|d| d.get("defaultLang"))
                .and_then(|l| l.as_array())
                .and_then(|a| a.first())
                .and_then(|l| l.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "fr".into());

    let built = document(&lines, &report, title.as_deref(), &lang);
    store.edit(script_path, |doc| {
        *doc = built;
        Ok(())
    })?;
    Ok(report)
}

/// The same, from a file on disk.
pub fn text_file(script_path: &Path, from: &Path) -> Result<Report> {
    let body = std::fs::read_to_string(from)
        .with_context(|| format!("reading {}", from.display()))?;
    text(script_path, &body, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(t: &str) -> (Vec<Line>, Report) {
        parse(t)
    }

    #[test]
    fn a_colon_attribution_is_the_common_case() {
        let (lines, r) = parsed("NADIA : Bonjour.\nÉRIC : Bonsoir.");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].character, "char-nadia");
        assert_eq!(lines[0].text, "Bonjour.");
        assert_eq!(lines[1].character, "char-eric");
        assert_eq!(r.characters, ["NADIA", "ÉRIC"]);
    }

    #[test]
    fn a_name_on_its_own_line_belongs_to_what_follows() {
        let (lines, _) = parsed("NADIA\nBonjour.\nEt bonsoir.");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.character == "char-nadia"));
        assert_eq!(lines[0].text, "Bonjour.");
    }

    #[test]
    fn manner_after_a_name_is_not_dialogue() {
        let (lines, r) = parsed("NADIA, s'asseyant.\nBonjour.\nGAËL (en même temps) : Salut.");
        assert_eq!(r.characters, ["NADIA", "GAËL"]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Bonjour.");
        assert_eq!(lines[1].character, "char-gael");
        assert_eq!(lines[1].text, "Salut.");
    }

    #[test]
    fn a_sentence_with_a_colon_is_not_an_attribution() {
        let (lines, r) = parsed(
            "NADIA : Voici ce que je pense : que nous devrions partir tout de suite.",
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(r.characters, ["NADIA"]);
        assert_eq!(lines[0].text, "Voici ce que je pense : que nous devrions partir tout de suite.");
    }

    #[test]
    fn a_shouted_line_is_not_mistaken_for_a_name() {
        // All capitals, but too long and it ends in a mark.
        let (lines, _) = parsed("NADIA : Bonjour.\nARRÊTEZ TOUT DE SUITE, JE VOUS EN SUPPLIE !");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].character, "char-nadia");
        assert!(lines[1].text.starts_with("ARRÊTEZ"));
    }

    #[test]
    fn bracketed_lines_become_stage_directions() {
        let (lines, r) = parsed("NADIA : Bonjour.\n(Elle sort.)\nIl entre (enfin) et referme.");
        assert_eq!(r.stage, 1);
        assert!(lines[1].stage);
        assert_eq!(lines[1].text, "Elle sort.");
        assert_eq!(lines[1].character, "");
        // Brackets inside a sentence are not a direction.
        assert!(!lines[2].stage);
    }

    // Scripts are written by people, under pressure, over years. A quote opens and
    // never closes; a stage direction gets pasted into the middle of a speech; a
    // parenthesis is left hanging. The importer's job is not to guess its way through
    // that — it is to leave every one of them visible and one click from being fixed.
    // These tests pin the behaviour so a later cleverness cannot quietly swallow them.

    #[test]
    fn an_unclosed_bracket_stays_dialogue_rather_than_becoming_a_direction() {
        let (lines, r) = parsed("NADIA : Bonjour.\n(Elle sort et ne revient jamais\nÉRIC : Bon.");
        assert_eq!(r.stage, 0, "an unclosed bracket is not a stage direction");
        assert_eq!(lines.len(), 3);
        // Attributed to whoever spoke last, where it is visible on the page.
        assert_eq!(lines[1].character, "char-nadia");
        assert!(lines[1].text.starts_with("(Elle sort"));
    }

    #[test]
    fn an_unclosed_quote_does_not_swallow_the_rest_of_the_script() {
        let (lines, _) = parsed(
            "SÉPHORA : « Polymestor s'avance.\nNADIA : Et alors ?\nÉRIC : Rien.",
        );
        assert_eq!(lines.len(), 3, "the quote does not run on into the next lines");
        assert_eq!(lines[1].character, "char-nadia");
        assert_eq!(lines[2].character, "char-eric");
    }

    #[test]
    fn a_direction_spliced_into_a_speech_is_left_whole_for_a_human() {
        // Splitting this would need to know that `(Elle sort.)` is not something Nadia
        // says, and getting that wrong invents a line nobody wrote. One line the
        // operator can split in the editor beats two the importer invented.
        let (lines, r) = parsed("NADIA : Bonjour. (Elle sort.) Au revoir.");
        assert_eq!(lines.len(), 1);
        assert_eq!(r.stage, 0);
        assert_eq!(lines[0].text, "Bonjour. (Elle sort.) Au revoir.");
    }

    #[test]
    fn a_speaker_named_inconsistently_stays_two_characters_to_be_merged_by_hand() {
        // `NADIA` and `NADlA` (with an ell) are one person and no rule can know it.
        // Two characters on the page is a visible problem; silently folding them on a
        // similarity guess would put a line in the wrong mouth invisibly.
        let (_, r) = parsed("NADIA : Un.\nNADLA : Deux.");
        assert_eq!(r.characters.len(), 2);
    }

    #[test]
    fn headings_set_the_act_and_scene() {
        let (lines, r) = parsed("ACTE II\nSCÈNE 3\nNADIA : Bonjour.\nSCÈNE 4\nNADIA : Encore.");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].act, "act-ii");
        assert_eq!(lines[0].scene, "scene-3");
        assert_eq!(lines[1].scene, "scene-4");
        assert_eq!(r.scenes, 2);
    }

    #[test]
    fn text_before_any_speaker_is_kept_and_counted() {
        let (lines, r) = parsed("Une salle vide.\nNADIA : Bonjour.");
        assert_eq!(lines.len(), 2, "nothing is dropped");
        assert_eq!(r.unattributed, 1);
        assert_eq!(lines[0].character, "");
    }

    #[test]
    fn ids_are_sequential_and_stable() {
        let (lines, _) = parsed("NADIA : Un.\nNADIA : Deux.\nNADIA : Trois.");
        let ids: Vec<&str> = lines.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, ["L-0001", "L-0002", "L-0003"]);
    }

    #[test]
    fn importing_over_a_script_snapshots_it_first() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("script.json");
        std::fs::write(
            &script,
            r#"{"title":"Lovedoll","defaultLang":["nl"],"lines":[{"id":"L-0001","text":"hand-prepped"}]}"#,
        )
        .unwrap();

        let r = text(&script, "NADIA : Bonjour.\n(Elle sort.)", None).unwrap();
        assert_eq!(r.lines, 2);

        let now: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&script).unwrap()).unwrap();
        assert_eq!(now["lines"][0]["text"], "Bonjour.");
        // The show keeps its own name and language across a re-import.
        assert_eq!(now["title"], "Lovedoll");
        assert_eq!(now["defaultLang"][0], "nl");
        assert_eq!(now["characters"][0]["id"], "char-nadia");

        // And the prepared script it replaced is recoverable.
        let versions: Vec<_> = std::fs::read_dir(tmp.path().join("versions"))
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(versions.len(), 1);
        let saved: serde_json::Value = serde_json::from_slice(
            &std::fs::read(versions[0].path().join("script.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["lines"][0]["text"], "hand-prepped");
    }

    #[test]
    fn empty_text_is_refused_rather_than_writing_an_empty_script() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("script.json");
        std::fs::write(&script, r#"{"lines":[{"id":"L-0001","text":"keep me"}]}"#).unwrap();
        assert!(text(&script, "\n\n   \n***\n", None).is_err());
        let now = std::fs::read_to_string(&script).unwrap();
        assert!(now.contains("keep me"));
    }
}
