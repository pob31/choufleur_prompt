//! `verify` — is this corpus intact and internally consistent?
//!
//! Run before every eval. A corpus that has quietly drifted (audio re-exported,
//! a line id renamed, ground truth labelled against an older script) produces
//! numbers that look fine and mean nothing, which is worse than an error.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::formats::{read_jsonl, GroundTruthLine};
use crate::manifest::{sha256_file, Corpus};
use crate::wav_stream::WavBlockReader;

pub fn run(corpus_path: &Path, audio_root: Option<PathBuf>, update_hashes: bool) -> Result<()> {
    let mut corpus = Corpus::load(corpus_path, audio_root)?;
    let m = &corpus.manifest;
    println!(
        "corpus: {} / {} ({} channels)",
        m.show,
        m.act,
        m.channels.len()
    );

    let (script, prepared) = super::load_script(&corpus.script_path())?;
    println!(
        "script: {} lines, default language {:?}",
        script.lines.len(),
        script.default_lang
    );

    let mut problems: Vec<String> = Vec::new();
    let mut hashes_written = 0usize;

    // --- Characters and channels -------------------------------------------
    let known_chars: HashSet<&str> = script.characters.iter().map(|c| c.id.as_str()).collect();
    let mut zone_channels = 0usize;
    for ch in &m.channels {
        match &ch.character {
            Some(c) if !known_chars.contains(c.as_str()) => {
                problems.push(format!("channel {}: unknown character {c:?}", ch.index));
            }
            None => zone_channels += 1,
            _ => {}
        }
    }
    for c in &script.characters {
        if !c.channels.is_empty() && !c.channels.iter().any(|i| corpus.channel(*i).is_some()) {
            problems.push(format!(
                "character {} claims channels {:?}, none of which the manifest carries",
                c.id, c.channels
            ));
        }
    }
    let speaking: HashSet<&str> = script.lines.iter().map(|l| l.character.as_str()).collect();
    for c in &speaking {
        let covered = m
            .channels
            .iter()
            .any(|ch| ch.character.as_deref() == Some(*c));
        if !covered && zone_channels == 0 {
            problems.push(format!(
                "character {c} speaks but has no channel and there is no zone mic"
            ));
        }
    }

    // --- Audio --------------------------------------------------------------
    let mut durations: Vec<(String, f64)> = Vec::new();
    let mut audio_files: Vec<(String, &crate::manifest::AudioFile)> = m
        .channels
        .iter()
        .map(|c| (format!("channel {}", c.index), &c.audio))
        .collect();
    if let Some(mix) = &m.mixdown {
        audio_files.push(("mixdown".to_string(), mix));
    }

    let mut computed: Vec<(String, String)> = Vec::new();
    for (label, audio) in &audio_files {
        let path = corpus.resolve_audio(&audio.file);
        if !path.exists() {
            problems.push(format!("{label}: missing audio {}", path.display()));
            continue;
        }
        let reader = WavBlockReader::open(&path)?;
        if reader.sample_rate != m.sample_rate {
            problems.push(format!(
                "{label}: {} Hz but the manifest says {} Hz",
                reader.sample_rate, m.sample_rate
            ));
        }
        if reader.channels != 1 {
            problems.push(format!(
                "{label}: {} audio channels; per-actor feeds should be mono",
                reader.channels
            ));
        }
        durations.push((label.clone(), reader.duration_seconds()));

        let digest = sha256_file(&path)?;
        if audio.sha256.is_empty() {
            if update_hashes {
                computed.push((audio.file.display().to_string(), digest));
                hashes_written += 1;
            } else {
                problems.push(format!(
                    "{label}: no sha256 in the manifest (run with --update-hashes)"
                ));
            }
        } else if audio.sha256 != digest {
            problems.push(format!(
                "{label}: sha256 mismatch — the audio has changed since the manifest was written\n    manifest {}\n    on disk  {}",
                audio.sha256, digest
            ));
        }
    }

    // Channel lengths must agree, or the timelines do not line up.
    if let (Some((_, first)), true) = (durations.first(), durations.len() > 1) {
        for (label, d) in &durations[1..] {
            if (d - first).abs() > 0.25 {
                problems.push(format!(
                    "{label}: {d:.2} s but the first file is {first:.2} s — channels must be the same take"
                ));
            }
        }
    }
    if let Some((_, d)) = durations.first() {
        println!("audio:  {:.1} s per channel", d);
    }

    // --- Ground truth -------------------------------------------------------
    match corpus.ground_truth_path() {
        None => println!("ground truth: not yet labelled — eval will not run against this corpus"),
        Some(path) if !path.exists() => problems.push(format!(
            "ground truth: manifest points at {}, which is missing",
            path.display()
        )),
        Some(path) => {
            let gt: Vec<GroundTruthLine> = read_jsonl(&path)?;
            let mut last_onset = f64::NEG_INFINITY;
            let mut performed = 0usize;
            let mut seen: HashSet<&str> = HashSet::new();
            for (i, l) in gt.iter().enumerate() {
                if prepared.index_of(&l.line_id).is_none() {
                    problems.push(format!(
                        "ground truth line {}: unknown line id {:?}",
                        i + 1,
                        l.line_id
                    ));
                }
                if !seen.insert(&l.line_id) {
                    problems.push(format!(
                        "ground truth line {}: duplicate line id {:?}",
                        i + 1,
                        l.line_id
                    ));
                }
                if l.omitted {
                    continue;
                }
                performed += 1;
                if l.end < l.onset {
                    problems.push(format!(
                        "ground truth line {}: end {} before onset {}",
                        i + 1,
                        l.end,
                        l.onset
                    ));
                }
                if l.onset < last_onset {
                    problems.push(format!(
                        "ground truth line {}: onset {:.3} goes backwards (previous {:.3})",
                        i + 1,
                        l.onset,
                        last_onset
                    ));
                }
                last_onset = l.onset;
                if let Some((_, d)) = durations.first() {
                    if l.onset > *d + 1.0 {
                        problems.push(format!(
                            "ground truth line {}: onset {:.1} s is past the end of the audio ({:.1} s)",
                            i + 1, l.onset, d
                        ));
                    }
                }
            }
            let labelled = performed as f64 / script.lines.len().max(1) as f64;
            println!(
                "ground truth: {performed} of {} script lines labelled ({:.0}%)",
                script.lines.len(),
                labelled * 100.0
            );
        }
    }

    if update_hashes && !computed.is_empty() {
        write_hashes(&mut corpus, &computed)?;
        println!("wrote {hashes_written} sha256 hash(es) into the manifest");
    }

    if problems.is_empty() {
        println!("\nOK — corpus is consistent.");
        Ok(())
    } else {
        for p in &problems {
            eprintln!("  ✗ {p}");
        }
        bail!("{} problem(s) found", problems.len())
    }
}

fn write_hashes(corpus: &mut Corpus, computed: &[(String, String)]) -> Result<()> {
    for (file, digest) in computed {
        for ch in &mut corpus.manifest.channels {
            if ch.audio.file.display().to_string() == *file {
                ch.audio.sha256 = digest.clone();
            }
        }
        if let Some(mix) = &mut corpus.manifest.mixdown {
            if mix.file.display().to_string() == *file {
                mix.sha256 = digest.clone();
            }
        }
    }
    let path = corpus.dir.join(crate::manifest::MANIFEST_FILE);
    let json = serde_json::to_string_pretty(&corpus.manifest)?;
    std::fs::write(&path, json + "\n").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
