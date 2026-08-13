//! The corpus manifest: what a recording *is*, in a form git can hold.
//!
//! Multi-gigabyte audio lives on external storage; the manifest — paths, channel
//! map, language tags, and a SHA-256 per file — lives in the repository. That
//! split is what makes an eval result reproducible without committing the show
//! recordings, and the hashes are what make "reproducible" mean something.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use choufleur_core::lang::LangCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MANIFEST_FILE: &str = "manifest.json";
pub const FORMAT: &str = "choufleur-corpus";
pub const FORMAT_VERSION: &str = "0.1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioFile {
    /// Path relative to the manifest, or absolute.
    pub file: PathBuf,
    /// SHA-256 of the file as committed. Empty means "not yet hashed".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSpec {
    /// Logical channel index, as referenced by `Character::channels`.
    pub index: u16,
    #[serde(flatten)]
    pub audio: AudioFile,
    /// Character id this channel carries. `None` marks a **zone channel** — an
    /// ambient or area mic with no speaker identity (PRD, *Ambient / area
    /// microphones*), matched against any expected speaker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<String>,
    /// Language to force decoding to when the script cannot say. Normally absent:
    /// the script's language tags are the authority (PRD, *Multi-Language Support*).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<LangCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ChannelSpec {
    pub fn is_zone(&self) -> bool {
        self.character.is_none()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default = "default_format_version")]
    pub format_version: String,
    pub show: String,
    pub act: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub sample_rate: u32,
    /// Script file, relative to the manifest.
    pub script: PathBuf,
    /// Ground-truth timeline, relative to the manifest. Absent until labelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ground_truth: Option<PathBuf>,
    pub channels: Vec<ChannelSpec>,
    /// A mixed-down variant of the *same* material, for the degraded-mode
    /// comparison the PRD promises (single mixed feed vs. per-actor channels).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mixdown: Option<AudioFile>,
    /// Free-form provenance: console, venue, session date, consent notes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provenance: BTreeMap<String, String>,
}

fn default_format() -> String {
    FORMAT.to_string()
}
fn default_format_version() -> String {
    FORMAT_VERSION.to_string()
}

/// A manifest together with the directory it was loaded from, so relative paths
/// resolve without the caller having to remember where it came from.
pub struct Corpus {
    pub manifest: Manifest,
    pub dir: PathBuf,
    /// Overrides the manifest directory for *audio* files only — the escape hatch
    /// for audio parked on an external drive.
    pub audio_root: Option<PathBuf>,
}

impl Corpus {
    pub fn load(dir_or_file: &Path, audio_root: Option<PathBuf>) -> Result<Self> {
        let path = if dir_or_file.is_dir() {
            dir_or_file.join(MANIFEST_FILE)
        } else {
            dir_or_file.to_path_buf()
        };
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let manifest: Manifest = serde_json::from_str(&text)
            .with_context(|| format!("parsing manifest {}", path.display()))?;
        if manifest.format != FORMAT {
            bail!(
                "{} is not a {FORMAT} file (format = {:?})",
                path.display(),
                manifest.format
            );
        }
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok(Corpus {
            manifest,
            dir,
            audio_root,
        })
    }

    pub fn resolve(&self, rel: &Path) -> PathBuf {
        if rel.is_absolute() {
            rel.to_path_buf()
        } else {
            self.dir.join(rel)
        }
    }

    pub fn resolve_audio(&self, rel: &Path) -> PathBuf {
        if rel.is_absolute() {
            return rel.to_path_buf();
        }
        match &self.audio_root {
            Some(root) => root.join(rel),
            None => self.dir.join(rel),
        }
    }

    pub fn script_path(&self) -> PathBuf {
        self.resolve(&self.manifest.script)
    }

    pub fn ground_truth_path(&self) -> Option<PathBuf> {
        self.manifest.ground_truth.as_ref().map(|p| self.resolve(p))
    }

    pub fn channel(&self, index: u16) -> Option<&ChannelSpec> {
        self.manifest.channels.iter().find(|c| c.index == index)
    }
}

/// SHA-256 of a file, streamed — corpus audio does not fit comfortably in memory.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips() {
        let m = Manifest {
            format: default_format(),
            format_version: default_format_version(),
            show: "seagull".into(),
            act: "act-1".into(),
            note: None,
            sample_rate: 48000,
            script: "script.json".into(),
            ground_truth: Some("ground-truth.jsonl".into()),
            channels: vec![
                ChannelSpec {
                    index: 1,
                    audio: AudioFile {
                        file: "ch01-marie.wav".into(),
                        sha256: "abc".into(),
                    },
                    character: Some("char-marie".into()),
                    lang: None,
                    note: None,
                },
                ChannelSpec {
                    index: 9,
                    audio: AudioFile {
                        file: "ch09-zone-dsl.wav".into(),
                        sha256: "def".into(),
                    },
                    character: None,
                    lang: None,
                    note: Some("downstage left boundary mic".into()),
                },
            ],
            mixdown: None,
            provenance: BTreeMap::new(),
        };
        let json = serde_json::to_string_pretty(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.channels.len(), 2);
        assert!(back.channels[1].is_zone());
        // The flattened audio fields must not nest.
        assert!(json.contains("\"file\": \"ch01-marie.wav\""), "{json}");
    }
}
