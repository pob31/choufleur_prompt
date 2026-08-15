//! `serve` — run a corpus at real speed, audibly, with the script on a screen.
//!
//! The whole of Phase 0 is file comparisons. This is the first thing that can be
//! *watched*, which is the only way to answer the questions the numbers cannot: does
//! a display that is right 91 % of the time feel trustworthy, does the lost banner
//! read as honest or as broken, is a second of lag invisible or maddening.
//!
//! Threading, and why it is this shape:
//!
//! - the **engine thread** is blocking and single-threaded, and owns Metal and the
//!   one reused `WhisperState`. It must never become async.
//! - the **audio callback** is real-time and owns nothing but a queue.
//! - **axum** runs on tokio and only ever reads shared state.
//!
//! - the **monitor thread** reads the same file and plays it, and is blocked by
//!   nothing but the sound card.
//!
//! Playback and analysis are deliberately *not* the same thread. Sharing one made
//! every Whisper decode stall the speaker, and the result warbled from end to end.
//! Kept apart, each is paced at real speed on its own — the device for sound, the
//! clock for analysis — and when a decode runs long the display slips behind the
//! voice rather than the audio breaking up. That slip is the thing being judged, so
//! it must be allowed to show.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use choufleur_core::lang::LangCode;
use choufleur_core::prompt::{proper_nouns, static_prompt_with, BiasMode, PromptConfig};
use choufleur_core::tracker::Tracker;

use crate::engine::{Engine, EngineConfig};
use crate::live::{Broadcast, CueCategory, CueSheet, CueView, LineView, LiveState, Update};
use crate::manifest::Corpus;
use crate::monitor::Monitor;

/// Where the page is read from while iterating, if it is present.
///
/// Compiled-in remains the fallback so a built binary is still self-contained.
///
/// Resolved against the crate directory at compile time, **not** against the working
/// directory. It was a repo-relative path, which meant it only resolved when the
/// binary happened to be run from the repository root — and from anywhere else the
/// lookup quietly failed and the page fell back to `include_str!`. That is worse than
/// no disk-serving at all: edits to the page appear to do nothing until the next
/// `cargo build` bakes them in, so a fix and a failed fix look identical. It cost an
/// afternoon here, twice, and the second time the operator was the one reporting that
/// a button did not work.
const ASSET_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/live.html"
);

/// A colour name to something a browser can paint.
///
/// Only the names a marked-up script actually uses. An unknown name is passed through
/// untouched, because CSS knows far more colour names than this table ever will and
/// the operator may well have written one of them.
fn swatch(name: &str) -> String {
    match name {
        "blue" => "#4d7ea8",
        "purple" => "#8566ad",
        "gold" => "#c8a33a",
        "pale yellow" => "#c2bd6a",
        "orange" => "#c07a3a",
        "green" => "#5f9e6a",
        "red" => "#c0564a",
        "grey" | "gray" => "#7a8088",
        other => other,
    }
    .to_string()
}

/// Every cue list beside the script, in a stable order.
///
/// `cues.json` and `cues-*.json`. A show grows a list per operator, and asking for each
/// path on the command line every time is friction with no decision behind it.
fn find_cue_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("json")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s == "cues" || s.starts_with("cues-"))
                // Backups are for reading by hand, not for serving.
                && !p.to_string_lossy().contains("backup")
        })
        .collect();
    out.sort();
    out
}

/// A sheet's meta read straight from its file.
fn read_sheet_meta(path: &Path, id: &str) -> Option<CueSheet> {
    let raw = std::fs::read_to_string(path).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(sheet_meta(&doc, id))
}

/// A sheet's identity and vocabulary: what it is called and what its marks mean.
fn sheet_meta(doc: &serde_json::Value, id: &str) -> CueSheet {
    // An explicit list wins. Nothing writes one yet, but this is the shape the
    // per-operator lists will arrive in, and reading it now costs a dozen lines.
    if let Some(arr) = doc.get("categories").and_then(|c| c.as_array()) {
        let categories = arr
            .iter()
            .filter_map(|c| {
                let key = c.get("key")?.as_str()?.to_string();
                let label = c
                    .get("label")
                    .and_then(|l| l.as_str())
                    .unwrap_or(&key)
                    .to_string();
                let swatch_of = c
                    .get("swatch")
                    .and_then(|s| s.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| swatch(&key));
                Some(CueCategory { key, label, swatch: swatch_of })
            })
            .collect();
        return CueSheet {
            id: id.to_string(),
            name: sheet_name(doc, id),
            categories,
        };
    }
    // Otherwise: the colours this sheet actually uses, in the order they first appear,
    // read through its own `colourMeans`. Order of first appearance rather than
    // alphabetical, so the buttons come out in the order the show does.
    let means = doc.get("colourMeans");
    let mut categories: Vec<CueCategory> = Vec::new();
    if let Some(cues) = doc.get("cues").and_then(|c| c.as_array()) {
        for c in cues {
            let Some(key) = c.get("noteColour").and_then(|v| v.as_str()) else {
                continue;
            };
            if categories.iter().any(|c| c.key == key) {
                continue;
            }
            let label = means
                .and_then(|m| m.get(key))
                .and_then(|l| l.as_str())
                .unwrap_or(key)
                .to_string();
            categories.push(CueCategory {
                key: key.to_string(),
                label,
                swatch: swatch(key),
            });
        }
    }
    CueSheet {
        id: id.to_string(),
        name: sheet_name(doc, id),
        categories,
    }
}

/// What the operator calls this list. `name` if they have said, otherwise the source
/// document's own name — the sound conduite already carries the PDF's filename there —
/// and the file's stem as a last resort.
fn sheet_name(doc: &serde_json::Value, id: &str) -> String {
    for key in ["name", "source"] {
        if let Some(v) = doc.get(key).and_then(|v| v.as_str()) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    id.to_string()
}

/// The sheet id for a file: its stem, so `cues-lumiere.json` is `cues-lumiere` and a
/// client can name it in a URL. Stable as long as the file is.
fn sheet_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cues")
        .to_string()
}

/// Fold for comparison, keeping a way back to the original.
///
/// The cue sheet's `evidence` is text lifted off the PDF around a highlight, so it
/// carries the source's accents, punctuation and spacing while the script carries the
/// playwright's. Comparing them needs folding; *showing* the result needs the original
/// characters, so the fold records where each of its characters came from.
fn fold_with_map(s: &str) -> (Vec<char>, Vec<usize>) {
    let mut out = Vec::new();
    let mut from = Vec::new();
    let mut space = false;
    for (i, ch) in s.chars().enumerate() {
        let lower: Vec<char> = ch.to_lowercase().collect();
        let c = lower.first().copied().unwrap_or(ch);
        let c = match c {
            'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' => 'i',
            'ô' | 'ö' => 'o',
            'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            other => other,
        };
        if c.is_alphanumeric() {
            out.push(c);
            from.push(i);
            space = false;
        } else if !space && !out.is_empty() {
            out.push(' ');
            from.push(i);
            space = true;
        }
    }
    while out.last() == Some(&' ') {
        out.pop();
        from.pop();
    }
    (out, from)
}

