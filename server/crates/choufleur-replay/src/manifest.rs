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
    /// Everyone this channel might be carrying, when one mic serves a position rather
    /// than a person.
    ///
    /// *Lovedoll* has no per-actor mics at all — a shotgun, three SM58s by position,
    /// four floor mics — and the operator described how it really works: *"I tried to
    /// keep people on the same mic as much as possible, not always possible. Veronica
    /// and Nicolas would share sometimes."* Naming one of them is wrong half the time;
    /// naming none throws away what is known. So a channel names the few people who
    /// could be on it, and a line spoken by any of them agrees with it.
    ///
    /// Takes precedence over `character` when both are set. Empty leaves the channel
    /// exactly as it was: one name, or none for a zone mic.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub characters: Vec<String>,
    /// Language to force decoding to when the script cannot say. Normally absent:
    /// the script's language tags are the authority (PRD, *Multi-Language Support*).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<LangCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ChannelSpec {
    /// A mic belonging to a place rather than to anybody: matched against any speaker,
    /// and trusted no further than that.
    pub fn is_zone(&self) -> bool {
        self.speakers().is_empty()
    }

    /// Everyone this channel might be carrying. Empty for a zone mic.
    pub fn speakers(&self) -> &[String] {
        if self.characters.is_empty() {
            self.character.as_slice()
        } else {
            &self.characters
        }
    }

    /// The one name to bias recognition towards, when there is one.
    ///
    /// Decoding takes a single hint, and on a shared mic the first name listed is as
    /// good a guess as exists — the operator writes them in the order they expect.
    /// Matching is not restricted this way; it considers all of them.
    pub fn primary(&self) -> Option<&str> {
        self.speakers().first().map(String::as_str)
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

/// A show, as somebody would write it by hand.
///
/// The corpus manifest is a *measurement* format: channel arrays, per-file SHA-256,
/// ground-truth paths, provenance. All of it earns its place when the point is a
/// reproducible experiment, and all of it is in the way when the point is "run this
/// show tonight". Four fields, no hashes, no channel table:
///
/// ```json
/// {
///   "title": "Hécube, pas Hécube",
///   "script": "script.json",
///   "audio":  "Hecube_20241116mono.wav"
/// }
/// ```
///
/// Paths are relative to the file. `audio` may be omitted, which simply means there is
/// nothing to play — useful for opening a show to read and correct its script without
/// a recording to hand.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShowFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub script: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<PathBuf>,
    /// Cues, as produced by the conduite readers. Not yet displayed; carried so the
    /// file does not have to change shape when they are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cues: Option<PathBuf>,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
}

fn default_sample_rate() -> u32 {
    48_000
}

impl ShowFile {
    /// Expand into the manifest the engine already knows how to run.
    ///
    /// One zone channel: a show file describes what to play, not who is on which mic.
    /// Per-actor channels remain a corpus concern, because assigning them is exactly
    /// the kind of bookkeeping this format exists to avoid.
    pub fn into_manifest(self) -> Manifest {
        let channels = self
            .audio
            .map(|file| {
                vec![ChannelSpec {
                    index: 1,
                    audio: AudioFile {
                        file,
                        sha256: String::new(),
                    },
                    character: None,
                    characters: Vec::new(),
                    lang: None,
                    note: Some("zone channel: no speaker identity".into()),
                }]
            })
            .unwrap_or_default();
        Manifest {
            format: FORMAT.to_string(),
            format_version: default_format_version(),
            show: self.title.clone().unwrap_or_else(|| "show".into()),
            act: "run".into(),
            note: None,
            sample_rate: self.sample_rate,
            script: self.script,
            ground_truth: None,
            channels,
            mixdown: None,
            provenance: Default::default(),
        }
    }
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
        // A show file has a script and no channel table; a manifest always has both.
        // Sniffing rather than requiring a format tag keeps the hand-written file as
        // short as it looks in the docs.
        let probe: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        if probe.get("channels").is_none() && probe.get("script").is_some() {
            let show: ShowFile = serde_json::from_value(probe)
                .with_context(|| format!("parsing show file {}", path.display()))?;
            return Ok(Corpus {
                manifest: show.into_manifest(),
                dir: path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from(".")),
                audio_root,
            });
        }
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
                    characters: Vec::new(),
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
                    characters: Vec::new(),
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
