//! `choufleur-replay` — the offline harness.
//!
//! One binary drives the whole Phase 0 loop: build or verify a corpus, transcribe
//! it, track it, and score the result. The tracking path it exercises is the same
//! `choufleur-core` the live server will call, so this is not a prototype that gets
//! thrown away — it is the regression and tuning backbone, permanently.

use std::path::PathBuf;

use choufleur_replay::cmd;

use anyhow::Result;
use choufleur_core::prompt::BiasMode;
use clap::{Parser, Subcommand, ValueEnum};

/// How much the recogniser is told about what to expect.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Bias {
    /// No prompt at all — the control condition for the sweep.
    None,
    /// A per-show constant: title and character names.
    Static,
    /// The lines the tracker expects next. Only meaningful with `track --from-audio`.
    Tracker,
}

impl From<Bias> for BiasMode {
    fn from(b: Bias) -> Self {
        match b {
            Bias::None => BiasMode::None,
            Bias::Static => BiasMode::Static,
            Bias::Tracker => BiasMode::Tracker,
        }
    }
}

/// Shared by `transcribe` and `track --from-audio`.
#[derive(clap::Args, Clone, Debug)]
struct AudioArgs {
    /// Whisper ggml model. Found automatically if fetch-models.sh has been run.
    #[arg(long)]
    model: Option<PathBuf>,
    /// Silero VAD onnx model.
    #[arg(long)]
    vad_model: Option<PathBuf>,
    /// Pace the run against the wall clock and measure end-to-end latency.
    #[arg(long)]
    realtime: bool,
    #[arg(long, value_enum, default_value_t = Bias::Static)]
    bias: Bias,
    /// Use the mixed feed instead of the per-actor channels (the degraded mode).
    #[arg(long)]
    mixdown: bool,
    /// Restrict to these logical channels, e.g. --channels 1,3.
    #[arg(long, value_delimiter = ',')]
    channels: Option<Vec<u16>>,
    /// How often to emit a partial hypothesis while someone is still speaking.
    /// 0 disables it, which restores the end-of-utterance latency floor: detection
    /// lag can then be no better than the length of the line being spoken.
    #[arg(long)]
    interim_ms: Option<u32>,
    /// Turn off the per-channel adaptive gain.
    ///
    /// It earns its keep on a close mic gained for a shout. On an ambient capture
    /// the same rule lifts room tone and reverb between phrases, and handing the
    /// recogniser louder noise is how it is invited to invent — so which way this
    /// goes is a measurement, per corpus, not a default.
    #[arg(long)]
    no_agc: bool,
}

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

    /// Transcribe a corpus's audio into timestamped segments.
    Transcribe {
        corpus: PathBuf,
        #[arg(long, short = 'o', default_value = "out/segments.jsonl")]
        out: PathBuf,
        #[command(flatten)]
        audio: AudioArgs,
        #[arg(long)]
        audio_root: Option<PathBuf>,
    },

    /// Track a script, from an existing transcript or straight from audio.
    Track {
        corpus: PathBuf,
        /// Segments produced by `transcribe`. Mutually exclusive with --from-audio.
        #[arg(long, conflicts_with = "from_audio")]
        segments: Option<PathBuf>,
        /// Transcribe and track in one pass, so the tracker can bias its own
        /// recognition with the lines it expects next.
        #[arg(long)]
        from_audio: bool,
        /// With --from-audio, also write the segments for later replay.
        #[arg(long, requires = "from_audio")]
        segments_out: Option<PathBuf>,
        #[command(flatten)]
        audio: AudioArgs,
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
        /// Build a load-test corpus instead: N characters, every channel speaking
        /// at once. The devplan's compute criterion is stated in *concurrent*
        /// active channels, and ordinary dialogue takes turns, so it cannot be
        /// measured on a normal fixture. Not a tracking test — the audio is
        /// nothing like a performance.
        #[arg(long, value_name = "CHANNELS")]
        load_test: Option<usize>,
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
        Command::Transcribe {
            corpus,
            out,
            audio,
            audio_root,
        } => cmd::transcribe::run(
            &corpus,
            &out,
            audio.model.as_deref(),
            audio.vad_model.as_deref(),
            audio.realtime,
            audio.bias.into(),
            audio.mixdown,
            audio.channels,
            audio.interim_ms,
            audio_root,
            audio.no_agc,
        ),
        Command::Track {
            corpus,
            segments,
            from_audio,
            segments_out,
            audio,
            out,
            tracker_config,
            audio_root,
            timings,
        } => match (from_audio, segments) {
            (true, _) => cmd::track::run_from_audio(
                &corpus,
                &out,
                segments_out.as_deref(),
                tracker_config.as_deref(),
                audio.model.as_deref(),
                audio.vad_model.as_deref(),
                audio.realtime,
                audio.bias.into(),
                audio.mixdown,
                audio.channels,
                audio.interim_ms,
                audio_root,
                timings,
            ),
            (false, Some(segments)) => cmd::track::run(
                &corpus,
                &segments,
                &out,
                tracker_config.as_deref(),
                audio_root,
                timings,
            ),
            (false, None) => {
                anyhow::bail!("track needs either --segments <file> or --from-audio")
            }
        },
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
            load_test,
        } => cmd::make_fixture::run(
            &out,
            script.as_deref(),
            seed,
            voices.as_deref(),
            rate_wpm,
            noise_db,
            load_test,
        ),
    }
}
