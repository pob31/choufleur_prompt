//! Where shows live, and how one comes into being.
//!
//! Until now a show was wherever somebody happened to put a manifest, and creating one
//! meant running two Python scripts and hand-writing a JSON file whose `cues` field is
//! silently ignored. That is fine for a corpus assembled by its author and useless for
//! anyone else, including its author in a year.
//!
//! A library is a directory of show directories. The layout is deliberately the one
//! already in use, plus two additions:
//!
//! ```text
//! <root>/<Show Name>/
//!   manifest.json        generated, never hand-written
//!   script.json          the script, in the existing format
//!   cues/<sheet>.json    one file per cue list           ← new location
//!   versions/<stamp>/    snapshots, written by `Store`   ← new
//! ```
//!
//! **No format changes live here.** The file shapes are exactly what `serve` already
//! reads; this module is about location and lifecycle. Retyping the show file is M1.1 and
//! wants its own plan, and doing it by accident inside a directory reshuffle is how a
//! corpus gets stranded.
//!
//! Cue sheets move into `cues/` because discovery beside the script is a glob over
//! `cues*.json`, which means the operator's own `cues.backup-20260815.json` is one
//! filename away from being loaded as a live list. A directory says what belongs.
//! Sheets beside the script are still found, because three shows already store them that
//! way and breaking them to tidy a convention would be the wrong trade.
//!
//! Audio is referenced, never copied. The corpus is 3.5 GB and lives on external storage
//! half the time; a library that insisted on owning it would be a library nobody put
//! their shows in.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// One show, as the Shows screen wants to list it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The directory name, which is also the show's name. One less thing to disagree.
    pub name: String,
    pub dir: PathBuf,
    pub manifest: PathBuf,
    pub script: PathBuf,
    /// From the script, when it has one — a show is usually recognised by its title
    /// rather than by the folder somebody typed in a hurry.
    pub title: Option<String>,
    pub lines: usize,
    /// Cue list files, in `cues/` and beside the script both.
    pub sheets: Vec<PathBuf>,
    pub versions: usize,
}

/// A directory of shows.
pub struct Library {
    root: PathBuf,
}

