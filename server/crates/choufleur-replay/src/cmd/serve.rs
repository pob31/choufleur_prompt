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
use crate::live::{Broadcast, LineView, LiveState, Update};
use crate::manifest::Corpus;
use crate::monitor::Monitor;

/// Where the page is read from while iterating, if it is present.
///
/// Compiled-in remains the fallback so a built binary is still self-contained.
const ASSET_PATH: &str = "server/crates/choufleur-replay/assets/live.html";

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
    no_lexicon: bool,
    realtime: bool,
    port: u16,
    monitor: bool,
    output_device: Option<String>,
    buffer_ms: u32,
    trace_out: Option<PathBuf>,
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
        })
        .collect();

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
    });

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
fn write_line_edit(
    path: &Path,
    index: usize,
    text: &str,
    character: Option<&str>,
    cut: bool,
) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let line = doc
        .get_mut("lines")
        .and_then(|l| l.as_array_mut())
        .and_then(|l| l.get_mut(index))
        .context("no such line in the script")?;
    line["text"] = serde_json::Value::String(text.to_string());
    if let Some(c) = character {
        line["character"] = serde_json::Value::String(c.to_string());
    }
    // Written even when false, so restoring a line is as durable as cutting one. A
    // cut that cannot be undone is a deletion wearing a different name.
    line["cut"] = serde_json::Value::Bool(cut);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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
                                    let Some(i) = v
                                        .get("lineIndex")
                                        .and_then(|i| i.as_u64())
                                        .map(|i| i as usize)
                                        .filter(|i| *i < inbound.lines.lock().unwrap().len())
                                    else {
                                        continue;
                                    };
                                    match kind {
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
                                            let cut = {
                                                let mut lines = inbound.lines.lock().unwrap();
                                                lines[i].text = text.clone();
                                                if let Some(c) = &character {
                                                    lines[i].character = c.clone();
                                                }
                                                if let Some(c) = cut {
                                                    lines[i].cut = c;
                                                }
                                                lines[i].cut
                                            };
                                            if let Err(e) = write_line_edit(
                                                &inbound.script_path,
                                                i,
                                                &text,
                                                character.as_deref(),
                                                cut,
                                            ) {
                                                eprintln!("could not save the edit: {e:#}");
                                            } else {
                                                println!("edited line {}: {text}", i + 1);
                                            }
                                            let _ = inbound.tx.send(Update::LineEdited {
                                                line_index: i,
                                                text,
                                                character,
                                                cut,
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
