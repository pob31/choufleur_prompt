//! Carrying a cue list between shows.
//!
//! With one show per session, this is the only way material crosses from one to another:
//! last year's conduite onto this year's script, the lighting list from the tour onto the
//! revival, a sheet prepared against a draft onto the version the company is actually
//! performing.
//!
//! The whole problem is that a cue is anchored to a line **id**, and ids do not survive
//! the crossing. `L-0142` in one script is a different sentence in another, so copying a
//! sheet across unchanged does not produce a wrong-looking result — it produces a
//! plausible-looking one, with a hundred cues silently pointing at the wrong moments.
//! Measured on Hécube when the script was reworked: *3 cues dangling, 123 silently
//! displaced.*
//!
//! So every cue is re-anchored by its **text**, not its id. Cue sheets record the line
//! they were attached to (`lineText`), which means the source script is not needed —
//! only the sheet and the script it is landing on.
//!
//! Three outcomes, and the third is the important one:
//!
//! - **exact** — the recorded text is in the new script, once, or unambiguously in order.
//! - **moved** — no exact match, but one line is close enough to be the same line
//!   reworded. Re-anchored and marked for review.
//! - **needs review** — nothing close enough. The cue **keeps its old anchor** and is
//!   flagged. Deliberately: a cue left visibly wrong in prep gets fixed at the table,
//!   whereas a cue quietly re-pointed at the nearest plausible line goes wrong during a
//!   performance, which is the failure this whole module exists to avoid.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use choufleur_core::matcher::{token_dice, token_set_ratio};
use choufleur_core::normalize::normalize_base;

use crate::store::Store;

/// How one cue landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    Exact,
    Moved,
    Review,
}

#[derive(Debug, Clone)]
pub struct Landing {
    pub cue: String,
    pub was: Option<String>,
    pub now: Option<String>,
    pub how: How,
    pub score: f64,
    /// The first words of the line it was attached to, for the report.
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub landings: Vec<Landing>,
    pub sheet: PathBuf,
}

impl Report {
    pub fn count(&self, how: How) -> usize {
        self.landings.iter().filter(|l| l.how == how).count()
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (e, m, r) = (
            self.count(How::Exact),
            self.count(How::Moved),
            self.count(How::Review),
        );
        writeln!(
            f,
            "{} cues — {e} landed exactly, {m} re-anchored, {r} need review",
            self.landings.len()
        )?;
        for l in self.landings.iter().filter(|l| l.how != How::Exact) {
            let what = match l.how {
                How::Moved => format!("moved to {}", l.now.as_deref().unwrap_or("?")),
                How::Review => format!("kept on {}", l.was.as_deref().unwrap_or("nothing")),
                How::Exact => unreachable!(),
            };
            writeln!(f, "  {:<8} {:<22} {:.2}  {:?}", l.cue, what, l.score, l.text)?;
        }
        write!(
            f,
            "{}",
            if r == 0 {
                "every cue found its line".to_string()
            } else {
                format!("{r} to check in prep — they are marked, and kept where they were")
            }
        )
    }
}

/// Below this, a line is not the same line reworded — it is a different line.
///
/// 0.55 comes from `reanchor_cues.py`, where it was tuned against the Hécube rework.
const REVIEW_BELOW: f64 = 0.55;

struct Target {
    ids: Vec<String>,
    keys: Vec<String>,
    by_key: HashMap<String, Vec<usize>>,
    tokens: Vec<Vec<String>>,
}

impl Target {
    fn read(script: &Path) -> Result<Self> {
        let doc: serde_json::Value = serde_json::from_slice(
            &std::fs::read(script).with_context(|| format!("reading {}", script.display()))?,
        )?;
        let lines = doc
            .get("lines")
            .and_then(|l| l.as_array())
            .context("that script has no lines")?;
        let mut t = Target {
            ids: Vec::with_capacity(lines.len()),
            keys: Vec::with_capacity(lines.len()),
            by_key: HashMap::new(),
            tokens: Vec::with_capacity(lines.len()),
        };
        for (i, l) in lines.iter().enumerate() {
            let id = l.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let key = normalize_base(l.get("text").and_then(|v| v.as_str()).unwrap_or(""));
            t.by_key.entry(key.clone()).or_default().push(i);
            t.tokens
                .push(key.split_whitespace().map(str::to_string).collect());
            t.ids.push(id);
            t.keys.push(key);
        }
        Ok(t)
    }