impl Library {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every show in the library, by name.
    ///
    /// A directory without a readable script is skipped rather than reported: the
    /// library root is a place people will drop things, and a stray folder should not
    /// stop the list rendering.
    pub fn list(&self) -> Result<Vec<Entry>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("reading the library at {}", self.root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(show) = self.describe(&entry.path()) {
                out.push(show);
            }
        }
        out.sort_by_key(|e| e.name.to_lowercase());
        Ok(out)
    }

    /// Look at one directory and decide whether it is a show.
    pub fn describe(&self, dir: &Path) -> Option<Entry> {
        let script = dir.join("script.json");
        let doc: serde_json::Value = serde_json::from_slice(&fs::read(&script).ok()?).ok()?;
        let lines = doc.get("lines")?.as_array()?.len();
        let title = doc
            .get("title")
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .filter(|t| !t.trim().is_empty());
        Some(Entry {
            name: dir.file_name()?.to_string_lossy().into_owned(),
            manifest: dir.join("manifest.json"),
            sheets: sheet_files(dir),
            versions: fs::read_dir(dir.join("versions"))
                .map(|d| d.filter_map(|e| e.ok()).count())
                .unwrap_or(0),
            script,
            title,
            lines,
            dir: dir.to_path_buf(),
        })
    }

    /// Make an empty show, ready for text to be imported into it.
    ///
    /// It gets one blank line rather than none. An empty script is a state the editor
    /// cannot get you out of — deletion refuses to remove the last line, and with no
    /// lines there is nothing to click to insert beside.
    pub fn create(&self, name: &str) -> Result<Entry> {
        let name = clean_name(name)?;
        let dir = self.root.join(&name);
        if dir.exists() {
            bail!("a show called {name:?} is already in the library");
        }
        fs::create_dir_all(dir.join("cues"))
            .with_context(|| format!("creating {}", dir.display()))?;

        let script = serde_json::json!({
            "format": "choufleur-script",
            "formatVersion": "0.1",
            "title": name,
            "defaultLang": ["fr"],
            "acts": [],
            "scenes": [],
            "characters": [],
            "lines": [{
                "id": "L-0001",
                "act": "act-1",
                "scene": "scene-1",
                "character": "",
                "text": "",
            }],
        });
        write_new(&dir.join("script.json"), &script)?;
        write_new(&dir.join("manifest.json"), &manifest_for(&name))?;

        self.describe(&dir)
            .context("the show was written but could not be read back")
    }

    /// Copy an existing show into the library, leaving the original alone.
    ///
    /// Copying rather than moving, and never touching the source, because the shows
    /// worth importing first are the ones in `test_Choufleur/` — gitignored, irreplaceable
    /// and the product of hours of hand-prepping. An importer that relocated them would
    /// be one bug away from being the last thing that ever read them.
    ///
    /// Audio is not copied. The manifest's channel list is carried over with its paths
    /// made absolute, so a show that pointed at 3.5 GB of recordings still points at it.
    pub fn import(&self, manifest: &Path, name: Option<&str>) -> Result<Entry> {
        let src_doc: serde_json::Value = serde_json::from_slice(
            &fs::read(manifest).with_context(|| format!("reading {}", manifest.display()))?,
        )
        .with_context(|| format!("parsing {}", manifest.display()))?;
        let src_dir = manifest.parent().unwrap_or(Path::new("."));

        let src_script = src_dir.join(
            src_doc
                .get("script")
                .and_then(|s| s.as_str())
                .context("that manifest names no script")?,
        );
        if !src_script.exists() {
            bail!("{} does not exist", src_script.display());
        }

        let name = clean_name(name.map(str::to_string).unwrap_or_else(|| {
            src_doc
                .get("show")
                .or_else(|| src_doc.get("title"))
                .and_then(|t| t.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    src_dir
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Untitled".into())
                })
        }))?;

        let dir = self.root.join(&name);
        if dir.exists() {
            bail!("a show called {name:?} is already in the library");
        }
        fs::create_dir_all(dir.join("cues"))?;
        fs::copy(&src_script, dir.join("script.json"))
            .with_context(|| format!("copying {}", src_script.display()))?;

        // Every cue list the source had, into `cues/`. The source may keep them beside
        // the script, in a `cues/` directory, or both.
        for sheet in sheet_files(src_dir) {
            let Some(file) = sheet.file_name() else { continue };
            fs::copy(&sheet, dir.join("cues").join(file))
                .with_context(|| format!("copying {}", sheet.display()))?;
        }

        // Two manifest shapes exist and the loader sniffs between them: a `channels`
        // array means the corpus form, which also requires `show` and `act`. Carrying
        // the channels over onto the simple form produced a file that parsed as neither
        // and would not open at all.
        let channels = src_doc.get("channels").and_then(|c| c.as_array());
        let mut manifest_doc = match channels {
            None => manifest_for(&name),
            Some(_) => serde_json::json!({
                "format": "choufleur-corpus",
                "formatVersion": "0.1",
                "show": name,
                "act": src_doc.get("act").and_then(|a| a.as_str()).unwrap_or("run"),
                "sampleRate": 48000,
                "script": "script.json",
            }),
        };
        // The character↔channel patch, with audio resolved where it stands. Losing it
        // would silently turn a six-mic show into a single zone feed.
        if let Some(channels) = channels {
            manifest_doc["channels"] = serde_json::Value::Array(
                channels.iter().map(|c| absolute_audio(c, src_dir)).collect(),
            );
        }
        for key in ["sampleRate", "mixdown", "groundTruth", "provenance", "note"] {
            if let Some(v) = src_doc.get(key) {
                manifest_doc[key] = match key {
                    "mixdown" => absolute_audio(v, src_dir),
                    // Ground truth is a sibling file we did not copy, so it is only
                    // meaningful as a path back to where it actually is.
                    "groundTruth" => v
                        .as_str()
                        .map(|p| absolute(p, src_dir))
                        .unwrap_or_else(|| v.clone()),
                    _ => v.clone(),
                };
            }
        }
        write_new(&dir.join("manifest.json"), &manifest_doc)?;

        self.describe(&dir)
            .context("the show was imported but could not be read back")
    }
}