/// Longest run of characters common to both, as `(start_in_b, len)`.
///
/// The plain dynamic-programming version. The inputs are a phrase and a line, so this
/// is a few thousand cell updates per cue and runs once at load.
fn longest_common(a: &[char], b: &[char]) -> Option<(usize, usize)> {
    if a.is_empty() || b.is_empty() {
        return None;
    }
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    let (mut best, mut end_b) = (0usize, 0usize);
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            cur[j] = if a[i - 1] == b[j - 1] { prev[j - 1] + 1 } else { 0 };
            if cur[j] > best {
                best = cur[j];
                end_b = j;
            }
        }
        std::mem::swap(&mut prev, &mut cur);
        cur.iter_mut().for_each(|c| *c = 0);
    }
    (best > 0).then(|| (end_b - best, best))
}

/// The phrase in `line` that the cue's `evidence` points at, if any.
///
/// A cue fires on a word, not on a line — "on «Polymestor entre»" is what an operator
/// writes in a margin, and it is what they are listening for. Only 37 of Hécube's 133
/// cues yield one: the rest were extracted from page-level marks with no highlighted
/// phrase behind them, and for those the honest answer is the line itself rather than a
/// guessed fragment of it.
fn trigger_phrase(evidence: &str, line: &str) -> Option<String> {
    const MIN: usize = 10;
    let (a, _) = fold_with_map(evidence);
    let (b, map) = fold_with_map(line);
    let (start, len) = longest_common(&a, &b)?;
    if len < MIN {
        return None;
    }
    // Back to original characters, so the page can find it in the text it is showing.
    let first = *map.get(start)?;
    let last = *map.get(start + len - 1)?;
    let phrase: String = line
        .chars()
        .skip(first)
        .take(last - first + 1)
        .collect::<String>()
        .trim()
        .to_string();
    (phrase.chars().count() >= MIN).then_some(phrase)
}

/// Read the cue sheet beside the script and resolve its anchors to line positions.
///
/// Missing file, unreadable file, unknown anchors: none of them stop the show. A cue
/// sheet is an aid, and a display that refuses to start because one cue points at a
/// line that was deleted this afternoon would be worse than no cue sheet at all — so
/// the dangling ones are counted, named on the console, and dropped.
fn load_cues(path: &Path, lines: &[LineView], sheet: &str) -> Vec<CueView> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(mut doc) = serde_json::from_str::<serde_json::Value>(&raw) else {
        eprintln!("cues: {} could not be parsed; carrying on without", path.display());
        return Vec::new();
    };
    // Give every cue an id, once, and write them back.
    //
    // The sheet was extracted from a PDF and its cues have never needed naming: they
    // were only ever read in order. Editing one needs a way to say *which*, and
    // position will not do — the rail sorts by line while the file is in page order,
    // and either can change under the other. Written back rather than assigned per
    // run so an id means the same thing tomorrow.
    if let Some(arr) = doc.get_mut("cues").and_then(|c| c.as_array_mut()) {
        let mut next = 1usize;
        let mut assigned = false;
        for c in arr.iter_mut() {
            if c.get("id").and_then(|v| v.as_str()).is_some() {
                continue;
            }
            if let Some(o) = c.as_object_mut() {
                o.insert("id".into(), format!("Q-{next:04}").into());
                assigned = true;
            }
            next += 1;
        }
        if assigned {
            if let Ok(text) = serde_json::to_string_pretty(&doc) {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, text + "\n").is_ok() && std::fs::rename(&tmp, path).is_ok() {
                    println!("cues:    assigned ids to {} cues", next - 1);
                }
            }
        }
    }
    let index: std::collections::HashMap<&str, usize> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| (l.id.as_str(), i))
        .collect();
    // What a colour means, from the categories if the operator has defined them and
    // from the PDF's own key otherwise. Categories win: once somebody has said their
    // targets are a desk and a QLab 5, the highlighter legend is history — and
    // renaming a target has to change what the cards say or the rename did nothing.
    let list = sheet_meta(&doc, sheet);
    let means: std::collections::HashMap<String, String> = if list.categories.is_empty() {
        doc.get("colourMeans")
            .and_then(|m| m.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        list.categories
            .iter()
            .map(|c| (c.key.clone(), c.label.clone()))
            .collect()
    };
    let mut out = Vec::new();
    let mut dangling: Vec<String> = Vec::new();
    let empty = Vec::new();
    for c in doc.get("cues").and_then(|c| c.as_array()).unwrap_or(&empty) {
        let Some(id) = c.get("lineId").and_then(|v| v.as_str()) else {
            continue; // a page-level note with no anchor; nothing to place it against
        };
        let Some(&line_index) = index.get(id) else {
            dangling.push(id.to_string());
            continue;
        };
        let text_of_cue = c
            .get("cue")
            .and_then(|v| v.as_str())
            .or_else(|| c.get("action").and_then(|v| v.as_str()))
            .unwrap_or("")
            .trim()
            .to_string();
        let colour = c
            .get("noteColour")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        // A phrase the operator picked is taken at its word.
        //
        // `trigger_phrase` exists to dig a trigger out of text scraped off a PDF, so
        // it fuzzy-matches and insists on ten characters — below that a common run
        // means nothing. Neither applies to a phrase somebody selected in the script:
        // it is already exact, and "Silence" is seven characters and a perfectly good
        // cue. Putting a hand-picked trigger through the extractor's rules would throw
        // it away without a word, which is the failure this whole day keeps producing.
        let evidence = c.get("evidence").and_then(|v| v.as_str());
        let by_hand = c.get("evidenceFrom").and_then(|v| v.as_str()) == Some("operator");
        let text = &lines[line_index].text;
        let (trigger_text, trigger_nth) = match evidence {
            Some(ev) if by_hand => {
                let nth = c
                    .get("evidenceNth")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                // The recorded occurrence has to still exist. A line edited since the
                // cue was placed may have fewer of them, and pointing at the wrong
                // "Comme ça." is exactly the fault this field was added to fix — so
                // fall back to the first rather than to whatever is nearest.
                let found = text.match_indices(ev).count();
                if text.contains(ev) {
                    (Some(ev.to_string()), if nth < found { nth } else { 0 })
                } else {
                    (None, 0)
                }
            }
            Some(ev) => match trigger_phrase(ev, text) {
                // An extracted phrase carries no occurrence, so it means the first —
                // which is what the extractor found, since it matched forwards.
                Some(p) => (Some(p), 0),
                None => (None, 0),
            },
            None => (None, 0),
        };
        out.push(CueView {
            id: c
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            sheet: sheet.to_string(),
            fired_by: match c.get("firedBy").and_then(|v| v.as_str()) {
                Some("auto") => "auto",
                _ => "operator",
            }
            .to_string(),
            via: c
                .get("via")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            line_index,
            line_id: id.to_string(),
            cue: text_of_cue,
            trigger_text,
            trigger_nth,
            trigger: c
                .get("trigger")
                .and_then(|v| v.as_str())
                .unwrap_or("text")
                .to_string(),
            means: colour.as_deref().and_then(|c| means.get(c)).cloned(),
            colour,
            needs_review: c
                .get("needsReview")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        });
    }
    // Script order, because the rail walks forward through them and the sheet is in
    // page order, which is not the same thing once a cue has been re-anchored.
    out.sort_by_key(|c| c.line_index);
    if !dangling.is_empty() {
        dangling.sort();
        dangling.dedup();
        eprintln!(
            "cues: {} anchored to lines that no longer exist, dropped: {}",
            dangling.len(),
            dangling.join(", ")
        );
    }
    out
}

