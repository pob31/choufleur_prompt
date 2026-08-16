//! The one place a show is written to disk.
//!
//! Before this existed there were eight hand-rolled `read → mutate → write` paths, and
//! between them they had every fault the arrangement invites. They are worth naming,
//! because each one is a rule below rather than a tidying preference:
//!
//! **The lock did not span the write.** Each writer took the in-memory mutex, released
//! it, and only then read the file back off disk to modify it. Since every write
//! rewrites the whole document, two operators editing different lines at the same
//! moment would each read the same original, and the second rename would silently
//! discard the first one's correction. With two tablets in the wings that is not a
//! thought experiment.
//!
//! **Nothing checked the file was still the document we loaded.** Edits address lines by
//! index. If anything changed the script underneath a running server — another tool, a
//! script, a person — the next correction landed in whatever line now occupied that
//! index. This is exactly how a line reading `"Nadia ? Nadia ?"` became a second copy of
//! `"Un temps."`, twice in one week, and it is the reason `expect` exists below.
//!
//! **There were no backups.** The `.backup-…json` files beside the corpus were made by
//! hand; nothing in the code has ever produced one. The notation spec (§3.4) already
//! required snapshot-before-write and required the write to *refuse* if the snapshot
//! could not be taken — it was simply written for re-import and never applied to the
//! edit path, which is where the hours of hand-prepping actually accumulate.
//!
//! **The temp file had a fixed name** — `script.json.tmp`, shared by every writer of
//! that file, no exclusive create. Two writers interleaved into one temp file and the
//! survivor renamed the mixture over the show.
//!
//! **Nothing was fsynced.** Rename gives you ordering, not durability: the metadata can
//! reach the disk before the bytes do, which turns a power cut into a zero-length show.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// A saved copy of the show, as the version list wants to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// `2026-08-16T21-04-11`, UTC. Sorts lexically, which is why it is shaped this way.
    pub stamp: String,
    pub files: Vec<PathBuf>,
}