    /// The line this text belongs to, preferring forward progress from `cursor`.
    ///
    /// Returns whether the match was **exact** separately from its score, because a high
    /// score is not the same claim. `token_set_ratio` answers 1.0 when the recorded text
    /// is a subset of the line — *"Tu sais où est le temple d'Athéna ?"* against *"…
    /// d'Athéna Troyenne ?"* — and treating that as exact filed a genuinely reworded
    /// line as certain and never showed it to anybody. Only normalised equality is
    /// exact; everything else is a proposal.
    ///
    /// Order is what disambiguates a repeated line. A script with `"Oui."` twelve times
    /// gives twelve exact matches and no way to choose between them on text alone — but
    /// cues arrive in performance order, so the first one after where the last cue landed
    /// is the right answer far more often than the first one in the file.
    fn find(&self, text: &str, cursor: usize) -> Option<(usize, f64, bool)> {
        let key = normalize_base(text);
        if key.is_empty() {
            return None;
        }
        if let Some(hits) = self.by_key.get(&key) {
            let at = hits
                .iter()
                .find(|&&i| i >= cursor)
                .or_else(|| hits.last())
                .copied()?;
            return Some((at, 1.0, true));
        }
        let want: Vec<&str> = key.split_whitespace().collect();
        if want.is_empty() {
            return None;
        }
        let mut best = (0usize, 0.0f64);
        for (i, toks) in self.tokens.iter().enumerate() {
            if toks.is_empty() {
                continue;
            }
            let have: Vec<&str> = toks.iter().map(String::as_str).collect();
            // Agreement, tempered by how much of each side the agreement covers.
            //
            // `token_set_ratio` alone answers 1.0 whenever either text is a subset of
            // the other, in *both* directions — so a bare `Pause.` line swallowed a cue
            // recorded against `Ça suffit. (Pause) « La honte m'empêche… »`, and a line
            // reading `Musique` swallowed `Chorégraphie sur la musique d'Otis Redding.`
            // Both landed at a confident-looking 1.00 on the real conduite.
            //
            // Dice is the correction and it is the tracker's own: `token_set_ratio ×
            // token_dice` is the scoring shape in `tracker.rs`, for exactly this reason.
            // A one-word line against a nine-word cue scores 2/10, while a genuine
            // rewording keeps almost all its tokens and barely moves.
            let s = token_set_ratio(&want, &have) * token_dice(&want, &have);
            // Ties go forwards, for the same reason exact matches do.
            if s > best.1 || (s == best.1 && i >= cursor && best.0 < cursor) {
                best = (i, s);
            }
        }
        (best.1 > 0.0).then_some((best.0, best.1, false))
    }
}

/// Re-anchor a cue document against a script, in place.
pub fn reanchor(doc: &mut serde_json::Value, script: &Path) -> Result<Report> {
    let target = Target::read(script)?;
    let known: HashMap<&str, usize> = target
        .ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();

    let cues = doc
        .get_mut("cues")
        .and_then(|c| c.as_array_mut())
        .context("that file has no cues")?;

    let mut report = Report::default();
    let mut cursor = 0usize;

    for cue in cues.iter_mut() {
        let id = cue
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let was = cue
            .get("lineId")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let recorded = cue
            .get("lineText")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // An id that already exists in this script, with text that agrees, is a sheet
        // coming home rather than crossing over — leave it exactly alone.
        if let Some(&i) = was.as_deref().and_then(|w| known.get(w)) {
            if recorded.is_empty() || normalize_base(&recorded) == target.keys[i] {
                cursor = i;
                report.landings.push(Landing {
                    cue: id,
                    was,
                    now: Some(target.ids[i].clone()),
                    how: How::Exact,
                    score: 1.0,
                    text: snippet(&target.keys[i]),
                });
                continue;
            }
        }

        let found = if recorded.is_empty() {
            None
        } else {
            target.find(&recorded, cursor)
        };

        let (how, now, score, text) = match found {
            Some((i, s, true)) => {
                cursor = i;
                (How::Exact, Some(target.ids[i].clone()), s, target.keys[i].clone())
            }
            Some((i, s, false)) if s >= REVIEW_BELOW => {
                cursor = i;
                (How::Moved, Some(target.ids[i].clone()), s, target.keys[i].clone())
            }
            other => (
                How::Review,
                was.clone(),
                other.map(|(_, s, _)| s).unwrap_or(0.0),
                recorded.clone(),
            ),
        };

        if let Some(o) = cue.as_object_mut() {
            match (&now, how) {
                (Some(new_id), How::Exact) | (Some(new_id), How::Moved) => {
                    o.insert("lineId".into(), new_id.clone().into());
                    // The recorded text follows the anchor, so a second crossing starts
                    // from where this one landed rather than from the original show.
                    o.insert("lineText".into(), text.clone().into());
                }
                _ => {}
            }
            match how {
                How::Exact => {
                    o.remove("needsReview");
                }
                // Both a reworded landing and a failed one want a human eye. The
                // difference is that one moved and one did not, which the report says.
                How::Moved | How::Review => {
                    o.insert("needsReview".into(), true.into());
                }
            }
        }

        report.landings.push(Landing {
            cue: id,
            was,
            now,
            how,
            score,
            text: snippet(&text),
        });
    }
    Ok(report)
}