/// Write a cue back to the sheet, or add or remove one.
///
/// `None` for a field means leave it as it was, which is what lets the page send only
/// what the operator touched. `Some("")` on the trigger clears it — the distinction
/// matters, because a trigger that could be set and not unset is a trap.
///
/// The whole sheet is rewritten each time. At 133 cues that costs nothing, and it has
/// the property that matters during a show: the file on disk is never half a cue.
#[allow(clippy::too_many_arguments)]
fn write_cue(
    path: &Path,
    id: &str,
    cue: Option<&str>,
    trigger: Option<&str>,
    trigger_text: Option<&str>,
    trigger_nth: Option<u64>,
    colour: Option<&str>,
    // `(firedBy, via)`. An empty `via` clears it; an unrecognised `firedBy` leaves both
    // alone, so a client that does not know about the field cannot erase it.
    fired: Option<(String, String)>,
    line_id: Option<&str>,
    delete: bool,
) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let arr = doc
        .get_mut("cues")
        .and_then(|c| c.as_array_mut())
        .context("cue sheet has no cues")?;
    let at = arr
        .iter()
        .position(|c| c.get("id").and_then(|v| v.as_str()) == Some(id))
        .context("no such cue")?;
    if delete {
        arr.remove(at);
    } else {
        let c = arr[at].as_object_mut().context("cue is not an object")?;
        if let Some(v) = cue {
            c.insert("cue".into(), v.into());
        }
        if let Some(v) = trigger {
            c.insert("trigger".into(), v.into());
        }
        if let Some(v) = line_id {
            c.insert("lineId".into(), v.into());
        }
        if let Some(v) = colour {
            if v.is_empty() {
                c.remove("noteColour");
            } else {
                c.insert("noteColour".into(), v.into());
            }
        }
        if let Some(n) = trigger_nth {
            if n == 0 {
                c.remove("evidenceNth");
            } else {
                c.insert("evidenceNth".into(), n.into());
            }
        }
        if let Some(v) = trigger_text {
            // Stored where the extractor put it, so a hand-set trigger and an
            // extracted one are the same kind of thing and survive a re-anchor
            // together. `evidenceFrom` says which, because "the operator chose this"
            // is worth more than "the PDF suggested it" when the two disagree.
            if v.is_empty() {
                c.remove("evidence");
                c.remove("evidenceFrom");
            } else {
                c.insert("evidence".into(), v.into());
                c.insert("evidenceFrom".into(), "operator".into());
            }
        }
        if let Some((fired_by, via)) = &fired {
            match fired_by.as_str() {
                "auto" => {
                    c.insert("firedBy".into(), "auto".into());
                    if via.trim().is_empty() {
                        c.remove("via");
                    } else {
                        c.insert("via".into(), via.trim().into());
                    }
                }
                "operator" => {
                    // Removed rather than written false: the default is that a person
                    // presses it, and the file should only record the exception.
                    c.remove("firedBy");
                    c.remove("via");
                }
                _ => {}
            }
        }
        // A cue the operator has been through is a cue somebody has checked.
        c.remove("needsReview");
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Replace the list's categories, keeping the cues that use them working.
///
/// Written as an explicit `categories` array, which then takes precedence over the
/// colours-in-use derivation for good — the moment an operator has said what their
/// targets are, guessing from a PDF's highlighter marks is no longer the better answer.
///
/// Keys are never rewritten here. A cue stores its category by key, so renaming
/// "QLab" to "QLab 5" must not orphan seventy cues; only the label and the swatch are
/// the operator's to change freely.
fn write_categories(path: &Path, cats: &[serde_json::Value]) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let obj = doc.as_object_mut().context("cue sheet is not an object")?;
    let clean: Vec<serde_json::Value> = cats
        .iter()
        .filter_map(|c| {
            let key = c.get("key")?.as_str()?.trim();
            if key.is_empty() {
                return None;
            }
            let label = c.get("label").and_then(|l| l.as_str()).unwrap_or(key).trim();
            let swatch = c.get("swatch").and_then(|s| s.as_str()).unwrap_or("").trim();
            Some(serde_json::json!({
                "key": key,
                "label": if label.is_empty() { key } else { label },
                "swatch": swatch,
            }))
        })
        .collect();
    obj.insert("categories".into(), serde_json::Value::Array(clean));
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Add a cue against a line, and return its id.
fn append_cue(path: &Path, line_id: &str) -> Result<String> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let arr = doc
        .get_mut("cues")
        .and_then(|c| c.as_array_mut())
        .context("cue sheet has no cues")?;
    let taken: std::collections::HashSet<String> = arr
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    let id = (1..)
        .map(|n| format!("Q-{n:04}"))
        .find(|c| !taken.contains(c))
        .expect("an unused id always exists");
    arr.push(serde_json::json!({
        "id": id,
        "lineId": line_id,
        "cue": "",
        "trigger": "text",
        "source": "operator",
    }));
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(id)
}

