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
///
/// Resolved against the crate directory at compile time, **not** against the working
/// directory. It was a repo-relative path, which meant it only resolved when the
/// binary happened to be run from the repository root — and from anywhere else the
/// lookup quietly failed and the page fell back to `include_str!`. That is worse than
/// no disk-serving at all: edits to the page appear to do nothing until the next
/// `cargo build` bakes them in, so a fix and a failed fix look identical. It cost an
/// afternoon here, twice, and the second time the operator was the one reporting that
/// a button did not work.
const ASSET_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/live.html"
);

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
    prep: bool,
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
            kind: match l.kind {
                choufleur_core::script::LineKind::Stage => "stage",
                choufleur_core::script::LineKind::Dialogue => "dialogue",
            }
            .to_string(),
            spoken: l.spoken,
            hold: l.hold.map(|h| {
                match h {
                    choufleur_core::script::Hold::Silence => "silence",
                    choufleur_core::script::Hold::Music => "music",
                    choufleur_core::script::Hold::Adlib => "adlib",
                }
                .to_string()
            }),
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
        prep,
    });

    // Nothing to load, nothing to play, nothing to decode. Serve the script and wait.
    if prep {
        println!("script:  {} lines", state.lines.lock().unwrap().len());
        println!("prep mode — no audio, no recognition; the script is here to be edited");
        return serve_http(state, port);
    }

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
fn write_line_edit(path: &Path, index: usize, edit: &LineEdit) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let line = doc
        .get_mut("lines")
        .and_then(|l| l.as_array_mut())
        .and_then(|l| l.get_mut(index))
        .context("no such line in the script")?;
    line["text"] = serde_json::Value::String(edit.text.clone());
    if let Some(c) = &edit.character {
        line["character"] = serde_json::Value::String(c.clone());
    }
    // Written even when false, so restoring a line is as durable as cutting one. A
    // cut that cannot be undone is a deletion wearing a different name.
    line["cut"] = serde_json::Value::Bool(edit.cut);
    // `spoken` likewise, always and explicitly. Its default depends on the kind, so
    // leaving it out would mean flipping a line from stage back to dialogue silently
    // changed whether it can be matched — and the operator setting it needs to see
    // which way round it is, in the file as well as on the page.
    line["kind"] = serde_json::Value::String(edit.kind.clone());
    line["spoken"] = serde_json::Value::Bool(edit.spoken);
    match &edit.hold {
        Some(h) => line["hold"] = serde_json::Value::String(h.clone()),
        None => {
            if let Some(o) = line.as_object_mut() {
                o.remove("hold");
            }
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Add a line to the script, after `after`, and give it an id of its own.
///
/// Ids carry a number for legibility but order comes from position in the array, so an
/// inserted line only needs to be *unique* — and must not renumber its neighbours,
/// because every cue, note and annotation in the show is anchored to an id. Renumbering
/// to keep them tidy is precisely this morning's cue-sheet drift, performed
/// deliberately. So an insert after `L-0075` becomes `L-0075-1`, then `L-0075-2`.
fn write_line_insert(path: &Path, after: usize, line: &LineView) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let lines = doc
        .get_mut("lines")
        .and_then(|l| l.as_array_mut())
        .context("script has no lines")?;
    let at = (after + 1).min(lines.len());
    let mut new = serde_json::Map::new();
    new.insert("id".into(), line.id.clone().into());
    // Act and scene are inherited from the neighbour rather than asked for: an
    // inserted line is always inside a scene that already exists, and making the
    // operator restate it would only be a chance to get it wrong.
    for key in ["act", "scene"] {
        let inherited = lines
            .get(after.min(lines.len().saturating_sub(1)))
            .and_then(|l| l.get(key))
            .cloned()
            .unwrap_or(serde_json::Value::String(String::new()));
        new.insert(key.into(), inherited);
    }
    new.insert("character".into(), line.character.clone().into());
    new.insert("text".into(), line.text.clone().into());
    new.insert("kind".into(), line.kind.clone().into());
    new.insert("spoken".into(), line.spoken.into());
    if let Some(h) = &line.hold {
        new.insert("hold".into(), h.clone().into());
    }
    lines.insert(at, serde_json::Value::Object(new));
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Remove a line from the script for good, keeping a copy where it can be found.
///
/// Distinct from `cut`, and the operator drew the line exactly right: *"a cut line is
/// an editing choice"* — the play contains it, tonight's production does not, and the
/// page must go on showing it struck through because a silently missing line is how
/// you lose your place. A mistake is not a choice. The importer turning the next
/// actor's name into something the previous one says produces text that was never in
/// the play at all, and leaving it on the page as a cut would be filing a typo as a
/// decision.
///
/// It still goes to a sidecar rather than into the void. The notation spec's
/// no-silent-data-loss rule is about not being able to *find out* what happened, not
/// about keeping rubbish on the page — and a deletion made at eleven at night is
/// exactly the one somebody will want to inspect at the next rehearsal.
fn write_line_delete(path: &Path, index: usize) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let lines = doc
        .get_mut("lines")
        .and_then(|l| l.as_array_mut())
        .context("script has no lines")?;
    anyhow::ensure!(index < lines.len(), "no such line in the script");
    let removed = lines.remove(index);

    // Recorded with the id it followed, not with its index: indices move with every
    // later edit, and an index is the one piece of information that will be wrong by
    // the time anybody reads this file.
    let after = index
        .checked_sub(1)
        .and_then(|i| lines.get(i))
        .and_then(|l| l.get("id"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let sidecar = path.with_extension("removed.json");
    let mut log: Vec<serde_json::Value> = std::fs::read_to_string(&sidecar)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_default();
    log.push(serde_json::json!({ "wasAfter": after, "line": removed }));
    std::fs::write(&sidecar, serde_json::to_string_pretty(&log)? + "\n")?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// An id unique in the whole script, derived from the line it follows.
fn insert_id(lines: &[LineView], after: usize) -> String {
    let base = lines
        .get(after)
        .map(|l| l.id.clone())
        .unwrap_or_else(|| "L-0000".into());
    let taken: std::collections::HashSet<&str> = lines.iter().map(|l| l.id.as_str()).collect();
    (1..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken.contains(id.as_str()))
        .expect("an unused suffix always exists")
}

/// One line's worth of correction, as it arrives from the page.
struct LineEdit {
    text: String,
    character: Option<String>,
    cut: bool,
    kind: String,
    spoken: bool,
    hold: Option<String>,
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
                                        // Prep only, and not out of caution. An insert
                                        // moves every index after it, while the running
                                        // tracker holds a `PreparedScript` built once
                                        // for the whole run — so mid-show it would put
                                        // the page's second half on the wrong text
                                        // while reporting perfect confidence. Editing a
                                        // line's words is safe because indices do not
                                        // move; adding one is not.
                                        Some("insert_line") if inbound.prep => {
                                            let line = {
                                                let mut lines = inbound.lines.lock().unwrap();
                                                let after = i.min(lines.len().saturating_sub(1));
                                                let neighbour = &lines[after];
                                                let line = LineView {
                                                    id: insert_id(&lines, after),
                                                    character: v
                                                        .get("character")
                                                        .and_then(|c| c.as_str())
                                                        .unwrap_or(&neighbour.character)
                                                        .to_string(),
                                                    text: v
                                                        .get("text")
                                                        .and_then(|t| t.as_str())
                                                        .unwrap_or("")
                                                        .to_string(),
                                                    scene: neighbour.scene.clone(),
                                                    cut: false,
                                                    kind: v
                                                        .get("kind")
                                                        .and_then(|k| k.as_str())
                                                        .filter(|k| *k == "stage")
                                                        .unwrap_or("dialogue")
                                                        .to_string(),
                                                    // A new stage direction is unvoiced
                                                    // until said otherwise. This is the
                                                    // one place that default belongs —
                                                    // at creation, never on an edit.
                                                    spoken: v
                                                        .get("spoken")
                                                        .and_then(|b| b.as_bool())
                                                        .unwrap_or(
                                                            v.get("kind")
                                                                .and_then(|k| k.as_str())
                                                                != Some("stage"),
                                                        ),
                                                    hold: v
                                                        .get("hold")
                                                        .and_then(|h| h.as_str())
                                                        .filter(|h| {
                                                            matches!(
                                                                *h,
                                                                "silence" | "music" | "adlib"
                                                            )
                                                        })
                                                        .map(str::to_string),
                                                };
                                                lines.insert(after + 1, line.clone());
                                                line
                                            };
                                            if let Err(e) = write_line_insert(
                                                &inbound.script_path,
                                                i,
                                                &line,
                                            ) {
                                                eprintln!("could not save the new line: {e:#}");
                                            } else {
                                                println!("inserted {} after line {}", line.id, i + 1);
                                            }
                                            let _ = inbound.tx.send(Update::LineInserted {
                                                line_index: i + 1,
                                                line,
                                            });
                                        }
                                        // Prep only, for the same reason as the
                                        // insert: it moves every index after it.
                                        Some("delete_line") if inbound.prep => {
                                            let gone = {
                                                let mut lines = inbound.lines.lock().unwrap();
                                                // The last line has to stay: an empty
                                                // script is a state with no way back
                                                // through this interface.
                                                if lines.len() <= 1 {
                                                    continue;
                                                }
                                                lines.remove(i)
                                            };
                                            if let Err(e) =
                                                write_line_delete(&inbound.script_path, i)
                                            {
                                                eprintln!("could not delete the line: {e:#}");
                                            } else {
                                                println!(
                                                    "deleted {} — {:?}",
                                                    gone.id,
                                                    gone.text.chars().take(48).collect::<String>()
                                                );
                                            }
                                            let _ = inbound
                                                .tx
                                                .send(Update::LineDeleted { line_index: i });
                                        }
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
                                            // `hold` carries three states, not two:
                                            // absent leaves it alone, null clears it,
                                            // a string sets it. Collapsing null into
                                            // absent would make a hold impossible to
                                            // remove once placed.
                                            let hold_given = v.get("hold");
                                            let kind = v
                                                .get("kind")
                                                .and_then(|k| k.as_str())
                                                .filter(|k| *k == "stage" || *k == "dialogue")
                                                .map(str::to_string);
                                            let spoken = v.get("spoken").and_then(|b| b.as_bool());
                                            let edit = {
                                                let mut lines = inbound.lines.lock().unwrap();
                                                let l = &mut lines[i];
                                                l.text = text.clone();
                                                if let Some(c) = &character {
                                                    l.character = c.clone();
                                                }
                                                if let Some(c) = cut {
                                                    l.cut = c;
                                                }
                                                if let Some(k) = kind {
                                                    l.kind = k;
                                                }
                                                if let Some(b) = spoken {
                                                    l.spoken = b;
                                                }
                                                if let Some(h) = hold_given {
                                                    l.hold = h
                                                        .as_str()
                                                        .filter(|h| {
                                                            matches!(
                                                                *h,
                                                                "silence" | "music" | "adlib"
                                                            )
                                                        })
                                                        .map(str::to_string);
                                                }
                                                LineEdit {
                                                    text: l.text.clone(),
                                                    character: character.clone(),
                                                    cut: l.cut,
                                                    kind: l.kind.clone(),
                                                    spoken: l.spoken,
                                                    hold: l.hold.clone(),
                                                }
                                            };
                                            if let Err(e) =
                                                write_line_edit(&inbound.script_path, i, &edit)
                                            {
                                                eprintln!("could not save the edit: {e:#}");
                                            } else {
                                                println!("edited line {}: {text}", i + 1);
                                            }
                                            let _ = inbound.tx.send(Update::LineEdited {
                                                line_index: i,
                                                text,
                                                character,
                                                cut: edit.cut,
                                                kind: edit.kind,
                                                spoken: edit.spoken,
                                                hold: edit.hold,
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
