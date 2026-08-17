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
    /// Do not put the show's proper nouns in the decoder's prompt.
    ///
    /// The lexicon fixes the words that matter most — the title character of Hécube
    /// was never once recognised without it — but priming a recogniser with names is
    /// also how it is invited to hear them where they were not said. Which effect
    /// wins is a measurement per production, so it is a switch.
    #[arg(long)]
    no_lexicon: bool,
    /// Turn off the per-channel adaptive gain.
    ///
    /// It earns its keep on a close mic gained for a shout. On an ambient capture
    /// the same rule lifts room tone and reverb between phrases, and handing the
    /// recogniser louder noise is how it is invited to invent — so which way this
    /// goes is a measurement, per corpus, not a default.
    #[arg(long)]
    no_agc: bool,
    /// Ceiling on the adaptive gain, in dB. Default 40.
    ///
    /// A source feed for a spatialisation engine carries no channel processing at all,
    /// and is gained for the loudest moment of the night — measured on a WFS show,
    /// loud speech sat at −67 dBFS, which needs 47 dB to reach the −20 both models
    /// were trained around. At the default the limiter engages and the recogniser is
    /// handed audio 7 dB quieter than it expects. Which feed a venue offers is not
    /// ours to choose, so the ceiling is a dial.
    #[arg(long, value_name = "DB")]
    agc_max_gain: Option<f32>,
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// What is here, what is missing, and where to put it by hand.
    List,
    /// Download whatever is missing, resuming anything half-fetched.
    Fetch {
        /// Also fetch `medium`: three times the size, slower, more accurate.
        #[arg(long)]
        medium: bool,
    },
}