fn snippet(s: &str) -> String {
    s.chars().take(48).collect()
}

/// Bring a cue list into a show, re-anchored against its script.
///
/// The source file is read and never written. The sheet lands in the show's `cues/`
/// under a name that is free, so importing the same list twice gives you two lists to
/// compare rather than one silently overwritten.
pub fn import_sheet(show_dir: &Path, from: &Path, name: Option<&str>) -> Result<Report> {
    let script = show_dir.join("script.json");
    if !script.exists() {
        bail!("{} is not a show — no script.json", show_dir.display());
    }
    let mut doc: serde_json::Value = serde_json::from_slice(
        &std::fs::read(from).with_context(|| format!("reading {}", from.display()))?,
    )
    .with_context(|| format!("parsing {}", from.display()))?;

    let stem = name
        .map(str::to_string)
        .or_else(|| {
            from.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .filter(|s| s != "cues")
        })
        .unwrap_or_else(|| "imported".into());
    let stem: String = stem
        .chars()
        .map(|c| if "/\\:<>\"|?*".contains(c) { '-' } else { c })
        .collect();

    let cues_dir = show_dir.join("cues");
    std::fs::create_dir_all(&cues_dir)?;
    let mut dest = cues_dir.join(format!("cues-{stem}.json"));
    for n in 2.. {
        if !dest.exists() {
            break;
        }
        dest = cues_dir.join(format!("cues-{stem}-{n}.json"));
    }

    let mut report = reanchor(&mut doc, &script)?;
    if let Some(o) = doc.as_object_mut() {
        // A list needs a name of its own: the name is the operator's identity on every
        // flag they leave, and two lists called the same thing are two lists nobody can
        // tell apart.
        o.entry("name").or_insert_with(|| stem.clone().into());
        o.insert(
            "provenance".into(),
            serde_json::json!({
                "importedFrom": from.to_string_lossy(),
                "reanchoredAgainst": script.to_string_lossy(),
            }),
        );
    }

    // Written through the store so the show's other files are snapshotted alongside —
    // an import that lands badly should be as recoverable as an edit that does.
    let store = Store::new(show_dir, vec![script.clone(), dest.clone()]);
    store.arm();
    std::fs::write(&dest, "{\"cues\":[]}\n")?;
    store.edit(&dest, |d| {
        *d = doc;
        Ok(())
    })?;

    report.sheet = dest;
    Ok(report)
}

