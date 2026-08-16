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
    /// Characters with a single line to their name.
    ///
    /// Almost always a misread: a didascalie taken for a speaker, or a name spelled two
    /// ways. Reported rather than corrected, because the fix is a judgement — merge,
    /// rename or retype — and the importer guessing is how the mistake became invisible
    /// in the first place.
    pub suspect: Vec<String>,
    /// Speakers that name more than one person — `LE CHŒUR et NADIA`.
    ///
    /// Listed, never split. Whether that is two characters sharing a line or one chorus
    /// that happens to include her is a fact about this production, and the operator
    /// said it plainly: *"this is real specific to the script of this show. No hard
    /// rules here. It's not a csv."* So the importer keeps them whole and hands over a
    /// checklist instead of a guess.
    pub shared: Vec<String>,
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
        if !self.suspect.is_empty() {
            writeln!(
                f,
                "check {}: {} — one line each, which usually means a stage direction \
                 read as a name, or a name spelled two ways",
                if self.suspect.len() == 1 { "this speaker" } else { "these speakers" },
                self.suspect.join(", ")
            )?;
        }
        if !self.shared.is_empty() {
            writeln!(
                f,
                "{} shared line{}: {} — kept whole, retouch to whoever actually speaks",
                self.shared.len(),
                if self.shared.len() == 1 { "" } else { "s" },
                self.shared.join(", ")
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

        // A numbered section — `3.TOUGH COOKIES (TEXT boys)`. *Lovedoll* is built
        // entirely this way: forty-one numbered parts, no acts, no scenes, and not one
        // speaker attribution in five hundred and ninety-nine paragraphs. The heading
        // is what says who is on and what is playing.
        //
        // It is kept on the page as a stage direction as well as ending the section
        // before it, because that title is the only structure the operator has to
        // navigate by. Recognised before the speaker rules, which would otherwise read
        // `1.AM I A HUMAN (Presenting everyone)` as a character called `1.AM I A HUMAN`
        // — capitals, short, parenthetical stripped as manner. The same fault as the
        // capitalised didascalie, wearing a number.
        if let Some(n) = numbered_section(para) {
            scene = format!("scene-{n}");
            report.scenes += 1;
            push(&mut out, &act, &scene, "", para, true);
            report.stage += 1;
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
            // Kept on the page, like a numbered section. It used to be consumed, and
            // the coverage check found what that costs: Lovedoll has a paragraph
            // reading `Act` and it simply disappeared. A heading is text somebody put
            // in the script, and text is never dropped.
            push(&mut out, &act, &scene, "", para, true);
            report.stage += 1;
            continue;
        }

        // A didascalie set in capitals reads as a name to any rule that looks for
        // capitals, and then it becomes a *character* — after which the speech beneath
        // it is attributed to somebody called SILENCE, and the real speaker's name is
        // left sitting in the dialogue. This is the fault the operator reported from
        // the first Hécube import: *"the names turning into dialogue… probably caused
        // by a didascalie that threw off the importer."* The set-piece words are few
        // and known, so they are named rather than guessed at.
        if is_set_piece(para) {
            push(&mut out, &act, &scene, "", para.trim_end_matches('.'), true);
            report.stage += 1;
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
    // Only worth saying on a script long enough for "one line" to be unusual. On a
    // ten-line excerpt everybody has one line and the warning is noise, which is how a
    // useful warning gets ignored on the script where it matters.
    report.suspect = characters
        .iter()
        .filter(|_| out.len() >= 40)
        .filter(|name| {
            let id = slug(name);
            out.iter().filter(|l| l.character == id).count() <= 1
        })
        .cloned()
        .collect();
    report.shared = characters
        .iter()
        .filter(|name| {
            name.split_whitespace()
                .any(|w| JOINERS.contains(&fold(w).trim()))
        })
        .cloned()
        .collect();
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

/// Directions that stand alone as a whole line, capitalised or not.
///
/// Matched on the whole line after folding, never as a substring — `Silence, mes amies`
/// is a line Éric says twenty times, and searching for the word inside would file every
/// one of them as a stage direction.
fn is_set_piece(line: &str) -> bool {
    matches!(
        fold(line.trim_end_matches(['.', '!'])).trim(),
        "silence" | "long silence" | "un temps" | "un temps long" | "pause"
            | "noir" | "blackout" | "fin" | "entracte" | "musique" | "obscurite"
    )
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

/// `3.TOUGH COOKIES (TEXT boys)` — a numbered part, and the number.
///
/// Deliberately narrow: digits, then a dot or bracket, then a real title. A line of
/// dialogue almost never opens that way, and when one does the operator sees a stage
/// direction on the page and retypes it, which is a visible mistake rather than a
/// silent one.
fn numbered_section(line: &str) -> Option<String> {
    let digits: String = line.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let rest = line[digits.len()..].trim_start();
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    (rest.trim().chars().count() >= 3).then_some(digits)
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
    // A colon is strong evidence on its own, so the name test is allowed to be lenient
    // here in a way the standalone one is not. A single stray lower-case letter — `NADlA`
    // for `NADIA`, an ell for a one, the commonest scanning slip there is — otherwise
    // fails the test, and the entire line including the name lands in the script as
    // dialogue spoken by whoever came before. That is the fault reported from the first
    // Hécube import, and it is silent: the name is simply *in* the text.
    if name.is_empty() || !(looks_like_name(&name) || mostly_capitals(&name)) {
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
    // A single letter alone on a line is text, not a name — somebody is spelling
    // something out. *Lovedoll* has `S` / `O` / `S` on three consecutive lines, and
    // another run of `B` `F` `A` `K`, and reading them as speakers cost 344 lines:
    // a false speaker does not merely mis-file its own line, it becomes the speaker
    // for every line after it until the next one is found.
    //
    // The asymmetry decides it. A missed one-letter character leaves lines
    // unattributed, which costs nothing on a zone mic and is visible in prep; a false
    // one silently rewrites a third of the script.
    if name.chars().filter(|c| c.is_alphabetic()).count() < 2 || !looks_like_name(&name) {
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

/// Words that join two speakers and are written in lower case even when the names
/// around them are not: `NADIA et ÉRIC`, `LE CHŒUR and NADIA`.
///
/// Without these, a shared line is not recognised as an attribution at all, and the
/// whole thing — names included — lands in the script as dialogue spoken by whoever
/// came before. That is the second way a name turns into a line.
const JOINERS: [&str; 6] = ["et", "and", "en", "&", "avec", "puis"];

/// Nearly all capitals — enough to be a name that was mistyped or badly scanned.
///
/// Only ever used where a colon has already said "this is an attribution". `Attention :`
/// and `Il dit :` fail it, which is the point.
fn mostly_capitals(s: &str) -> bool {
    let letters: Vec<char> = s.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() < 3 || letters.len() > 40 {
        return false;
    }
    let upper = letters.iter().filter(|c| !c.is_lowercase()).count();
    upper * 10 >= letters.len() * 7
}

/// Capitalised, and not a sentence.
///
/// The test is the absence of lower-case letters rather than the presence of upper-case
/// ones, so `ÉLISSA` and `LOÏC` pass without a table of accented capitals.
fn looks_like_name(s: &str) -> bool {
    let letters: Vec<char> = s.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() || letters.len() > 40 || s.ends_with('!') || s.ends_with('?') {
        return false;
    }
    // Every word must be a name, or one of the few words that join two of them.
    s.split_whitespace().all(|w| {
        JOINERS.contains(&fold(w).trim())
            || w.chars().filter(|c| c.is_alphabetic()).all(|c| !c.is_lowercase())
    }) && s
        .split_whitespace()
        .any(|w| !JOINERS.contains(&fold(w).trim()) && w.chars().any(|c| c.is_alphabetic()))
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
        .flat_map(|c| match c {
            // Ligatures, so `LE CHŒUR` and `LE CHOEUR` are one character rather than
            // two — the same name typed on two different keyboards.
            'œ' => "oe".chars().collect::<Vec<_>>(),
            'æ' => "ae".chars().collect(),
            c => vec![c],
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
    fn letters_being_spelled_out_are_text_not_speakers() {
        let (lines, r) = parsed("NICO : Help.\nS\nO\nS\nNICO : Please.");
        assert!(r.characters.iter().all(|c| c.len() > 1), "{:?}", r.characters);
        assert_eq!(lines.len(), 5);
        // And they stay where they were said, rather than becoming speakers who then
        // own everything after them.
        assert!(lines[1..4].iter().all(|l| l.character == "char-nico"));
        assert_eq!(lines[4].character, "char-nico");
    }

    #[test]
    fn a_numbered_part_is_a_section_and_stays_on_the_page() {
        // Lovedoll's whole structure: numbered parts, no acts, no attributions.
        let (lines, r) = parsed(
            "1.AM I A HUMAN (Presenting everyone) on Voice Over\nIs your body real?\n\
             2.ENTRANCE (Nico and Boys) on Music LOVE DOLL#2\nWho is taking care of you?",
        );
        assert_eq!(r.scenes, 2);
        assert_eq!(r.characters.len(), 0, "a numbered title is not a character");
        assert!(lines[0].stage && lines[2].stage);
        assert!(lines[0].text.starts_with("1.AM I A HUMAN"));
        assert_eq!(lines[1].scene, "scene-1");
        assert_eq!(lines[3].scene, "scene-2");
    }

    #[test]
    fn a_capitalised_didascalie_does_not_become_a_character() {
        // Reported from the first Hécube import. SILENCE reads as a name to any rule
        // looking for capitals; the speech beneath it is then attributed to a character
        // called SILENCE, and the real speaker's name is left in the dialogue.
        let (lines, r) = parsed("NADIA : Bonjour.\nSILENCE.\nÉRIC : Bonsoir.");
        assert_eq!(r.characters, ["NADIA", "ÉRIC"], "SILENCE is not a person");
        assert_eq!(r.stage, 1);
        assert!(lines[1].stage);
        assert_eq!(lines[2].character, "char-eric");
        assert_eq!(lines[2].text, "Bonsoir.");
    }

    #[test]
    fn a_line_that_merely_starts_with_a_direction_word_is_still_dialogue() {
        // Éric says this twenty times in Hécube. Matching the word as a substring would
        // file every one of them as a stage direction.
        let (lines, r) = parsed("ÉRIC : Silence, mes amies.\nSilence, mes amies.");
        assert_eq!(r.stage, 0);
        assert!(lines.iter().all(|l| !l.stage));
        assert_eq!(lines[1].character, "char-eric");
    }

    #[test]
    fn two_speakers_sharing_a_line_are_recognised() {
        // The joining word is lower case while the names are not, so a strict
        // no-lower-case test rejects the whole attribution and the names land in the
        // script as dialogue — the second way a name turns into a line.
        let (lines, r) = parsed("NADIA et ÉRIC : Bonjour.\nLE CHŒUR avec NADIA\nEnsemble.");
        assert_eq!(r.characters, ["NADIA et ÉRIC", "LE CHŒUR avec NADIA"]);
        assert_eq!(lines[0].text, "Bonjour.");
        assert_eq!(lines[1].text, "Ensemble.");
        assert_eq!(lines[1].character, "char-le-choeur-avec-nadia");
    }

    #[test]
    fn shared_speakers_are_listed_for_retouching_and_never_split() {
        let (lines, r) = parsed("NADIA et ÉRIC : Bonjour.\nGAËL : Seul.");
        // One character, because whether that is two people or a group is a fact about
        // the production and not something a rule can know.
        assert_eq!(lines[0].character, "char-nadia-et-eric");
        assert_eq!(r.shared, ["NADIA et ÉRIC"]);
        assert!(!r.shared.contains(&"GAËL".to_string()));
    }

    #[test]
    fn a_joining_word_alone_is_not_a_speaker() {
        let (lines, r) = parsed("NADIA : Un.\net\nDeux.");
        assert_eq!(r.characters, ["NADIA"]);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn a_speaker_with_one_line_is_reported_as_worth_checking() {
        // Long enough for a single line to be unusual. On a short excerpt everybody has
        // one line, so the warning is suppressed rather than made meaningless.
        let mut script = String::new();
        for n in 0..50 {
            script.push_str(&format!("NADIA : Ligne {n}.\n"));
        }
        script.push_str("NADlA : Trois.\n");
        let (_, r) = parsed(&script);
        assert_eq!(r.suspect, ["NADlA"], "the misspelling is surfaced, not merged");

        let (_, short) = parsed("NADIA : Un.\nNADlA : Deux.");
        assert!(short.suspect.is_empty(), "no noise on a short excerpt");
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