#[derive(Subcommand)]
enum ShowCmd {
    /// What is in the library.
    List,
    /// Start a show, optionally with its text.
    New {
        name: String,
        /// A plain-text script to import straight away.
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Copy an existing show into the library, leaving the original untouched.
    Import {
        /// The show's `manifest.json`.
        manifest: PathBuf,
        /// Call it something other than what the manifest says.
        #[arg(long)]
        name: Option<String>,
    },
    /// Print the rules a preparer works to, for handing to an AI.
    Rules,

    /// Check a prepared script: ids, speakers, holds, and whether any text went missing.
    Check {
        /// The script to check.
        script: PathBuf,
        /// The text it was prepared from. Without it, the coverage check cannot run.
        #[arg(long)]
        source: Option<PathBuf>,
    },

    /// Bring a cue list into a show, re-anchored against its script.
    ///
    /// Ids do not survive the crossing between shows, so every cue is placed by the
    /// text it recorded. Anything that cannot be placed keeps its old anchor and is
    /// marked for review rather than guessed at.
    Cues {
        /// The show's directory.
        show: PathBuf,
        /// The cue list to bring in.
        from: PathBuf,
        /// What to call the list.
        #[arg(long)]
        name: Option<String>,
    },

    /// Copy a cue list out of a show, to carry to another.
    Export {
        /// The cue list file.
        sheet: PathBuf,
        /// Where to put it — a directory or a filename.
        to: PathBuf,
    },

    /// Replace a show's script with a plain-text one, keeping a snapshot.
    Text {
        /// The show's `script.json`.
        script: PathBuf,
        /// The text to read.
        from: PathBuf,
    },
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
    /// Make, list and fill shows in the library.
    ///
    /// The library is `~/Choufleur` unless `$CHOUFLEUR_LIBRARY` says otherwise. The
    /// Shows screen does all of this with a window; this is the same operations from a
    /// terminal, and what the screen will call.
    Show {
        #[command(subcommand)]
        what: ShowCmd,
        /// The library directory.
        #[arg(long, global = true)]
        library: Option<PathBuf>,
    },

    /// The recogniser's weights: what is here, and how to get the rest.
    ///
    /// About 490 MB, once per machine. They live beside the library rather than in the
    /// repository — they carry their own licences and cannot be committed — and the
    /// Shows screen offers the same thing with a button.
    Models {
        #[command(subcommand)]
        what: ModelsCmd,
        /// Where to put them. Defaults to `<library>/models`.
        #[arg(long, global = true)]
        into: Option<PathBuf>,
    },

    /// The server UI: the library, in a window.
    ///
    /// Serves the admin screens and holds no show. Opening one from it starts a show
    /// server as a child on the next port up.
    Ui {
        /// The library directory.
        #[arg(long)]
        library: Option<PathBuf>,
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },

    /// Watch the patched inputs and say whether audio is arriving.
    ///
    /// Run it in a terminal you are looking at: if macOS has never been asked for the
    /// microphone, this is where the prompt appears.
    Listen {
        #[arg(long)]
        library: Option<PathBuf>,
        /// How long to watch.
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        /// Ignore the patch and listen to every input the device has.
        ///
        /// For finding which input a signal actually arrives on, which on a
        /// 128-channel card is not worth doing one channel at a time.
        #[arg(long)]
        scan: bool,
    },

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

    /// Run a corpus at real speed, audibly, with the script on a screen.
    ///
    /// The live spike: open the printed URL and watch the text follow the sound.
    /// Not the real server (that is Phase 2) — a thin slice over this harness so the
    /// display can be judged by eye and ear rather than by metric.
    Serve {
        corpus: PathBuf,
        #[command(flatten)]
        audio: AudioArgs,
        #[arg(long)]
        audio_root: Option<PathBuf>,
        #[arg(long)]
        tracker_config: Option<PathBuf>,
        /// Race a second tracker config against the first, side by side.
        ///
        /// Both see the same recognition — only the first steers it — so the screen
        /// shows which matcher is ahead at each moment rather than which won overall.
        /// A table of totals cannot tell a config that is slower from one that is
        /// somewhere else entirely.
        #[arg(long)]
        compare: Option<PathBuf>,
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Do not play the audio. Pacing then falls back to the wall clock.
        #[arg(long)]
        no_monitor: bool,
        /// Serve the script for editing, with no audio and no recognition.
        ///
        /// Preparing a script — marking cuts, typing the stage directions, saying
        /// which are read aloud, placing the holds where music runs — is table work.
        /// It happens days before the show, and needing a sound card and a two-hour
        /// playback to reach the passage you want to fix makes it work nobody does.
        #[arg(long)]
        prep: bool,
        /// Listen to the patched inputs instead of the corpus audio.
        ///
        /// The same VAD, recognition and tracking as a replay — only where a block of
        /// audio comes from differs, which is exactly the seam this exists to prove.
        #[arg(long)]
        live: bool,
        /// Cue lists to display. Repeatable — a show has one per operator, and they
        /// are independent documents rather than views of one another. Defaults to
        /// every `cues.json` and `cues-*.json` beside the script.
        #[arg(long)]
        cues: Vec<std::path::PathBuf>,
        /// Output device to play through; substring match. Default device otherwise.
        #[arg(long)]
        output_device: Option<String>,
        /// Monitor buffer. Kept short on purpose: whatever sits in it is audio the
        /// screen has already seen, so a deep buffer would make the display look
        /// faster than it is.
        #[arg(long, default_value_t = 600)]
        buffer_ms: u32,
        /// Also write the trace, so a live run can be scored like an offline one.
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
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
        Command::Listen {
            library,
            seconds,
            scan,
        } => cmd::listen::run(
            &library.unwrap_or_else(cmd::show::default_root),
            seconds,
            scan,
        ),
        Command::Models { what, into } => match what {
            ModelsCmd::List => cmd::models::list(into),
            ModelsCmd::Fetch { medium } => cmd::models::fetch_all(into, medium),
        },
        Command::Ui { library, port } => {
            cmd::ui::run(library.unwrap_or_else(cmd::show::default_root), port)
        }
        Command::Show { what, library } => {
            let root = library.unwrap_or_else(cmd::show::default_root);
            match what {
                ShowCmd::List => cmd::show::list(&root),
                ShowCmd::New { name, from } => cmd::show::new(&root, &name, from.as_deref()),
                ShowCmd::Import { manifest, name } => {
                    cmd::show::import_show(&root, &manifest, name.as_deref())
                }
                ShowCmd::Rules => {
                    cmd::show::rules();
                    Ok(())
                }
                ShowCmd::Check { script, source } => {
                    cmd::show::check(&script, source.as_deref())
                }
                ShowCmd::Cues { show, from, name } => {
                    cmd::show::cues_in(&show, &from, name.as_deref())
                }
                ShowCmd::Export { sheet, to } => cmd::show::cues_out(&sheet, &to),
                ShowCmd::Text { script, from } => cmd::show::add_text(&script, &from),
            }
        }
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
            audio.agc_max_gain,
            audio.no_lexicon,
        ),
        Command::Serve {
            corpus,
            audio,
            audio_root,
            tracker_config,
            compare,
            port,
            no_monitor,
            output_device,
            buffer_ms,
            out,
            prep,
            live,
            cues,
        } => cmd::serve::run(
            &corpus,
            tracker_config.as_deref(),
            compare.as_deref(),
            audio.model.as_deref(),
            audio.vad_model.as_deref(),
            audio.bias.into(),
            audio.mixdown,
            audio.channels,
            audio.interim_ms,
            audio_root,
            audio.no_agc,
            audio.agc_max_gain,
            audio.no_lexicon,
            audio.realtime,
            port,
            !no_monitor && !prep && !live,
            output_device,
            buffer_ms,
            out,
            prep,
            live,
            &cues,
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