/// Copy a cue list out of a show, so it can be carried to another.
pub fn export_sheet(sheet: &Path, to: &Path) -> Result<PathBuf> {
    let doc: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sheet).with_context(|| format!("reading {}", sheet.display()))?,
    )?;
    let dest = if to.is_dir() {
        to.join(sheet.file_name().context("that sheet has no name")?)
    } else {
        to.to_path_buf()
    };
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    std::fs::write(&dest, serde_json::to_string_pretty(&doc)? + "\n")
        .with_context(|| format!("writing {}", dest.display()))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script_at(dir: &Path, lines: &[(&str, &str)]) -> PathBuf {
        let doc = serde_json::json!({
            "lines": lines.iter().map(|(id, text)| serde_json::json!({
                "id": id, "text": text,
            })).collect::<Vec<_>>(),
        });
        let p = dir.join("script.json");
        std::fs::write(&p, serde_json::to_string(&doc).unwrap()).unwrap();
        p
    }

    fn sheet(cues: &[(&str, &str, &str)]) -> serde_json::Value {
        serde_json::json!({
            "cues": cues.iter().map(|(id, line_id, text)| serde_json::json!({
                "id": id, "lineId": line_id, "lineText": text, "cue": "GO",
            })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn text_finds_the_line_even_when_every_id_has_changed() {
        let tmp = tempfile::tempdir().unwrap();
        // Same play, renumbered from a different import.
        let script = script_at(
            tmp.path(),
            &[("X-1", "Bonjour."), ("X-2", "Nadia ? Nadia ?"), ("X-3", "Un temps.")],
        );
        let mut doc = sheet(&[("Q-0001", "L-0042", "Nadia ? Nadia ?")]);
        let r = reanchor(&mut doc, &script).unwrap();
        assert_eq!(r.count(How::Exact), 1);
        assert_eq!(doc["cues"][0]["lineId"], "X-2");
        assert!(doc["cues"][0].get("needsReview").is_none());
    }

    #[test]
    fn a_reworded_line_is_moved_and_flagged_rather_than_lost() {
        let tmp = tempfile::tempdir().unwrap();
        let script = script_at(
            tmp.path(),
            &[("X-1", "Bonjour."), ("X-2", "Tu sais où est le temple d'Athéna ?")],
        );
        let mut doc = sheet(&[("Q-1", "L-9", "Tu sais où est le temple d'Athéna Troyenne ?")]);
        let r = reanchor(&mut doc, &script).unwrap();
        assert_eq!(r.count(How::Moved), 1);
        assert_eq!(doc["cues"][0]["lineId"], "X-2");
        assert_eq!(doc["cues"][0]["needsReview"], true);
    }

    #[test]
    fn a_cue_with_no_home_keeps_its_old_anchor_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let script = script_at(tmp.path(), &[("X-1", "Bonjour."), ("X-2", "Au revoir.")]);
        let mut doc = sheet(&[("Q-1", "L-77", "Le chœur entre par le fond du plateau")]);
        let r = reanchor(&mut doc, &script).unwrap();
        assert_eq!(r.count(How::Review), 1);
        // Kept where it was, not re-pointed at the nearest plausible line.
        assert_eq!(doc["cues"][0]["lineId"], "L-77");
        assert_eq!(doc["cues"][0]["needsReview"], true);
    }

    #[test]
    fn repeated_lines_are_told_apart_by_order() {
        let tmp = tempfile::tempdir().unwrap();
        let script = script_at(
            tmp.path(),
            &[("X-1", "Oui."), ("X-2", "Et alors ?"), ("X-3", "Oui."), ("X-4", "Fin."), ("X-5", "Oui.")],
        );
        let mut doc = sheet(&[
            ("Q-1", "a", "Oui."),
            ("Q-2", "b", "Fin."),
            ("Q-3", "c", "Oui."),
        ]);
        reanchor(&mut doc, &script).unwrap();
        assert_eq!(doc["cues"][0]["lineId"], "X-1");
        assert_eq!(doc["cues"][1]["lineId"], "X-4");
        // The third "Oui." is the one after "Fin.", not the first in the file.
        assert_eq!(doc["cues"][2]["lineId"], "X-5");
    }

    #[test]
    fn a_sheet_coming_home_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let script = script_at(tmp.path(), &[("L-0001", "Bonjour."), ("L-0002", "Au revoir.")]);
        let mut doc = sheet(&[("Q-1", "L-0002", "Au revoir.")]);
        let r = reanchor(&mut doc, &script).unwrap();
        assert_eq!(r.count(How::Exact), 1);
        assert_eq!(doc["cues"][0]["lineId"], "L-0002");
    }

    #[test]
    fn an_id_that_still_exists_but_now_says_something_else_is_re_anchored() {
        let tmp = tempfile::tempdir().unwrap();
        // L-0002 survived as an id but the line at it was replaced — the exact shape of
        // the drift that displaced 123 cues on Hécube.
        let script = script_at(
            tmp.path(),
            &[("L-0001", "Bonjour."), ("L-0002", "Something else entirely."), ("L-0003", "Au revoir.")],
        );
        let mut doc = sheet(&[("Q-1", "L-0002", "Au revoir.")]);
        reanchor(&mut doc, &script).unwrap();
        assert_eq!(doc["cues"][0]["lineId"], "L-0003", "text wins over a stale id");
    }

    #[test]
    fn importing_lands_the_sheet_in_the_show_without_touching_the_source() {
        let tmp = tempfile::tempdir().unwrap();
        let show = tmp.path().join("Show");
        std::fs::create_dir_all(show.join("cues")).unwrap();
        script_at(&show, &[("X-1", "Bonjour."), ("X-2", "Au revoir.")]);

        let src = tmp.path().join("conduite.json");
        std::fs::write(&src, serde_json::to_string(&sheet(&[("Q-1", "L-9", "Au revoir.")])).unwrap())
            .unwrap();
        let before = std::fs::read(&src).unwrap();

        let r = import_sheet(&show, &src, Some("LUMIÈRE")).unwrap();
        assert_eq!(r.count(How::Exact), 1);
        assert_eq!(r.sheet, show.join("cues/cues-LUMIÈRE.json"));
        let landed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&r.sheet).unwrap()).unwrap();
        assert_eq!(landed["cues"][0]["lineId"], "X-2");
        assert_eq!(landed["name"], "LUMIÈRE");

        assert_eq!(std::fs::read(&src).unwrap(), before, "the source is read-only");

        // A second import does not overwrite the first.
        let again = import_sheet(&show, &src, Some("LUMIÈRE")).unwrap();
        assert_eq!(again.sheet, show.join("cues/cues-LUMIÈRE-2.json"));
    }

    #[test]
    fn exporting_refuses_to_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("cues.json");
        std::fs::write(&src, r#"{"cues":[]}"#).unwrap();
        let out = tmp.path().join("out.json");
        assert_eq!(export_sheet(&src, &out).unwrap(), out);
        assert!(export_sheet(&src, &out).is_err());
    }
}
