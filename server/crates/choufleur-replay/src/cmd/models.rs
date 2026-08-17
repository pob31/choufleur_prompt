//! Getting the models, from a terminal or from the Shows screen.
//!
//! Half a gigabyte of weights that cannot go in the repository, carry their own
//! licences, and are the difference between a program that listens and one that
//! cannot. Somebody has to fetch them once per machine, and until now that somebody
//! had to be holding a shell prompt in a checkout.
//!
//! Three rules, each of them a thing that goes wrong in a theatre:
//!
//! - **Resume.** A venue's network drops, and re-downloading 465 MB from zero on a
//!   show day is not a plan. Downloads land in `<name>.part` and continue from
//!   wherever they got to.
//! - **Verify.** A truncated or half-rewritten model file does not announce itself:
//!   it loads, or it loads and is subtly wrong. Every file is checked against a
//!   pinned SHA-256 before it is given its real name, so a file with the right name
//!   is always a file with the right contents.
//! - **Say where.** A network that blocks Hugging Face is a real afternoon, so the
//!   folder, the filenames and the URLs are printed plainly enough to be carried to
//!   another machine and copied back on a stick.
//!
//! Downloading is `curl`'s job rather than a linked HTTP client: it is on every Mac,
//! it already knows about redirects, proxies, resumption and flaky connections, and
//! it is what `scripts/fetch-models.sh` has always used. Progress is not parsed out
//! of it — the size of the `.part` file on disk is the truth, and anyone watching,
//! including the Shows screen, can read it without being told.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One model file: what it is called, how big it is, what it must hash to, and where
/// to get it.
pub struct Model {
    pub file: &'static str,
    /// For a person: what this is and why they might want it.
    pub label: &'static str,
    pub bytes: u64,
    /// Pinned. Taken from the publisher and checked against a copy known to work —
    /// Hugging Face reports it as `x-linked-etag`, and the Silero file was fetched
    /// fresh and compared. A model that hashes differently is not this model.
    pub sha256: &'static str,
    /// In order. The second is a fallback for when a project moves its files, which
    /// Silero has already done once.
    pub urls: &'static [&'static str],
    /// Downloaded unless asked for: `medium` is three times the size and only worth
    /// it on a machine that has been measured.
    pub optional: bool,
}

pub const WHISPER_SMALL: Model = Model {
    file: "ggml-small.bin",
    label: "Whisper small — the recogniser the show runs on",
    bytes: 487_601_967,
    sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    urls: &["https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin"],
    optional: false,
};

pub const SILERO: Model = Model {
    file: "silero_vad.onnx",
    label: "Silero VAD — what decides somebody is speaking",
    bytes: 2_327_524,
    sha256: "2623a2953f6ff3d2c1e61740c6cdb7168133479b267dfef114a4a3cc5bdd788f",
    // Pinned to a tag, never to `master`. Silero v4 exposes a different set of
    // inputs and is rejected at load; a moving URL would turn that into a mystery.
    urls: &[
        "https://raw.githubusercontent.com/snakers4/silero-vad/v5.1.2/src/silero_vad/data/silero_vad.onnx",
        "https://raw.githubusercontent.com/snakers4/silero-vad/v5.1.2/files/silero_vad.onnx",
    ],
    optional: false,
};

pub const WHISPER_MEDIUM: Model = Model {
    file: "ggml-medium.bin",
    label: "Whisper medium — slower, better, for machines that can hold it",
    bytes: 1_533_763_059,
    sha256: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
    urls: &["https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin"],
    optional: true,
};

pub const CATALOG: &[Model] = &[WHISPER_SMALL, SILERO, WHISPER_MEDIUM];

/// Where models live when nobody says otherwise: beside the library, in the open.
///
/// Not `~/Library/Application Support`. This is the folder somebody is told to drop a
/// file into when the venue's network will not fetch it, and a folder the Finder hides
/// is a folder that cannot be part of an instruction.
pub fn default_dir() -> PathBuf {
    super::show::default_root().join("models")
}