/// Play a file to the monitor, on a thread that does nothing else.
///
/// The first version of this pushed audio from inside the engine loop, which meant
/// every Whisper decode — around 600 ms for a five-second segment — stalled the feed
/// to a 250 ms buffer. It warbled, audibly and constantly, and the run reported
/// falling behind 1141 times over five minutes. The compute budget was never the
/// problem: this machine transcribes Hécube at 8.4× real time. Sharing a thread was.
///
/// So playback is its own thread, blocked only ever by the sound card, and the engine
/// reads the same file independently paced by `VirtualClock`. When a decode runs long
/// the *engine* slips behind the audio, which is exactly what a live system does and
/// exactly the lag the spike exists to let somebody judge. The alternative — letting
/// the engine run ahead into a deep buffer — would put the text on screen before the
/// words were audible and flatter the system into looking better than it is.
fn play_file(
    path: std::path::PathBuf,
    device: Option<String>,
    buffer_ms: u32,
    audible: Arc<Mutex<f64>>,
) -> Result<()> {
    let mut reader = crate::wav_stream::WavBlockReader::open(&path)?;
    let rate = reader.sample_rate;
    let monitor = Monitor::open(rate, device.as_deref(), buffer_ms)?;
    let frames = (crate::engine::BLOCK_SECONDS * rate as f64) as usize;
    let mut buf = Vec::with_capacity(frames);
    let mut reported = 0u64;
    loop {
        let n = reader.read_block(&mut buf, frames)?;
        if n == 0 {
            break;
        }
        monitor.push(&buf[..n]);
        *audible.lock().unwrap() = monitor.played_seconds();
        // Report starvation as it happens rather than at the end: it means the sound
        // is now behind the analysis, and it will not catch up by itself.
        let s = monitor.starved_frames();
        if s > reported + rate as u64 / 10 {
            eprintln!(
                "monitor: {:.1} s of audio dropped — sound is now behind the display",
                s as f64 / rate as f64
            );
            reported = s;
        }
    }
    monitor.drain();
    // Nothing further will be played, so nothing further should be withheld.
    *audible.lock().unwrap() = f64::INFINITY;
    let s = monitor.starved_frames();
    if s > 0 {
        eprintln!("monitor: {:.2} s dropped in total", s as f64 / rate as f64);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    corpus_path: &Path,
    tracker_config: Option<&Path>,
    compare: Option<&Path>,
    whisper_model: Option<&Path>,
    vad_model: Option<&Path>,
    bias: BiasMode,
    mixdown: bool,
    channels: Option<Vec<u16>>,
    interim_ms: Option<u32>,
    audio_root: Option<PathBuf>,
    no_agc: bool,
    agc_max_gain: Option<f32>,
    no_lexicon: bool,
    realtime: bool,
    port: u16,
    monitor: bool,
    output_device: Option<String>,
    buffer_ms: u32,
    trace_out: Option<PathBuf>,
    prep: bool,
    cues_path: &[PathBuf],
) -> Result<()> {
    let corpus = Corpus::load(corpus_path, audio_root)?;
    let (script, prepared) = super::load_script(&corpus.script_path())?;

    let lines: Vec<LineView> = prepared
        .lines
        .iter()
        .map(|l| LineView {
            id: l.id.clone(),
            character: l.character.clone(),
            text: l.text.clone(),
            scene: l.scene.clone(),
            cut: l.cut,
            kind: match l.kind {
                choufleur_core::script::LineKind::Stage => "stage",
                choufleur_core::script::LineKind::Dialogue => "dialogue",
            }
            .to_string(),
            spoken: l.spoken,
            flag: l.flag,
            hold: l.hold.map(|h| {
                match h {
                    choufleur_core::script::Hold::Silence => "silence",
                    choufleur_core::script::Hold::Music => "music",
                    choufleur_core::script::Hold::Adlib => "adlib",
                }
                .to_string()
            }),
        })
        .collect();

    // Every cue list beside the script unless told otherwise. A show has as many as it
    // has operators, each in its own file — the lists are independent documents, not
    // views of one, so they are never merged and never renumbered against each other.
    let cue_paths: Vec<PathBuf> = if cues_path.is_empty() {
        find_cue_files(&corpus.script_path().with_file_name("."))
    } else {
        cues_path.iter().map(|p| p.to_path_buf()).collect()
    };
    let mut cues: Vec<CueView> = Vec::new();
    let mut sheets: Vec<CueSheet> = Vec::new();
    let mut sheet_paths = std::collections::HashMap::new();
    for path in &cue_paths {
        let id = sheet_id(path);
        let found = load_cues(path, &lines, &id);
        let Some(meta) = read_sheet_meta(path, &id) else {
            continue;
        };
        println!("list:    {} — {} cues from {}", meta.name, found.len(), path.display());
        cues.extend(found);
        sheet_paths.insert(id, path.clone());
        sheets.push(meta);
    }
    cues.sort_by_key(|c| c.line_index);


    let (tx, _rx) = tokio::sync::broadcast::channel(256);
    let state = Arc::new(LiveState {
        title: script.title.clone(),
        script_path: corpus.script_path(),
        lines: Mutex::new(lines),
        latest: Mutex::new(None),
        t_audio: Mutex::new(0.0),
        running: Mutex::new(true),
        tx,
        audible_until: Mutex::new(if monitor { 0.0 } else { f64::INFINITY }),
        steer: Mutex::new(Vec::new()),
        cues: Mutex::new(cues),
        sheet_paths,
        sheets: Mutex::new(sheets),
        prep,
    });

    // Nothing to load, nothing to play, nothing to decode. Serve the script and wait.
    if prep {
        println!("script:  {} lines", state.lines.lock().unwrap().len());
        println!("prep mode — no audio, no recognition; the script is here to be edited");
        return serve_http(state, port);
    }

    let mut ecfg = EngineConfig::new(
        super::resolve_model(whisper_model, super::DEFAULT_WHISPER_MODEL)?,
        super::resolve_model(vad_model, super::DEFAULT_VAD_MODEL)?,
    );
    // Pacing follows the monitor. With sound playing, the block tap has already
    // waited and the clock only measures (`wallLatencyMs` against the audio
    // deadline); with `--no-monitor` there is nothing to pace against, so the run
    // goes as fast as it can unless `--realtime` asks otherwise. That is what makes
    // `--no-monitor` usable as the passthrough gate rather than a two-hour wait.
    ecfg.realtime = monitor || realtime;
    ecfg.agc.enabled = !no_agc;
    if let Some(db) = agc_max_gain {
        ecfg.agc.max_gain_db = db;
    }
    ecfg.mixdown = mixdown;
    ecfg.channels = channels;
    if let Some(ms) = interim_ms {
        ecfg.vad.interim_interval_ms = ms;
    }

    let tcfg = super::track::load_tracker_config(tracker_config)?;
    // A second matcher, raced against the first on the same recognition.
    let rival_cfg = match compare {
        Some(p) => Some(super::track::load_tracker_config(Some(p))?),
        None => None,
    };
    let names: Vec<String> = script.characters.iter().map(|c| c.name.clone()).collect();
    let default_lang = script
        .default_lang
        .first()
        .cloned()
        .unwrap_or(LangCode::new("en"));
    let proper = if no_lexicon {
        Vec::new()
    } else {
        proper_nouns(&prepared.lines, 40)
    };
    println!("lexicon: {}", proper.join(", "));
    let static_text = static_prompt_with(script.title.as_deref(), &names, &proper);
    let lang_of = super::character_languages(&prepared);

    println!("model:   {}", ecfg.whisper_model.display());
    println!("bias:    {bias:?}");
    println!("script:  {} lines", prepared.lines.len());

    // Resolved before the corpus moves into the engine thread. The monitor plays the
    // first selected channel: hearing several close mics summed would be neither what
    // the room sounded like nor what the recogniser is given.
    let play_path = if monitor {
        corpus
            .manifest
            .channels
            .first()
            .map(|c| corpus.resolve_audio(&c.audio.file))
    } else {
        None
    };

    let engine_state = Arc::clone(&state);
    let engine = std::thread::Builder::new()
        .name("choufleur-engine".into())
        .spawn(move || -> Result<()> {
            // Report failures here, not at join(): the server outlives the run, so a
            // panic-free error would otherwise sit invisible until ctrl-c and look
            // exactly like a run that simply produced nothing.
            let outcome = (|| -> Result<()> {
            // Load first: Whisper takes seconds to come up, and starting playback
            // before it is ready would put the show ahead of the tracker from the
            // first word.
            let mut eng = Engine::load(ecfg)?;
            if let Some(path) = play_path {
                let audible = Arc::clone(&engine_state);
                std::thread::Builder::new()
                    .name("choufleur-monitor".into())
                    .spawn(move || {
                        let handle = Arc::new(Mutex::new(0.0));
                        // Publish into the shared state the websocket reads.
                        let shared = Arc::clone(&handle);
                        std::thread::spawn(move || loop {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            let v = *shared.lock().unwrap();
                            *audible.audible_until.lock().unwrap() = v;
                            if v.is_infinite() {
                                break;
                            }
                        });
                        if let Err(e) = play_file(path, output_device, buffer_ms, handle) {
                            eprintln!("monitor stopped: {e:#}");
                        }
                    })
                    .context("spawning the monitor thread")?;
            }
            run_engine(&mut eng, &corpus, &prepared, tcfg, rival_cfg, bias, lang_of,
                       default_lang, static_text, &engine_state, trace_out)?;
            *engine_state.running.lock().unwrap() = false;
            let _ = engine_state.tx.send(Update::RunState {
                t_audio: *engine_state.t_audio.lock().unwrap(),
                running: false,
            });
            println!("\nrun finished");
            Ok(())
            })();
            if let Err(e) = &outcome {
                eprintln!("\nengine stopped: {e:#}");
                *engine_state.running.lock().unwrap() = false;
            }
            outcome
        })
        .context("spawning the engine thread")?;

    serve_http(state, port)?;
    match engine.join() {
        Ok(r) => r,
        Err(_) => anyhow::bail!("the engine thread panicked"),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_engine(
    eng: &mut Engine,
    corpus: &Corpus,
    prepared: &choufleur_core::script::PreparedScript,
    tcfg: choufleur_core::tracker::TrackerConfig,
    rival_cfg: Option<choufleur_core::tracker::TrackerConfig>,
    bias: BiasMode,
    lang_of: std::collections::HashMap<String, Vec<LangCode>>,
    default_lang: LangCode,
    static_text: String,
    state: &Arc<LiveState>,
    trace_out: Option<PathBuf>,
) -> Result<()> {
    let mut inner = super::track::TrackingConsumer {
        tracker: Tracker::new(prepared, tcfg),
        script: prepared,
        trace: Vec::new(),
        bias,
        prompt_cfg: PromptConfig::default(),
        lang_of,
        default_lang,
        static_prompt: static_text,
        timings: false,
    };
    let mut rival = rival_cfg.map(|cfg| super::track::TrackingConsumer {
        tracker: Tracker::new(prepared, cfg),
        script: prepared,
        trace: Vec::new(),
        bias: BiasMode::None,
        prompt_cfg: PromptConfig::default(),
        lang_of: Default::default(),
        default_lang: choufleur_core::lang::LangCode::new("fr"),
        static_prompt: String::new(),
        timings: false,
    });
    let mut consumer = Broadcast {
        inner: &mut inner,
        rival: rival
            .as_mut()
            .map(|r| r as &mut dyn crate::live::PositionSource),
        state: Arc::clone(state),
        seq: 0,
    };
    let stats = eng.run(corpus, &mut consumer)?;
    println!("\n{stats}");
    if let Some(path) = trace_out {
        crate::formats::write_jsonl(&path, &inner.trace)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// Write one corrected line back into the script on disk.
///
/// Read-modify-write of the whole file, which is fine at this scale and has the
/// property that matters: the correction survives the run. An edit that lives only in
/// a browser tab is a note nobody will ever find again, and the mistakes being fixed —
/// an attribution imported as dialogue, a stage direction that became a line — are
/// exactly the ones that would otherwise be rediscovered at the next rehearsal.
///
/// Written via a temporary file and renamed, because this is running during a show and
/// a half-written script is worse than a wrong one.
///
/// The running matcher keeps the text it was prepared with: `PreparedScript` is built
/// once and borrowed by the tracker for the length of the run. So a correction reaches
/// every screen immediately and reaches the *matching* from the next run.
fn write_line_edit(path: &Path, index: usize, edit: &LineEdit) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let line = doc
        .get_mut("lines")
        .and_then(|l| l.as_array_mut())
        .and_then(|l| l.get_mut(index))
        .context("no such line in the script")?;
    line["text"] = serde_json::Value::String(edit.text.clone());
    if let Some(c) = &edit.character {
        line["character"] = serde_json::Value::String(c.clone());
    }
    // Written even when false, so restoring a line is as durable as cutting one. A
    // cut that cannot be undone is a deletion wearing a different name.
    line["cut"] = serde_json::Value::Bool(edit.cut);
    // `spoken` likewise, always and explicitly. Its default depends on the kind, so
    // leaving it out would mean flipping a line from stage back to dialogue silently
    // changed whether it can be matched — and the operator setting it needs to see
    // which way round it is, in the file as well as on the page.
    line["kind"] = serde_json::Value::String(edit.kind.clone());
    line["spoken"] = serde_json::Value::Bool(edit.spoken);
    match &edit.hold {
        Some(h) => line["hold"] = serde_json::Value::String(h.clone()),
        None => {
            if let Some(o) = line.as_object_mut() {
                o.remove("hold");
            }
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Add a line to the script, after `after`, and give it an id of its own.
///
/// Ids carry a number for legibility but order comes from position in the array, so an
/// inserted line only needs to be *unique* — and must not renumber its neighbours,
/// because every cue, note and annotation in the show is anchored to an id. Renumbering
/// to keep them tidy is precisely this morning's cue-sheet drift, performed
/// deliberately. So an insert after `L-0075` becomes `L-0075-1`, then `L-0075-2`.
fn write_line_insert(path: &Path, after: usize, line: &LineView) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let lines = doc
        .get_mut("lines")
        .and_then(|l| l.as_array_mut())
        .context("script has no lines")?;
    let at = (after + 1).min(lines.len());
    let mut new = serde_json::Map::new();
    new.insert("id".into(), line.id.clone().into());
    // Act and scene are inherited from the neighbour rather than asked for: an
    // inserted line is always inside a scene that already exists, and making the
    // operator restate it would only be a chance to get it wrong.
    for key in ["act", "scene"] {
        let inherited = lines
            .get(after.min(lines.len().saturating_sub(1)))
            .and_then(|l| l.get(key))
            .cloned()
            .unwrap_or(serde_json::Value::String(String::new()));
        new.insert(key.into(), inherited);
    }
    new.insert("character".into(), line.character.clone().into());
    new.insert("text".into(), line.text.clone().into());
    new.insert("kind".into(), line.kind.clone().into());
    new.insert("spoken".into(), line.spoken.into());
    if let Some(h) = &line.hold {
        new.insert("hold".into(), h.clone().into());
    }
    lines.insert(at, serde_json::Value::Object(new));
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Remove a line from the script for good, keeping a copy where it can be found.
///
/// Distinct from `cut`, and the operator drew the line exactly right: *"a cut line is
/// an editing choice"* — the play contains it, tonight's production does not, and the
/// page must go on showing it struck through because a silently missing line is how
/// you lose your place. A mistake is not a choice. The importer turning the next
/// actor's name into something the previous one says produces text that was never in
/// the play at all, and leaving it on the page as a cut would be filing a typo as a
/// decision.
///
/// It still goes to a sidecar rather than into the void. The notation spec's
/// no-silent-data-loss rule is about not being able to *find out* what happened, not
/// about keeping rubbish on the page — and a deletion made at eleven at night is
/// exactly the one somebody will want to inspect at the next rehearsal.
fn write_line_delete(path: &Path, index: usize) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let lines = doc
        .get_mut("lines")
        .and_then(|l| l.as_array_mut())
        .context("script has no lines")?;
    anyhow::ensure!(index < lines.len(), "no such line in the script");
    let removed = lines.remove(index);

    // Recorded with the id it followed, not with its index: indices move with every
    // later edit, and an index is the one piece of information that will be wrong by
    // the time anybody reads this file.
    let after = index
        .checked_sub(1)
        .and_then(|i| lines.get(i))
        .and_then(|l| l.get("id"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let sidecar = path.with_extension("removed.json");
    let mut log: Vec<serde_json::Value> = std::fs::read_to_string(&sidecar)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_default();
    log.push(serde_json::json!({ "wasAfter": after, "line": removed }));
    std::fs::write(&sidecar, serde_json::to_string_pretty(&log)? + "\n")?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Set or clear a line's flag on disk, touching nothing else.
///
/// Deliberately its own path rather than a corner of `write_line_edit`: flagging
/// happens mid-run, from a page whose copy of the text may be seconds stale, and
/// writing the whole line back to record a bookmark is how a stale tab overwrites a
/// correction. Read-modify-write of one boolean cannot do that.
fn write_line_flag(path: &Path, index: usize, flag: bool) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let line = doc
        .get_mut("lines")
        .and_then(|l| l.as_array_mut())
        .and_then(|l| l.get_mut(index))
        .context("no such line in the script")?;
    if flag {
        line["flag"] = serde_json::Value::Bool(true);
    } else if let Some(o) = line.as_object_mut() {
        o.remove("flag");
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// An id unique in the whole script, derived from the line it follows.
fn insert_id(lines: &[LineView], after: usize) -> String {
    let base = lines
        .get(after)
        .map(|l| l.id.clone())
        .unwrap_or_else(|| "L-0000".into());
    let taken: std::collections::HashSet<&str> = lines.iter().map(|l| l.id.as_str()).collect();
    (1..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken.contains(id.as_str()))
        .expect("an unused suffix always exists")
}

/// One line's worth of correction, as it arrives from the page.
struct LineEdit {
    text: String,
    character: Option<String>,
    cut: bool,
    kind: String,
    spoken: bool,
    hold: Option<String>,
}

/// Re-read the sheet from disk and publish it into the shared state.
///
/// Re-read rather than patched in place: the trigger phrase is derived from the line's
/// text, the sort is by line, and the categories decide the painting — so the only
/// copy worth trusting is the one the loader produces.
fn reload_cues(state: &Arc<LiveState>, changed: &str) -> (Vec<CueView>, CueSheet) {
    let lines = state.lines.lock().unwrap();
    let mut cues: Vec<CueView> = Vec::new();
    let mut sheets: Vec<CueSheet> = Vec::new();
    // Every sheet is re-read so the server's own copy stays whole, but only the changed
    // one goes on the wire — see the return. A real board is a dozen lists: sound,
    // lights, video, three followspots, two stage managers, flys, surtitles,
    // automation. Measured at 766 cues that is 156 kB, which is nothing to serve once
    // and wasteful to re-send on every keystroke-sized edit over a phone tether.
    for sheet in state.sheets.lock().unwrap().iter() {
        let Some(path) = state.sheet_paths.get(&sheet.id) else {
            continue;
        };
        cues.extend(load_cues(path, &lines, &sheet.id));
        sheets.push(read_sheet_meta(path, &sheet.id).unwrap_or_else(|| sheet.clone()));
    }
    cues.sort_by_key(|c| c.line_index);
    let mine = sheets
        .iter()
        .find(|s| s.id == changed)
        .cloned()
        .unwrap_or_else(|| CueSheet {
            id: changed.to_string(),
            name: changed.to_string(),
            categories: Vec::new(),
        });
    let just_changed: Vec<CueView> = cues.iter().filter(|c| c.sheet == changed).cloned().collect();
    *state.cues.lock().unwrap() = cues;
    *state.sheets.lock().unwrap() = sheets;
    (just_changed, mine)
}

/// The HTTP side: the page, the script, and the position stream.
fn serve_http(state: Arc<LiveState>, port: u16) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        use axum::extract::ws::{Message, WebSocketUpgrade};
        use axum::extract::State;
        use axum::response::{Html, IntoResponse};
        use axum::routing::get;

        let app = axum::Router::new()
            // No-store, because this page is being iterated on during a live run and
            // a browser holding yesterday's copy looks exactly like a change that did
            // not work — which cost one round of "it still overlaps".
            //
            // Read from disk when the file is there, falling back to the copy compiled
            // in. `include_str!` bakes the page at build time, so anyone editing it —
            // including the operator — saw nothing change until the next cargo build,
            // which is indistinguishable from an edit that did not take.
            .route(
                "/",
                get(|| async {
                    let live = std::path::Path::new(ASSET_PATH);
                    let body = std::fs::read_to_string(live)
                        .unwrap_or_else(|_| include_str!("../../assets/live.html").to_string());
                    (
                        [(axum::http::header::CACHE_CONTROL, "no-store, must-revalidate")],
                        Html(body),
                    )
                }),
            )
            .route(
                "/script.json",
                get(|State(s): State<Arc<LiveState>>| async move {
                    axum::Json(s.lines.lock().unwrap().clone())
                }),
            )
            .route(
                "/cues.json",
                get(|State(s): State<Arc<LiveState>>| async move {
                    axum::Json(s.cues.lock().unwrap().clone())
                }),
            )
            .route(
                "/sheets.json",
                get(|State(s): State<Arc<LiveState>>| async move {
                    axum::Json(s.sheets.lock().unwrap().clone())
                }),
            )
            .route(
                "/ws",
                get(
                    |ws: WebSocketUpgrade, State(s): State<Arc<LiveState>>| async move {
                        ws.on_upgrade(move |socket| async move {
                            // Split so a correction arriving from the operator is not
                            // stuck behind a position update waiting on playback.
                            use futures_util::{SinkExt, StreamExt};
                            let (sink, mut stream) = socket.split();
                            let inbound = Arc::clone(&s);
                            tokio::spawn(async move {
                                while let Some(Ok(msg)) = stream.next().await {
                                    let Message::Text(text) = msg else { continue };
                                    let Ok(v): Result<serde_json::Value, _> =
                                        serde_json::from_str(&text)
                                    else {
                                        continue;
                                    };
                                    let kind = v.get("type").and_then(|t| t.as_str());
                                    // Cue messages are addressed by cue id, not by
                                    // line, so they are handled before the line-index
                                    // requirement below — which would otherwise drop
                                    // every one of them on the floor without a word.
                                    // Which list an edit concerns. Independent lists
                                    // mean an edit is meaningless without one, so a
                                    // message that omits it is dropped rather than
                                    // guessed at — guessing writes a lighting
                                    // correction into the sound sheet.
                                    let sheet_of = |v: &serde_json::Value| -> Option<(String, PathBuf)> {
                                        let id = v.get("sheet")?.as_str()?.to_string();
                                        let path = inbound.sheet_paths.get(&id)?.clone();
                                        Some((id, path))
                                    };
                                    if kind == Some("edit_categories") {
                                        let Some((id, path)) = sheet_of(&v) else {
                                            continue;
                                        };
                                        let cats: Vec<serde_json::Value> = v
                                            .get("categories")
                                            .and_then(|c| c.as_array())
                                            .cloned()
                                            .unwrap_or_default();
                                        if let Err(e) = write_categories(&path, &cats) {
                                            eprintln!("could not save the targets: {e:#}");
                                        }
                                        let (fresh, list) = reload_cues(&inbound, &id);
                                        let _ = inbound.tx.send(Update::CuesChanged {
                                            sheet: id,
                                            cues: fresh,
                                            list,
                                        });
                                        continue;
                                    }
                                    if matches!(kind, Some("edit_cue" | "delete_cue" | "insert_cue"))
                                    {
                                        let str_of = |k: &str| {
                                            v.get(k).and_then(|x| x.as_str()).map(str::to_string)
                                        };
                                        let Some((sheet, path)) = sheet_of(&v) else {
                                            continue;
                                        };
                                        let id = str_of("id").unwrap_or_default();
                                        let outcome = match kind {
                                            Some("insert_cue") => v
                                                .get("lineId")
                                                .and_then(|x| x.as_str())
                                                .context("insert_cue needs a lineId")
                                                .and_then(|lid| append_cue(&path, lid))
                                                .map(|_| ()),
                                            Some("delete_cue") => write_cue(
                                                &path,
                                                &id,
                                                None,
                                                None,
                                                None,
                                                None,
                                                None,
                                                None,
                                                None,
                                                true,
                                            ),
                                            _ => write_cue(
                                                &path,
                                                &id,
                                                str_of("cue").as_deref(),
                                                str_of("trigger").as_deref(),
                                                str_of("triggerText").as_deref(),
                                                v.get("triggerNth").and_then(|x| x.as_u64()),
                                                str_of("colour").as_deref(),
                                                Some((
                                                    str_of("firedBy").unwrap_or_default(),
                                                    str_of("via").unwrap_or_default(),
                                                )),
                                                str_of("lineId").as_deref(),
                                                false,
                                            ),
                                        };
                                        match outcome {
                                            Err(e) => eprintln!("cue edit failed: {e:#}"),
                                            Ok(()) => {
                                                // Re-read rather than patch in place.
                                                // The trigger phrase is derived from
                                                // the line's text, the sort is by line,
                                                // and a re-anchor changes both — so the
                                                // only copy worth trusting is the one
                                                // the loader produces.
                                                let (fresh, list) =
                                                    reload_cues(&inbound, &sheet);
                                                let _ = inbound.tx.send(
                                                    Update::CuesChanged {
                                                        sheet,
                                                        cues: fresh,
                                                        list,
                                                    },
                                                );
                                            }
                                        }
                                        continue;
                                    }
                                    let Some(i) = v
                                        .get("lineIndex")
                                        .and_then(|i| i.as_u64())
                                        .map(|i| i as usize)
                                        .filter(|i| *i < inbound.lines.lock().unwrap().len())
                                    else {
                                        continue;
                                    };
                                    match kind {
                                        // Prep only, and not out of caution. An insert
                                        // moves every index after it, while the running
                                        // tracker holds a `PreparedScript` built once
                                        // for the whole run — so mid-show it would put
                                        // the page's second half on the wrong text
                                        // while reporting perfect confidence. Editing a
                                        // line's words is safe because indices do not
                                        // move; adding one is not.
                                        Some("insert_line") if inbound.prep => {
                                            let line = {
                                                let mut lines = inbound.lines.lock().unwrap();
                                                let after = i.min(lines.len().saturating_sub(1));
                                                let neighbour = &lines[after];
                                                let line = LineView {
                                                    id: insert_id(&lines, after),
                                                    character: v
                                                        .get("character")
                                                        .and_then(|c| c.as_str())
                                                        .unwrap_or(&neighbour.character)
                                                        .to_string(),
                                                    text: v
                                                        .get("text")
                                                        .and_then(|t| t.as_str())
                                                        .unwrap_or("")
                                                        .to_string(),
                                                    scene: neighbour.scene.clone(),
                                                    cut: false,
                                                    flag: false,
                                                    kind: v
                                                        .get("kind")
                                                        .and_then(|k| k.as_str())
                                                        .filter(|k| *k == "stage")
                                                        .unwrap_or("dialogue")
                                                        .to_string(),
                                                    // A new stage direction is unvoiced
                                                    // until said otherwise. This is the
                                                    // one place that default belongs —
                                                    // at creation, never on an edit.
                                                    spoken: v
                                                        .get("spoken")
                                                        .and_then(|b| b.as_bool())
                                                        .unwrap_or(
                                                            v.get("kind")
                                                                .and_then(|k| k.as_str())
                                                                != Some("stage"),
                                                        ),
                                                    hold: v
                                                        .get("hold")
                                                        .and_then(|h| h.as_str())
                                                        .filter(|h| {
                                                            matches!(
                                                                *h,
                                                                "silence" | "music" | "adlib"
                                                            )
                                                        })
                                                        .map(str::to_string),
                                                };
                                                lines.insert(after + 1, line.clone());
                                                line
                                            };
                                            if let Err(e) = write_line_insert(
                                                &inbound.script_path,
                                                i,
                                                &line,
                                            ) {
                                                eprintln!("could not save the new line: {e:#}");
                                            } else {
                                                println!("inserted {} after line {}", line.id, i + 1);
                                            }
                                            let _ = inbound.tx.send(Update::LineInserted {
                                                line_index: i + 1,
                                                line,
                                            });
                                        }
                                        // Prep only, for the same reason as the
                                        // insert: it moves every index after it.
                                        Some("delete_line") if inbound.prep => {
                                            let gone = {
                                                let mut lines = inbound.lines.lock().unwrap();
                                                // The last line has to stay: an empty
                                                // script is a state with no way back
                                                // through this interface.
                                                if lines.len() <= 1 {
                                                    continue;
                                                }
                                                lines.remove(i)
                                            };
                                            if let Err(e) =
                                                write_line_delete(&inbound.script_path, i)
                                            {
                                                eprintln!("could not delete the line: {e:#}");
                                            } else {
                                                println!(
                                                    "deleted {} — {:?}",
                                                    gone.id,
                                                    gone.text.chars().take(48).collect::<String>()
                                                );
                                            }
                                            let _ = inbound
                                                .tx
                                                .send(Update::LineDeleted { line_index: i });
                                        }
                                        // Allowed during a show, unlike insert and
                                        // delete: it moves no indices and changes no
                                        // text, so nothing the run depends on moves.
                                        Some("flag_line") => {
                                            let flag = {
                                                let mut lines = inbound.lines.lock().unwrap();
                                                let f = v
                                                    .get("flag")
                                                    .and_then(|f| f.as_bool())
                                                    .unwrap_or(!lines[i].flag);
                                                lines[i].flag = f;
                                                f
                                            };
                                            if let Err(e) =
                                                write_line_flag(&inbound.script_path, i, flag)
                                            {
                                                eprintln!("could not save the flag: {e:#}");
                                            }
                                            let _ = inbound.tx.send(Update::LineFlagged {
                                                line_index: i,
                                                flag,
                                            });
                                        }
                                        Some("position_jump") => {
                                            inbound.steer.lock().unwrap().push(i);
                                        }
                                        Some("edit_line") => {
                                            let Some(text) = v
                                                .get("text")
                                                .and_then(|t| t.as_str())
                                                .map(|t| t.trim().to_string())
                                                .filter(|t| !t.is_empty())
                                            else {
                                                continue;
                                            };
                                            // The speaker is corrected as often as the
                                            // words: an attribution imported as
                                            // dialogue is the commonest importer fault,
                                            // and fixing only the text would leave the
                                            // line spoken by whoever came before.
                                            let character = v
                                                .get("character")
                                                .and_then(|c| c.as_str())
                                                .map(|c| c.trim().to_string())
                                                .filter(|c| !c.is_empty());
                                            // Absent means "leave it as it was", so
                                            // an edit that only fixes a typo does not
                                            // silently un-cut a struck line.
                                            let cut = v.get("cut").and_then(|c| c.as_bool());
                                            // `hold` carries three states, not two:
                                            // absent leaves it alone, null clears it,
                                            // a string sets it. Collapsing null into
                                            // absent would make a hold impossible to
                                            // remove once placed.
                                            let hold_given = v.get("hold");
                                            let kind = v
                                                .get("kind")
                                                .and_then(|k| k.as_str())
                                                .filter(|k| *k == "stage" || *k == "dialogue")
                                                .map(str::to_string);
                                            let spoken = v.get("spoken").and_then(|b| b.as_bool());
                                            let edit = {
                                                let mut lines = inbound.lines.lock().unwrap();
                                                let l = &mut lines[i];
                                                l.text = text.clone();
                                                if let Some(c) = &character {
                                                    l.character = c.clone();
                                                }
                                                if let Some(c) = cut {
                                                    l.cut = c;
                                                }
                                                if let Some(k) = kind {
                                                    l.kind = k;
                                                }
                                                if let Some(b) = spoken {
                                                    l.spoken = b;
                                                }
                                                if let Some(h) = hold_given {
                                                    l.hold = h
                                                        .as_str()
                                                        .filter(|h| {
                                                            matches!(
                                                                *h,
                                                                "silence" | "music" | "adlib"
                                                            )
                                                        })
                                                        .map(str::to_string);
                                                }
                                                LineEdit {
                                                    text: l.text.clone(),
                                                    character: character.clone(),
                                                    cut: l.cut,
                                                    kind: l.kind.clone(),
                                                    spoken: l.spoken,
                                                    hold: l.hold.clone(),
                                                }
                                            };
                                            if let Err(e) =
                                                write_line_edit(&inbound.script_path, i, &edit)
                                            {
                                                eprintln!("could not save the edit: {e:#}");
                                            } else {
                                                println!("edited line {}: {text}", i + 1);
                                            }
                                            let _ = inbound.tx.send(Update::LineEdited {
                                                line_index: i,
                                                text,
                                                character,
                                                cut: edit.cut,
                                                kind: edit.kind,
                                                spoken: edit.spoken,
                                                hold: edit.hold,
                                            });
                                        }
                                        _ => {}
                                    }
                                }
                            });
                            let mut socket = sink;
                            // Full state first, so a client that joins mid-show — or
                            // rejoins after a dropped connection — starts correct
                            // rather than waiting for the next segment.
                            let mut rx = s.tx.subscribe();
                            if let Ok(text) = serde_json::to_string(&s.hello()) {
                                if socket.send(Message::Text(text.into())).await.is_err() {
                                    return;
                                }
                            }
                            loop {
                                match rx.recv().await {
                                    Ok(update) => {
                                        let Ok(text) = serde_json::to_string(&update) else {
                                            continue;
                                        };
                                        if socket.send(Message::Text(text.into())).await.is_err() {
                                            return;
                                        }
                                    }
                                    // A client too slow to keep up is resynced rather
                                    // than disconnected: position is state, and the
                                    // latest one is all it ever needed.
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                        if let Ok(text) = serde_json::to_string(&s.hello()) {
                                            if socket.send(Message::Text(text.into())).await.is_err()
                                            {
                                                return;
                                            }
                                        }
                                    }
                                    Err(_) => return,
                                }
                            }
                        })
                        .into_response()
                    },
                ),
            )
            .with_state(Arc::clone(&state));

        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        println!("\n  watch it at http://localhost:{port}   (ctrl-c to stop)\n");
        // Serving outlives the run on purpose: when the show ends the page should
        // still be there, showing where it finished, rather than going blank.
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        Ok::<(), anyhow::Error>(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_highlighted_phrase_is_found_in_the_line_it_belongs_to() {
        // The evidence is text lifted off the PDF around the highlight, so it carries
        // the source's accents and punctuation while the script carries the
        // playwright's — and it usually overruns into whatever was printed alongside.
        let line = "« Arrive, par la droite, Agamemnon. »";
        // The phrase ends at the last character that folds to something: trailing
        // punctuation is not evidence of anything and would only make the highlight
        // on the page reach past the word the operator is listening for.
        assert_eq!(
            trigger_phrase("droite, Agamemnon. » 4 musique pulsation", line).as_deref(),
            Some("droite, Agamemnon")
        );
        // Case and accents fold; the phrase comes back in the line's own characters,
        // because that is what the page has to find in the text it is showing.
        assert_eq!(
            trigger_phrase("BAILLONNEZ-MOI, J AI DIT", "« Bâillonnez-moi. J’ai dit ce qu’il fallait. »")
                .as_deref(),
            Some("Bâillonnez-moi. J’ai dit")
        );
    }

    #[test]
    fn a_page_level_mark_yields_no_phrase_rather_than_a_guess() {
        // 96 of Hécube's 133 cues come from marks with no highlighted phrase behind
        // them. Pointing at an incidental scrap of shared text would be worse than
        // pointing at the line: it would look like knowledge.
        assert_eq!(
            trigger_phrase("CONDUITE SON", "Nous avons lu la scène où Polyxène affronte son destin."),
            None
        );
        assert_eq!(trigger_phrase("", "Silence, mes amies."), None);
        // Common short words must not qualify — "des " is a run of four.
        assert_eq!(trigger_phrase("des", "Il faut des figurants ?"), None);
    }
}

#[cfg(test)]
mod occurrence_tests {
    /// The same phrase three times in one speech, which is ordinary in this play —
    /// "Comme ça." / "Comme ça ?" / "Comme ça." sit consecutively in Hécube, and
    /// scene 4 asks one question eight ways. A cue on the third is not a cue on the
    /// first, and finding the phrase by text alone always returns the first.
    #[test]
    fn counting_occurrences_distinguishes_repeated_phrases() {
        let line = "Comme ça. Puis comme ça. Et enfin comme ça, voilà.";
        let phrase = "comme ça";
        let found: Vec<usize> = line.match_indices(phrase).map(|(i, _)| i).collect();
        assert_eq!(found.len(), 2, "case-sensitive, so the capitalised one is separate");
        // The nth is reachable and distinct.
        assert_ne!(found[0], found[1]);
        // And a count beyond what survives falls back to the first rather than to
        // nothing, which is what `load_cues` does when a line has been edited.
        let nth = 5usize;
        let use_nth = if nth < found.len() { nth } else { 0 };
        assert_eq!(use_nth, 0);
    }
}