/// Cue lists belonging to a show: everything in `cues/`, plus the older convention of
/// `cues*.json` beside the script.
///
/// Hand-made backups are excluded by name, as they were before — `find_cue_files` has
/// skipped anything containing "backup" since the operator started making them, and a
/// backup loaded as a live list is a second set of cues nobody asked for.
pub fn sheet_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut consider = |p: PathBuf, require_prefix: bool| {
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return;
        };
        if !name.ends_with(".json") || name.contains("backup") {
            return;
        }
        let stem = name.trim_end_matches(".json");
        if require_prefix && !(stem == "cues" || stem.starts_with("cues-")) {
            return;
        }
        out.push(p);
    };
    if let Ok(entries) = fs::read_dir(dir.join("cues")) {
        for e in entries.flatten() {
            consider(e.path(), false);
        }
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            consider(e.path(), true);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Re-point a `{file, ...}` object's relative audio path at where it really is.
fn absolute_audio(v: &serde_json::Value, base: &Path) -> serde_json::Value {
    let mut v = v.clone();
    if let Some(f) = v.get("file").and_then(|f| f.as_str()) {
        v["file"] = absolute(f, base);
    }
    v
}

fn absolute(p: &str, base: &Path) -> serde_json::Value {
    let path = Path::new(p);
    if path.is_relative() {
        base.join(path).to_string_lossy().into_owned().into()
    } else {
        p.into()
    }
}

fn manifest_for(name: &str) -> serde_json::Value {
    serde_json::json!({
        "title": name,
        "script": "script.json",
        "sampleRate": 48000,
    })
}

/// A show name that is also a safe directory name.
///
/// Kept readable rather than slugged: the folder name *is* the show's name in the list,
/// and `Hécube pas Hécube` reads better than `hecube-pas-hecube`. Only what a filesystem
/// or a path traversal would object to is removed.
fn clean_name(name: impl AsRef<str>) -> Result<String> {
    let cleaned: String = name
        .as_ref()
        .chars()
        .map(|c| if "/\\:<>\"|?*".contains(c) || c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        bail!("a show needs a name");
    }
    Ok(cleaned)
}

fn write_new(path: &Path, doc: &serde_json::Value) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(doc)? + "\n")
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_show_is_immediately_openable() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path());
        let show = lib.create("Lovedoll").unwrap();
        assert_eq!(show.name, "Lovedoll");
        assert!(show.manifest.exists() && show.script.exists());
        // One line, not zero: the editor cannot recover from an empty script.
        assert_eq!(show.lines, 1);
        assert_eq!(lib.list().unwrap().len(), 1);
    }

    #[test]
    fn two_shows_cannot_share_a_name() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path());
        lib.create("Lovedoll").unwrap();
        assert!(lib.create("Lovedoll").unwrap_err().to_string().contains("already"));
    }

    #[test]
    fn names_stay_readable_but_cannot_escape_the_library() {
        assert_eq!(clean_name("Hécube pas Hécube").unwrap(), "Hécube pas Hécube");
        // The separators become spaces, so the result is one harmless directory name
        // rather than a path out of the library.
        let escaped = clean_name("../../etc/passwd").unwrap();
        assert_eq!(escaped, ".. .. etc passwd");
        assert!(!escaped.contains('/') && !escaped.contains('\\'));
        assert_eq!(clean_name("  spaced   out  ").unwrap(), "spaced out");
        assert!(clean_name("   ").is_err());
        assert!(clean_name("..").is_err());
    }

    #[test]
    fn importing_copies_the_show_and_leaves_the_original_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("elsewhere");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("script.json"),
            r#"{"title":"Hécube","lines":[{"id":"L-0001","text":"one"}]}"#,
        )
        .unwrap();
        fs::write(src.join("cues.json"), r#"{"cues":[]}"#).unwrap();
        fs::write(src.join("cues-lumiere.json"), r#"{"cues":[]}"#).unwrap();
        // Neither of these is a live cue list.
        fs::write(src.join("cues.backup-20260815.json"), r#"{"cues":[]}"#).unwrap();
        fs::write(src.join("notes.json"), r#"{}"#).unwrap();
        fs::write(
            src.join("manifest.json"),
            r#"{"show":"Hécube","script":"script.json","sampleRate":48000}"#,
        )
        .unwrap();

        let lib = Library::new(tmp.path().join("library"));
        fs::create_dir_all(lib.root()).unwrap();
        let show = lib.import(&src.join("manifest.json"), None).unwrap();

        assert_eq!(show.name, "Hécube");
        assert_eq!(show.lines, 1);
        let mut names: Vec<String> = show
            .sheets
            .iter()
            .map(|s| s.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["cues-lumiere.json", "cues.json"]);

        // The source is untouched — this is the rule that matters most here.
        assert!(src.join("script.json").exists());
        assert!(src.join("cues.json").exists());
        assert_eq!(fs::read_dir(&src).unwrap().count(), 6);
    }

    #[test]
    fn importing_keeps_the_channel_patch_and_points_at_the_audio_where_it_is() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("corpus");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("script.json"), r#"{"lines":[{"id":"L-1","text":"x"}]}"#).unwrap();
        fs::write(
            src.join("manifest.json"),
            r#"{"show":"La Reprise","script":"script.json","sampleRate":48000,
                "channels":[{"index":1,"file":"audio/sara.wav","character":"char-sara"},
                            {"index":2,"file":"/elsewhere/zone.wav"}]}"#,
        )
        .unwrap();

        let lib = Library::new(tmp.path().join("library"));
        fs::create_dir_all(lib.root()).unwrap();
        let show = lib.import(&src.join("manifest.json"), None).unwrap();

        let m: serde_json::Value =
            serde_json::from_slice(&fs::read(&show.manifest).unwrap()).unwrap();
        let ch = m["channels"].as_array().unwrap();
        assert_eq!(ch[0]["character"], "char-sara");
        // Relative audio is re-pointed at where it actually is; absolute is left alone.
        assert_eq!(ch[0]["file"], src.join("audio/sara.wav").to_string_lossy().as_ref());
        assert_eq!(ch[1]["file"], "/elsewhere/zone.wav");
    }

    #[test]
    fn a_stray_directory_does_not_break_the_list() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path());
        lib.create("Real Show").unwrap();
        fs::create_dir_all(tmp.path().join("Screenshots")).unwrap();
        fs::write(tmp.path().join("loose.txt"), "hello").unwrap();
        let shows = lib.list().unwrap();
        assert_eq!(shows.len(), 1);
        assert_eq!(shows[0].name, "Real Show");
    }

    #[test]
    fn an_empty_or_missing_library_lists_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Library::new(tmp.path().join("nope")).list().unwrap().is_empty());
        assert!(Library::new(tmp.path()).list().unwrap().is_empty());
    }
}