/// How far along one model is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// There, and the right file.
    Ready,
    /// Started. `got` bytes of it are on disk.
    Partial { got: u64 },
    Absent,
}

pub fn state_of(dir: &Path, m: &Model) -> State {
    if dir.join(m.file).exists() {
        return State::Ready;
    }
    match std::fs::metadata(dir.join(format!("{}.part", m.file))) {
        Ok(md) => State::Partial { got: md.len() },
        Err(_) => State::Absent,
    }
}

/// Everything the show needs, present and correct.
pub fn ready(dir: &Path) -> bool {
    CATALOG
        .iter()
        .filter(|m| !m.optional)
        .all(|m| dir.join(m.file).exists())
}

/// Fetch one model into `dir`, resuming and verifying. Already there is success.
pub fn fetch(dir: &Path, m: &Model) -> Result<()> {
    let final_path = dir.join(m.file);
    if final_path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("making the models folder at {}", dir.display()))?;
    let part = dir.join(format!("{}.part", m.file));

    let mut transport: Option<anyhow::Error> = None;
    // Kept apart from transport errors and preferred when reporting. A mismatch says
    // something arrived and was wrong, which is worth knowing; a 404 on the fallback
    // URL tried afterwards is not, and it was drowning out the real answer.
    let mut mismatch: Option<anyhow::Error> = None;

    for url in m.urls {
        // Twice at most: once resuming whatever is there, and — if that turns out to
        // hash wrong — once from nothing. The second attempt is what rescues a `.part`
        // left behind by an interrupted download that resumed onto the wrong bytes,
        // which cannot be told from a good one by looking at its size.
        for attempt in 0..2 {
            if let Err(e) = download(&part, url) {
                transport = Some(e);
                break;
            }
            let got = sha256_of(&part)?;
            if got == m.sha256 {
                std::fs::rename(&part, &final_path).with_context(|| {
                    format!("putting {} in place at {}", m.file, final_path.display())
                })?;
                return Ok(());
            }
            // Never kept: resuming onto wrong bytes stays wrong for ever, and a file
            // under the real name is supposed to mean a file with the right contents.
            let _ = std::fs::remove_file(&part);
            if attempt == 1 {
                mismatch = Some(anyhow::anyhow!(
                    "{} arrived damaged from {url}\n  expected {}\n  got      {got}\n\
                     Twice, so this is not a dropped connection. A network that rewrites \
                     downloads — a hotel or venue login page, most often — will do this \
                     to every attempt. Fetch it on another network and copy it in.",
                    m.file,
                    m.sha256
                ));
            }
        }
    }
    Err(mismatch
        .or(transport)
        .unwrap_or_else(|| anyhow::anyhow!("no source for {}", m.file)))
}

/// `curl`, resuming, following redirects, and failing loudly on an error page.
///
/// `--fail` matters more than it looks: without it a captive portal's login page is
/// saved as the model, and the first thing anybody hears about it is a hash mismatch
/// after a twenty-minute download.
fn download(part: &Path, url: &str) -> Result<()> {
    let status = std::process::Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--retry",
            "3",
            "--retry-delay",
            "2",
            // Resume from whatever is already there, and start from zero when there
            // is nothing — which is what `-C -` means to curl.
            "-C",
            "-",
            "--silent",
            "--show-error",
            "-o",
        ])
        .arg(part)
        .arg(url)
        .status()
        .context("running curl — is it installed?")?;
    if !status.success() {
        anyhow::bail!("could not download {url} ({status})");
    }
    Ok(())
}

fn sha256_of(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("reading {} to check it", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).context("hashing")?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Human-readable size, for messages a person reads while waiting.
pub fn megabytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{} MB", bytes / 1_000_000)
    }
}