/// Guards every write to one show.
///
/// Construct it with the files it is responsible for — the script and the cue sheets.
/// Knowing the set is what lets a snapshot be exact: copying the whole directory would
/// drag in recordings and the operator's own hand-made backups, and copying only the
/// file being edited would produce a version you cannot actually go back to, because a
/// cue sheet restored without its script anchors to lines that have moved.
pub struct Store {
    root: PathBuf,
    guarded: Vec<PathBuf>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// What we last saw on disk, per file. An `edit` whose file no longer hashes to this
    /// is refused rather than applied to a document we have not seen.
    seen: HashMap<PathBuf, [u8; 32]>,
    /// This session's snapshot, taken lazily before the first write. Lazily because a
    /// server that is only ever read from should not litter the show with versions.
    snapshot: Option<String>,
    /// Distinguishes concurrent temp files within this process.
    tmp_seq: u64,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>, guarded: Vec<PathBuf>) -> Self {
        Self {
            root: root.into(),
            guarded,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The directory holding this show's snapshots.
    pub fn versions_dir(&self) -> PathBuf {
        self.root.join("versions")
    }

    /// Record what every guarded file looks like right now.
    ///
    /// Without this the staleness check has no baseline until the first write, so the
    /// one edit most likely to be aimed at a stale document — the first one after the
    /// show was loaded, possibly hours earlier — is the one edit not covered by it.
    /// A file that cannot be read is skipped rather than fatal: it may simply not exist
    /// yet, and refusing to start a show over a missing cue sheet helps nobody.
    pub fn arm(&self) {
        let mut inner = self.inner.lock().unwrap();
        for file in &self.guarded {
            if let Ok(raw) = fs::read(file) {
                inner.seen.insert(file.clone(), digest(&raw));
            }
        }
    }

    /// Read a document and remember it, so a later edit can tell whether it has moved.
    ///
    /// Loading through here rather than through a bare `read_to_string` is what arms the
    /// staleness check — with no baseline recorded, the first write of a session has
    /// nothing to compare against and must trust what it finds.
    pub fn read(&self, path: &Path) -> Result<serde_json::Value> {
        let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let doc = serde_json::from_slice(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        self.inner
            .lock()
            .unwrap()
            .seen
            .insert(path.to_path_buf(), digest(&raw));
        Ok(doc)
    }

    /// Read, modify and write one document, with nothing able to interleave.
    ///
    /// The closure sees the whole parsed document and may do as it likes to it; return
    /// an error from it to abandon the edit without writing. Everything the module
    /// doc-comment describes happens around it.
    ///
    /// Note that the document is a `serde_json::Value`, not a typed struct, and
    /// deliberately: a field this build does not model — a future one, or a note left by
    /// one of the prep scripts — survives a round trip untouched. Typing it would make
    /// every edit a silent migration.
    pub fn edit<T>(
        &self,
        path: &Path,
        f: impl FnOnce(&mut serde_json::Value) -> Result<T>,
    ) -> Result<T> {
        let mut inner = self.inner.lock().unwrap();

        let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let found = digest(&raw);
        if let Some(expected) = inner.seen.get(path) {
            if *expected != found {
                bail!(
                    "{} changed on disk since we last read it — refusing to write, \
                     because edits address lines by position and the positions may have \
                     moved. Reload the show.",
                    path.display()
                );
            }
        }

        let mut doc: serde_json::Value = serde_json::from_slice(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;

        // Before the first change of the session, and only then. Refusing on failure is
        // the point: a backup that quietly does not happen is worse than none at all,
        // because you stop checking for it.
        if inner.snapshot.is_none() {
            let stamp = self
                .take_snapshot()
                .context("refusing to edit: this show's snapshot could not be written")?;
            inner.snapshot = Some(stamp);
        }

        let out = f(&mut doc)?;

        inner.tmp_seq += 1;
        let seq = inner.tmp_seq;
        let bytes = serde_json::to_string_pretty(&doc)? + "\n";
        write_atomic(path, bytes.as_bytes(), seq)?;
        inner
            .seen
            .insert(path.to_path_buf(), digest(bytes.as_bytes()));
        Ok(out)
    }

    /// Copy every guarded file into `versions/<stamp>/`, and report the stamp.
    ///
    /// The stamp is resolved to a directory that does not exist yet. Seconds are coarse
    /// enough to collide — a restore snapshots the current state on its way past, and
    /// that lands in the same second as an edit often enough — and writing into an
    /// existing snapshot would overwrite the very copy somebody is about to restore.
    /// Caught by `restore_brings_the_old_text_back`, which failed exactly that way.
    fn take_snapshot(&self) -> Result<String> {
        let base = utc_stamp(SystemTime::now());
        let versions = self.versions_dir();
        fs::create_dir_all(&versions)
            .with_context(|| format!("creating {}", versions.display()))?;

        let mut stamp = base.clone();
        let mut dir = versions.join(&stamp);
        for n in 2.. {
            match fs::create_dir(&dir) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    stamp = format!("{base}-{n}");
                    dir = versions.join(&stamp);
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("creating {}", dir.display()))
                }
            }
        }
        for file in &self.guarded {
            // A guarded file that does not exist yet is not an error — a show may be
            // saving its first cue sheet.
            if !file.exists() {
                continue;
            }
            let name = file
                .file_name()
                .context("a guarded path with no file name")?;
            fs::copy(file, dir.join(name))
                .with_context(|| format!("copying {} into the snapshot", file.display()))?;
        }
        // The snapshot is only a backup once it is actually on the platter.
        File::open(&dir)?.sync_all()?;
        Ok(stamp)
    }

    /// Every snapshot this show has, newest first.
    pub fn versions(&self) -> Result<Vec<Version>> {
        let dir = self.versions_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let stamp = entry.file_name().to_string_lossy().into_owned();
            let mut files: Vec<PathBuf> = fs::read_dir(entry.path())?
                .filter_map(|f| f.ok().map(|f| f.path()))
                .collect();
            files.sort();
            out.push(Version { stamp, files });
        }
        // The stamp is shaped so that lexical order is chronological order.
        out.sort_by(|a, b| b.stamp.cmp(&a.stamp));
        Ok(out)
    }

    /// Put a snapshot back, having first snapshotted what is there now.
    ///
    /// Restoring is itself a destructive write — it is the one people reach for in a
    /// hurry, and the one most likely to be aimed at the wrong version — so the current
    /// state is preserved before it happens and appears in the list as its own entry.
    /// Going back from a restore is then the same operation again.
    pub fn restore(&self, stamp: &str) -> Result<()> {
        let from = self.versions_dir().join(stamp);
        if !from.is_dir() {
            bail!("no snapshot called {stamp}");
        }
        let mut inner = self.inner.lock().unwrap();
        self.take_snapshot()
            .context("refusing to restore: the current state could not be snapshotted first")?;

        for file in &self.guarded {
            let Some(name) = file.file_name() else { continue };
            let saved = from.join(name);
            if !saved.exists() {
                continue;
            }
            let bytes = fs::read(&saved)?;
            inner.tmp_seq += 1;
            let seq = inner.tmp_seq;
            write_atomic(file, &bytes, seq)?;
            inner.seen.insert(file.clone(), digest(&bytes));
        }
        // The next edit takes a fresh snapshot: the restored state is a new starting
        // point, and conflating it with the pre-restore one would lose whichever of the
        // two somebody actually wanted.
        inner.snapshot = None;
        Ok(())
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// Write bytes so that a reader sees either the old file or the new one, never a mixture
/// and never a truncation.
///
/// The temp name carries the process id and a counter because the previous fixed
/// `.json.tmp` was shared by every writer of that file.
fn write_atomic(path: &Path, bytes: &[u8], seq: u64) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "show".into());
    let tmp = dir.join(format!(".{stem}.{}.{seq}.tmp", std::process::id()));

    let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(bytes)?;
    // The bytes, then the rename, then the directory that records the rename. Skipping
    // the last one leaves a durable pointer to a file whose contents never arrived.
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming into {}", path.display()))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// `2026-08-16T21-04-11`, UTC, filename-safe.
///
/// Hand-rolled because the workspace has no date library and this is the only place one
/// would be needed. The civil-from-days conversion is Howard Hinnant's, which is exact
/// for any date this program will ever see.
fn utc_stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);

    format!("{y:04}-{m:02}-{d:02}T{h:02}-{mi:02}-{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn show(dir: &Path) -> (Store, PathBuf) {
        let script = dir.join("script.json");
        fs::write(&script, r#"{"lines":[{"id":"L-0001","text":"one"}]}"#).unwrap();
        let store = Store::new(dir, vec![script.clone()]);
        (store, script)
    }

    #[test]
    fn edit_writes_and_snapshots_the_previous_text() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, script) = show(tmp.path());

        store
            .edit(&script, |doc| {
                doc["lines"][0]["text"] = "two".into();
                Ok(())
            })
            .unwrap();

        let now: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&script).unwrap()).unwrap();
        assert_eq!(now["lines"][0]["text"], "two");

        // The snapshot holds what was there *before* the edit — the property that makes
        // it worth having.
        let versions = store.versions().unwrap();
        assert_eq!(versions.len(), 1);
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&versions[0].files[0]).unwrap()).unwrap();
        assert_eq!(saved["lines"][0]["text"], "one");
    }

    #[test]
    fn only_one_snapshot_per_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, script) = show(tmp.path());
        for n in 0..3 {
            store
                .edit(&script, |doc| {
                    doc["lines"][0]["text"] = n.into();
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(store.versions().unwrap().len(), 1);
    }

    #[test]
    fn an_edit_is_refused_when_the_file_moved_underneath_us() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, script) = show(tmp.path());
        store.read(&script).unwrap();

        // Somebody else rewrites the script — the prep script, another tool, a person.
        fs::write(&script, r#"{"lines":[{"id":"L-0009","text":"different"}]}"#).unwrap();

        let err = store
            .edit(&script, |doc| {
                doc["lines"][0]["text"] = "clobbered".into();
                Ok(())
            })
            .unwrap_err();
        assert!(err.to_string().contains("changed on disk"), "{err}");

        // And the other writer's content is still there.
        let now = fs::read_to_string(&script).unwrap();
        assert!(now.contains("different"), "{now}");
    }

    #[test]
    fn arm_covers_the_very_first_edit_of_a_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, script) = show(tmp.path());
        store.arm();
        // Nothing has been read or written through the store since; without `arm` the
        // first edit would have no baseline and would overwrite this happily.
        fs::write(&script, r#"{"lines":[{"id":"L-0009","text":"someone else"}]}"#).unwrap();
        let err = store
            .edit(&script, |doc| {
                doc["lines"][0]["text"] = "clobbered".into();
                Ok(())
            })
            .unwrap_err();
        assert!(err.to_string().contains("changed on disk"), "{err}");
        assert!(fs::read_to_string(&script).unwrap().contains("someone else"));
    }

    #[test]
    fn a_failing_edit_leaves_the_file_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, script) = show(tmp.path());
        let before = fs::read_to_string(&script).unwrap();
        let err = store
            .edit(&script, |_| bail!("no such line") as Result<()>)
            .unwrap_err();
        assert!(err.to_string().contains("no such line"));
        assert_eq!(fs::read_to_string(&script).unwrap(), before);
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("script.json");
        fs::write(
            &script,
            r#"{"lines":[{"id":"L-0001","text":"one","page":12}],"somethingNew":true}"#,
        )
        .unwrap();
        let store = Store::new(tmp.path(), vec![script.clone()]);
        store
            .edit(&script, |doc| {
                doc["lines"][0]["text"] = "two".into();
                Ok(())
            })
            .unwrap();
        let now: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&script).unwrap()).unwrap();
        assert_eq!(now["lines"][0]["page"], 12);
        assert_eq!(now["somethingNew"], true);
    }

    #[test]
    fn key_order_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("script.json");
        fs::write(&script, "{\n \"text\": \"one\",\n \"act\": \"a\",\n \"id\": \"L-1\"\n}")
            .unwrap();
        let store = Store::new(tmp.path(), vec![script.clone()]);
        store
            .edit(&script, |doc| {
                doc["text"] = "two".into();
                Ok(())
            })
            .unwrap();
        let now = fs::read_to_string(&script).unwrap();
        let order: Vec<&str> = ["text", "act", "id"]
            .into_iter()
            .filter(|k| now.contains(&format!("\"{k}\"")))
            .collect();
        assert_eq!(order, ["text", "act", "id"]);
        // Alphabetising would put "act" first; a version diff would then show every
        // line as changed on the first edit.
        assert!(now.find("\"text\"") < now.find("\"act\""), "{now}");
    }

    #[test]
    fn restore_brings_the_old_text_back_and_keeps_the_new_one() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, script) = show(tmp.path());
        store
            .edit(&script, |doc| {
                doc["lines"][0]["text"] = "two".into();
                Ok(())
            })
            .unwrap();
        let stamp = store.versions().unwrap()[0].stamp.clone();

        store.restore(&stamp).unwrap();
        let now: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&script).unwrap()).unwrap();
        assert_eq!(now["lines"][0]["text"], "one");

        // Restoring snapshotted "two" on the way past, so the restore is undoable too.
        let versions = store.versions().unwrap();
        assert_eq!(versions.len(), 2);
        let newest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&versions[0].files[0]).unwrap()).unwrap();
        assert_eq!(newest["lines"][0]["text"], "two");
    }

    #[test]
    fn an_edit_that_cannot_be_snapshotted_does_not_happen() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, script) = show(tmp.path());
        // A file where the versions directory needs to go: creating it must fail.
        fs::write(tmp.path().join("versions"), "in the way").unwrap();
        let store = Store::new(tmp.path(), vec![script.clone()]);

        let err = store
            .edit(&script, |doc| {
                doc["lines"][0]["text"] = "two".into();
                Ok(())
            })
            .unwrap_err();
        assert!(err.to_string().contains("snapshot"), "{err}");
        let now = fs::read_to_string(&script).unwrap();
        assert!(now.contains("one"), "the edit must not have landed: {now}");
    }

    #[test]
    fn no_temp_files_are_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, script) = show(tmp.path());
        store.edit(&script, |_| Ok(())).unwrap();
        let strays: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
    }

    #[test]
    fn two_snapshots_in_one_second_do_not_overwrite_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, script) = show(tmp.path());
        for text in ["two", "three"] {
            store
                .edit(&script, |doc| {
                    doc["lines"][0]["text"] = text.into();
                    Ok(())
                })
                .unwrap();
            // Force a second snapshot without waiting a second.
            store.inner.lock().unwrap().snapshot = None;
        }
        let versions = store.versions().unwrap();
        assert_eq!(versions.len(), 2, "{versions:?}");
        let mut texts: Vec<String> = versions
            .iter()
            .map(|v| {
                let d: serde_json::Value =
                    serde_json::from_str(&fs::read_to_string(&v.files[0]).unwrap()).unwrap();
                d["lines"][0]["text"].as_str().unwrap().to_string()
            })
            .collect();
        texts.sort();
        assert_eq!(texts, ["one", "two"], "each snapshot kept its own copy");
    }

    #[test]
    fn stamps_are_utc_and_sort_chronologically() {
        let a = utc_stamp(UNIX_EPOCH);
        assert_eq!(a, "1970-01-01T00-00-00");
        // 2026-08-16T21:04:11Z
        let b = utc_stamp(UNIX_EPOCH + Duration::from_secs(1_786_914_251));
        assert_eq!(b, "2026-08-16T21-04-11");
        assert!(a < b);
    }
}
