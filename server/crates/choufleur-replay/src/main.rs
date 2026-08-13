//! `choufleur-replay` — the offline harness.
//!
//! One binary drives the whole Phase 0 loop: build or verify a corpus, transcribe
//! it, track it, and score the result. The tracking path it exercises is the same
//! `choufleur-core` the live server will call, so this is not a prototype that gets
//! thrown away — it is the regression and tuning backbone, permanently.

use std::path::PathBuf;

use choufleur_replay::cmd;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "choufleur-replay",
    about = "Offline replay, tracking and evaluation harness for Choufleur",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check a corpus: files present, hashes intact, script and ground truth agree.
    Verify {
        /// Corpus directory (containing manifest.json) or the manifest itself.
        corpus: PathBuf,
        /// Re-base relative audio paths — for audio kept on external storage.
        #[arg(long)]
        audio_root: Option<PathBuf>,
        /// Compute and write missing SHA-256 hashes back into the manifest.
        #[arg(long)]
        update_hashes: bool,
    },

    /// Track a script from an existing transcript, writing a position trace.
    Track {
        corpus: PathBuf,
        /// Segments produced by `transcribe`.
        #[arg(long)]
        segments: PathBuf,
        #[arg(long, short = 'o', default_value = "trace.jsonl")]
        out: PathBuf,
        /// JSON file of `TrackerConfig` overrides.
        #[arg(long)]
        tracker_config: Option<PathBuf>,
        #[arg(long)]
        audio_root: Option<PathBuf>,
        /// Record per-segment match cost in the trace. Off by default: it makes
        /// the trace non-reproducible, and the trace is the regression baseline.
        #[arg(long)]
        timings: bool,
    },

    /// Score a trace against ground truth.
    Eval {
        corpus: PathBuf,
        #[arg(long)]
        trace: PathBuf,
        /// Optional; adds ASR latency and filter statistics to the report.
        #[arg(long)]
        segments: Option<PathBuf>,
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
        #[arg(long)]
        pretty: bool,
        #[arg(long)]
        audio_root: Option<PathBuf>,
    },

    /// Generate a synthetic corpus with macOS speech synthesis.
    ///
    /// Ground truth is exact by construction, so the whole pipeline can be tested
    /// end to end without waiting for a rehearsal recording. Synthetic speech is
    /// far easier than a real stage: this proves the plumbing, never the gate.
    MakeFixture {
        /// Output directory; created if absent.
        out: PathBuf,
        /// Script to speak. Defaults to a built-in bilingual two-hander.
        #[arg(long)]
        script: Option<PathBuf>,
        /// Seed for the (deterministic) inter-line gaps.
        #[arg(long, default_value_t = 42)]
        seed: u64,
        /// `lang=voice` pairs, e.g. `fr=Thomas,en=Samantha`.
        #[arg(long)]
        voices: Option<String>,
        #[arg(long, default_value_t = 180)]
        rate_wpm: u32,
        /// Add a dither/noise floor at this level in dBFS, so VAD is not tested
        /// against digital silence. Use 0 to disable.
        #[arg(long, default_value_t = -60.0)]
        noise_db: f32,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Verify {
            corpus,
            audio_root,
            update_hashes,
        } => cmd::verify::run(&corpus, audio_root, update_hashes),
        Command::Track {
            corpus,
            segments,
            out,
            tracker_config,
            audio_root,
            timings,
        } => cmd::track::run(
            &corpus,
            &segments,
            &out,
            tracker_config.as_deref(),
            audio_root,
            timings,
        ),
        Command::Eval {
            corpus,
            trace,
            segments,
            out,
            pretty,
            audio_root,
        } => cmd::eval::run(
            &corpus,
            &trace,
            segments.as_deref(),
            out.as_deref(),
            pretty,
            audio_root,
        ),
        Command::MakeFixture {
            out,
            script,
            seed,
            voices,
            rate_wpm,
            noise_db,
        } => cmd::make_fixture::run(
            &out,
            script.as_deref(),
            seed,
            voices.as_deref(),
            rate_wpm,
            noise_db,
        ),
    }
}