/// `models fetch` — get what is missing.
pub fn fetch_all(dir: Option<PathBuf>, medium: bool) -> Result<()> {
    let dir = dir.unwrap_or_else(default_dir);
    println!("models: {}", dir.display());
    for m in CATALOG {
        if m.optional && !(medium && m.file == WHISPER_MEDIUM.file) {
            continue;
        }
        match state_of(&dir, m) {
            State::Ready => {
                println!("  {} — already here", m.file);
                continue;
            }
            State::Partial { got } => println!(
                "  {} — resuming from {} of {}",
                m.file,
                megabytes(got),
                megabytes(m.bytes)
            ),
            State::Absent => println!("  {} — {}", m.file, megabytes(m.bytes)),
        }
        fetch(&dir, m)?;
        println!("  {} — done", m.file);
    }
    Ok(())
}

/// `models list` — what is here, what is not, and where to put it.
pub fn list(dir: Option<PathBuf>) -> Result<()> {
    let dir = dir.unwrap_or_else(default_dir);
    println!("models: {}\n", dir.display());
    for m in CATALOG {
        let note = match state_of(&dir, m) {
            State::Ready => "ready".to_string(),
            State::Partial { got } => {
                format!("part-downloaded, {} of {}", megabytes(got), megabytes(m.bytes))
            }
            State::Absent if m.optional => format!("not here ({}, optional)", megabytes(m.bytes)),
            State::Absent => format!("MISSING ({})", megabytes(m.bytes)),
        };
        println!("  {:<18} {}", m.file, note);
        println!("  {:<18} {}", "", m.label);
    }
    if !ready(&dir) {
        println!("\n  choufleur-replay models fetch");
        println!("\nor, on a machine that can reach them, download these and copy them into");
        println!("the folder above under exactly these names:");
        for m in CATALOG.iter().filter(|m| !m.optional) {
            println!("  {}", m.urls[0]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pinned_hash_is_a_sha256() {
        // A typo here is a download that can never succeed, and the failure would
        // arrive after twenty minutes of network rather than at compile time.
        for m in CATALOG {
            assert_eq!(m.sha256.len(), 64, "{}", m.file);
            assert!(
                m.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
                "{} is not lowercase hex",
                m.file
            );
            assert!(!m.urls.is_empty(), "{} has nowhere to come from", m.file);
        }
    }

    #[test]
    fn the_vad_url_is_pinned_to_a_tag() {
        // Silero v4 exposes different inputs and is rejected at load. A URL that
        // followed `master` would turn the next release into a mystery in a venue.
        for url in SILERO.urls {
            assert!(url.contains("/v5.1.2/"), "{url} is not pinned");
        }
    }

    #[test]
    fn a_part_file_is_progress_and_not_a_model() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(state_of(dir.path(), &SILERO), State::Absent);
        assert!(!ready(dir.path()));

        std::fs::write(dir.path().join("silero_vad.onnx.part"), b"half of it").unwrap();
        assert_eq!(state_of(dir.path(), &SILERO), State::Partial { got: 10 });
        // Still not usable: a `.part` is never loaded, whatever it contains.
        assert!(!ready(dir.path()));

        std::fs::write(dir.path().join("silero_vad.onnx"), b"all of it").unwrap();
        assert_eq!(state_of(dir.path(), &SILERO), State::Ready);
    }

    #[test]
    fn readiness_ignores_the_optional_model() {
        let dir = tempfile::tempdir().unwrap();
        for m in CATALOG.iter().filter(|m| !m.optional) {
            std::fs::write(dir.path().join(m.file), b"x").unwrap();
        }
        // `medium` is absent and that is a complete installation.
        assert!(ready(dir.path()));
    }

    #[test]
    fn sizes_read_the_way_a_person_says_them() {
        assert_eq!(megabytes(487_601_967), "487 MB");
        assert_eq!(megabytes(1_533_763_059), "1.5 GB");
    }
}
