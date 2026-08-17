//! The server UI: the screens for the machine that runs the show.
//!
//! Separate from `serve` on purpose. A show server holds exactly one show, fixed when it
//! starts — the operator's rule, and the reason `LiveState` never had to become
//! swappable. This one holds no show at all. It lists the library, makes shows, fills
//! them, checks them, and starts a show server when asked.
//!
//! Opening a show therefore spawns `serve` as a child and links to it, one at a time.
//! That is the same lifecycle the desktop shell will own later, so it is written once
//! here rather than twice.

use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use choufleur_server::{check, import, Library, Store};
use serde::{Deserialize, Serialize};

struct Running {
    name: String,
    port: u16,
    child: Child,
    /// The write end of the child's stdin, held and never written to.
    ///
    /// Not a channel — a deadline. Closing it, or dying and letting the kernel close
    /// it, is what tells the child we are gone; see [`crate::supervise`]. It is the
    /// only signal that still arrives when this process is killed outright.
    stdin: Option<std::process::ChildStdin>,
}

struct Ui {
    root: PathBuf,
    port: u16,
    /// The one show server this process has started, if any.
    show: Mutex<Option<Running>>,
    /// A model download, while one is happening.
    ///
    /// Only whether it is running and what went wrong last. How far along it is comes
    /// from the size of the `.part` file on disk, which is the truth and which a
    /// download started from a terminal — or finished by hand on a stick — moves in
    /// exactly the same way.
    fetching: Mutex<Fetching>,
}

#[derive(Default)]
struct Fetching {
    running: bool,
    error: Option<String>,
}

/// Anything a handler can fail with, rendered as the message the screen shows.
///
/// The wording matters more than the status code here: every one of these errors is
/// something the operator can act on — a name already taken, a file that moved, a
/// snapshot that could not be written — and the page puts the text on screen verbatim.
struct Fail(anyhow::Error);

impl IntoResponse for Fail {
    fn into_response(self) -> axum::response::Response {
        (StatusCode::BAD_REQUEST, format!("{:#}", self.0)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.into())
    }
}

type Reply<T> = std::result::Result<T, Fail>;

// The wire shapes. Deliberately not the internal types: `library::Entry` and
// `store::Version` carry `PathBuf`s that mean nothing to a browser, and keeping the two
// apart means the screen can change without the library having to.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ShowDto {
    name: String,
    title: Option<String>,
    lines: usize,
    sheets: Vec<String>,
    versions: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionDto {
    stamp: String,
    /// `2026-08-16 21:04`, for reading rather than sorting.
    when: String,
    files: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StateDto {
    root: String,
    /// The show that is up, and what it is doing — see [`state`]. Not `OpenDto`,
    /// because "which show" and "what is it doing" come from different processes.
    open: Option<serde_json::Value>,
    /// What to give the operators — see [`lan_addresses`]. Shown on the library screen
    /// because that is the screen somebody is looking at when they read it out.
    operators: Vec<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct OpenDto {
    name: String,
    port: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewShow {
    name: String,
    /// A script pasted straight in. Optional: a show can start empty.
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportShow {
    manifest: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Restore {
    stamp: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckReq {
    /// The text the script was prepared from, if the operator has it to hand. Without
    /// it the coverage check — the one that matters — cannot run.
    #[serde(default)]
    source: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenReq {
    /// Prep by default: opening a show from the library is preparation, and starting
    /// audio is a separate decision somebody makes deliberately.
    #[serde(default = "yes")]
    prep: bool,
    /// Listen to the room.
    ///
    /// The other half of that deliberate decision, and until it existed the decision
    /// could not be made at all: the screen could open a show for preparation and
    /// nothing else, so the tracker was unreachable from the app that is the only
    /// thing macOS will give a microphone to.
    ///
    /// Replaying a recording — a run with neither flag — stays a command-line matter.
    /// The way to watch the tracker against a recording is to play it into the patch,
    /// which exercises capture, resampling and the venue map rather than going around
    /// them, and needs nothing here.
    #[serde(default)]
    live: bool,
}

fn yes() -> bool {
    true
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    ok: bool,
    text: String,
}

impl Ui {
    fn library(&self) -> Library {
        Library::new(&self.root)
    }

    fn entry(&self, name: &str) -> Result<choufleur_server::Entry> {
        // A show name is one directory inside the library, and nothing else.
        //
        // Unsanitised, this took `../elsewhere` — and, because `Path::join` throws away
        // everything to the left when its argument is absolute, `/anywhere` as well. A
        // request from any tablet on the venue network could therefore point a show
        // server at a directory outside the library and have it served back, or take a
        // snapshot into one. `Library::create` has always cleaned the names it makes;
        // this path, which accepts a name rather than making one, never did.
        let one_component = std::path::Path::new(name).components().count() == 1;
        if name.is_empty() || !one_component || name.contains('/') || name.contains('\\') {
            anyhow::bail!("no show called {name:?}");
        }
        self.library()
            .describe(&self.root.join(name))
            .with_context(|| format!("no show called {name:?}"))
    }

    /// A store over one show, guarding its script *and* every cue sheet.
    ///
    /// Both, always. A snapshot of a script without its sheets restores to a state where
    /// every cue anchors to a line that has since moved — which `store.rs` calls a
    /// version you cannot go back to, and it is right.
    fn store(&self, name: &str) -> Result<(Store, choufleur_server::Entry)> {
        let e = self.entry(name)?;
        let mut guarded = vec![e.script.clone()];
        guarded.extend(e.sheets.iter().cloned());
        Ok((Store::new(e.dir.clone(), guarded), e))
    }
}

/// The show server, if one is still alive.
///
/// Asked with `try_wait` rather than taken on trust, because nothing else reaps it. A
/// child that died on its own — a crash, or an engine that could not open the room —
/// left its name and its port in the slot, and every screen that asked was told a show
/// was running and sent to a port with nothing on it.
fn live_show(ui: &Ui) -> Option<(String, u16)> {
    let mut slot = ui.show.lock().unwrap();
    if slot
        .as_mut()
        .is_some_and(|r| matches!(r.child.try_wait(), Ok(Some(_))))
    {
        slot.take();
    }
    slot.as_ref().map(|r| (r.name.clone(), r.port))
}

async fn state(State(ui): State<Arc<Ui>>) -> Json<StateDto> {
    let mut open = None;
    if let Some((name, port)) = live_show(&ui) {
        // Asked of the show server rather than remembered from how it was started: a
        // run whose engine failed is still "not prep" but is no longer running, and the
        // screen that offers to close it should say which of those it is about to end.
        let run = ask_show_server(port, "/run.json").await;
        let flag = |k: &str| {
            run.as_ref()
                .and_then(|v| v.get(k))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        open = Some(serde_json::json!({
            "name": name,
            "port": port,
            "prep": flag("prep"),
            "running": flag("running"),
        }));
    }
    Json(StateDto {
        root: ui.root.to_string_lossy().into_owned(),
        open,
        operators: lan_addresses()
            .into_iter()
            .map(|a| format!("http://{a}:{}/", ui.port))
            .collect(),
    })
}

async fn list(State(ui): State<Arc<Ui>>) -> Reply<Json<Vec<ShowDto>>> {
    let shows = ui.library().list()?;
    Ok(Json(
        shows
            .into_iter()
            .map(|s| ShowDto {
                name: s.name,
                title: s.title,
                lines: s.lines,
                sheets: s
                    .sheets
                    .iter()
                    .filter_map(|p| p.file_stem().map(|n| n.to_string_lossy().into_owned()))
                    .collect(),
                versions: s.versions,
            })
            .collect(),
    ))
}

async fn create(State(ui): State<Arc<Ui>>, Json(req): Json<NewShow>) -> Reply<Json<Report>> {
    // `Library::create` does not make the library root — the CLI does it first, and so
    // must this, or the very first show on a fresh machine fails.
    std::fs::create_dir_all(&ui.root)
        .with_context(|| format!("creating the library at {}", ui.root.display()))?;
    let show = ui.library().create(&req.name)?;

    let text = req.text.unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(Json(Report {
            ok: true,
            text: format!("created {}\nempty — paste a script into it next", show.name),
        }));
    }
    let report = import::text(&show.script, &text, None)?;
    Ok(Json(Report {
        ok: true,
        text: report.to_string(),
    }))
}

async fn import_show(
    State(ui): State<Arc<Ui>>,
    Json(req): Json<ImportShow>,
) -> Reply<Json<Report>> {
    std::fs::create_dir_all(&ui.root)?;
    let show = ui
        .library()
        .import(std::path::Path::new(&req.manifest), req.name.as_deref())?;
    Ok(Json(Report {
        ok: true,
        text: format!(
            "imported {} — {} lines, {} cue list{}\nthe original was not touched",
            show.name,
            show.lines,
            show.sheets.len(),
            if show.sheets.len() == 1 { "" } else { "s" }
        ),
    }))
}

async fn versions(
    State(ui): State<Arc<Ui>>,
    Path(name): Path<String>,
) -> Reply<Json<Vec<VersionDto>>> {
    let (store, _) = ui.store(&name)?;
    Ok(Json(
        store
            .versions()?
            .into_iter()
            .map(|v| VersionDto {
                when: readable(&v.stamp),
                stamp: v.stamp,
                files: v
                    .files
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .collect(),
            })
            .collect(),
    ))
}

/// `2026-08-16T21-04-11` as `2026-08-16 21:04`.
///
/// Seconds are dropped: the stamp needs them to stay unique, a person reading a list of
/// versions does not. A collision suffix — `…-11-2`, when two snapshots landed in the
/// same second — falls off the same way.
fn readable(stamp: &str) -> String {
    let Some((day, time)) = stamp.split_once('T') else {
        return stamp.to_string();
    };
    let mut parts = time.split('-');
    match (parts.next(), parts.next()) {
        (Some(h), Some(m)) => format!("{day} {h}:{m}"),
        _ => stamp.to_string(),
    }
}

async fn restore(
    State(ui): State<Arc<Ui>>,
    Path(name): Path<String>,
    Json(req): Json<Restore>,
) -> Reply<Json<Report>> {
    let (store, _) = ui.store(&name)?;
    store.restore(&req.stamp)?;
    Ok(Json(Report {
        ok: true,
        text: format!(
            "restored {}\nthe state before the restore was saved first, so this is undoable",
            req.stamp
        ),
    }))
}

async fn check_show(
    State(ui): State<Arc<Ui>>,
    Path(name): Path<String>,
    Json(req): Json<CheckReq>,
) -> Reply<Json<Report>> {
    let e = ui.entry(&name)?;
    let source = req.source.filter(|s| !s.trim().is_empty());
    let report = check::script(&e.script, source.as_deref())?;
    Ok(Json(Report {
        ok: report.ok(),
        text: report.to_string(),
    }))
}

async fn open(
    State(ui): State<Arc<Ui>>,
    Path(name): Path<String>,
    Json(req): Json<OpenReq>,
) -> Reply<Json<OpenDto>> {
    let e = ui.entry(&name)?;
    // Asked here rather than discovered by the child.
    //
    // Without the models a show server starts, fails to load them, and exits — and
    // all this end sees is a readiness check that times out after twelve seconds and
    // says the script may not load, which is both wrong and slow. Prep needs no
    // models at all and is left alone.
    if !req.prep {
        if !crate::cmd::models::ready(&models_dir(&ui)) {
            return Err(Fail(anyhow::anyhow!(
                "the recogniser's models are not here yet — download them from the panel at \
                 the top of this screen, or run `choufleur-replay models fetch`. Opening for \
                 prep works without them."
            )));
        }
        if req.live {
            check_the_patch(&ui.root)?;
        }
    }
    // A port nothing else is on.
    //
    // This used to be `ui.port + 1` and that was a trap. A show server outlives its
    // parent if the parent is killed rather than asked to stop, and the orphan keeps
    // the port — so the next spawn failed to bind, the readiness check found the
    // *orphan* listening and reported success, and the browser was handed a stale
    // server running older code and a different show. Both symptoms reported — a patch
    // that reverted, and every show opening as Hécube — were that one orphan.
    let port = free_port(ui.port + 1)?;
    // The lock is scoped rather than dropped, because a `MutexGuard` is not `Send` and
    // a handler that holds one across an await will not compile — which is the right
    // rule, and here it also keeps the spawn and the wait properly separated.
    {
        let mut slot = ui.show.lock().unwrap();
        // One show at a time. Whatever was open closes first, so there is never a
        // second server quietly holding a lock on a different show's files.
        if let Some(old) = slot.take() {
            stop_child(old);
        }
        let exe = std::env::current_exe().context("finding this binary")?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("serve")
            .arg(&e.manifest)
            .arg("--port")
            .arg(port.to_string())
            // A pipe the child watches for our death, and the contract that comes with
            // the variable: whoever sets it owes the child a stdin it can read.
            .stdin(std::process::Stdio::piped())
            .env("CHOUFLEUR_SUPERVISED", "1")
            // Which library this show belongs to. Without it the child falls back to
            // `$HOME/Choufleur` and reads — and writes — the patch of a different
            // library entirely, which is how a show opened from the scratchpad ended up
            // saving its microphones into the home directory.
            .env("CHOUFLEUR_LIBRARY", &ui.root);
        if req.prep {
            cmd.arg("--prep");
        }
        if req.live {
            // The monitor is already refused alongside this (see `main.rs`): playing a
            // recording into the room while listening to the room is neither useful nor
            // safe.
            cmd.arg("--live");
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting a show server for {}", e.name))?;
        let stdin = child.stdin.take();
        *slot = Some(Running {
            name: e.name.clone(),
            port,
            child,
            stdin,
        });
    }

    if let Err(waited) = wait_until_listening(port, &e.name).await {
        // It never came up, so it is not the open show and must not be announced as
        // one. Guarded by the port rather than taken blindly: `free_port` runs before
        // the lock, so two opens can overlap, and an unguarded `take()` here would
        // reach into the slot and kill the show that did start.
        let dead = {
            let mut slot = ui.show.lock().unwrap();
            match slot.as_ref() {
                Some(r) if r.port == port => slot.take(),
                _ => None,
            }
        };
        if let Some(dead) = dead {
            stop_child(dead);
        }
        return Err(waited.into());
    }
    Ok(Json(OpenDto { name: e.name, port }))
}

/// End a show server, asking before insisting.
///
/// Three steps, weakest first, because a show server that is asked to stop writes
/// nothing more and lets go of the audio interface, and one that is killed outright
/// does neither. The pipe goes first because it reaches the child even if the signal
/// does not; SIGTERM is the ordinary path; SIGKILL is what is left when a process is
/// wedged, and taking it means something was already wrong.
///
/// Blocks the caller for up to three seconds. A show server answers in a fraction of
/// that unless it is mid-decode, and this server has one operator.
fn stop_child(mut r: Running) {
    // Closing the pipe: the child's supervisor thread sees EOF and raises SIGTERM on
    // itself, which is the same shutdown the signal below asks for.
    drop(r.stdin.take());
    let pid = r.child.id() as libc::pid_t;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match r.child.try_wait() {
            Ok(Some(_)) => return,
            // Already gone, and somebody else reaped it.
            Err(_) => return,
            Ok(None) => {}
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    // Not `eprintln!`: this often runs with the supervisor already gone, and printing
    // to a pipe nobody is reading panics.
    crate::supervise::say(&format!("{} did not stop when asked; killing it", r.name));
    let _ = r.child.kill();
    let _ = r.child.wait();
}

/// Kill any show server left behind by a previous run of this binary.
///
/// A parent that is killed outright cannot tidy up after itself, and what it leaves
/// is a server holding a port and serving a show nobody asked for. That orphan has
/// twice been mistaken for a server that had just started — most memorably as every
/// show opening as Hécube. Scanning for a free port stopped the collision; this stops
/// the orphan, which is the thing that was actually wrong.
///
/// Deliberately narrow: only processes whose parent is already gone (`ppid` 1), and
/// only this exact binary running `serve`. A show server started by hand from a
/// terminal still has its shell for a parent and is none of our business.
fn sweep_orphans() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let prefix = format!("{} serve", exe.display());
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
    else {
        return;
    };
    let me = std::process::id();
    let mut found = 0;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        if ppid != 1 || pid == me {
            continue;
        }
        let command = line
            .split_once(char::is_whitespace)
            .and_then(|(_, rest)| rest.trim_start().split_once(char::is_whitespace))
            .map(|(_, rest)| rest.trim_start())
            .unwrap_or("");
        if !command.starts_with(&prefix) {
            continue;
        }
        eprintln!("an orphaned show server was still running (pid {pid}); stopping it");
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        found += 1;
    }
    // Long enough for a signalled server to let go of its port, so the next show can
    // have the port it would have had anyway.
    if found > 0 {
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// The first port from `from` upward that nothing is listening on.
fn free_port(from: u16) -> Result<u16> {
    for port in from..from.saturating_add(40) {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!("no free port near {from} — something else is using them")
}

/// Poll until the show server answers, or give up and say so.
async fn wait_until_listening(port: u16, name: &str) -> Result<()> {
    for _ in 0..120 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "{name} did not start. Its script may not load — try Check, which reads it the \
         same way the show server does."
    )
}

/// What the recogniser needs, and how much of it is here.
///
/// Read from the folder on every request rather than remembered, so a file copied in
/// from a stick — which is how this goes when a venue blocks the download — shows up
/// without anything having to be told.
async fn models(State(ui): State<Arc<Ui>>) -> Json<serde_json::Value> {
    let dir = models_dir(&ui);
    let files: Vec<serde_json::Value> = crate::cmd::models::CATALOG
        .iter()
        .map(|m| {
            let (state, got) = match crate::cmd::models::state_of(&dir, m) {
                crate::cmd::models::State::Ready => ("ready", m.bytes),
                crate::cmd::models::State::Partial { got } => ("partial", got),
                crate::cmd::models::State::Absent => ("absent", 0),
            };
            serde_json::json!({
                "file": m.file,
                "label": m.label,
                "bytes": m.bytes,
                "size": crate::cmd::models::megabytes(m.bytes),
                "got": got,
                "state": state,
                "optional": m.optional,
                "url": m.urls[0],
            })
        })
        .collect();
    let f = ui.fetching.lock().unwrap();
    Json(serde_json::json!({
        "dir": dir.to_string_lossy(),
        "ready": crate::cmd::models::ready(&dir),
        "fetching": f.running,
        "error": f.error,
        "models": files,
    }))
}

/// Start fetching whatever is missing. Answers immediately; the screen watches
/// `/api/models` to see it happen.
async fn fetch_models(State(ui): State<Arc<Ui>>) -> Reply<Json<Report>> {
    {
        let mut f = ui.fetching.lock().unwrap();
        if f.running {
            return Ok(Json(Report {
                ok: true,
                text: "already downloading".into(),
            }));
        }
        f.running = true;
        f.error = None;
    }
    let dir = models_dir(&ui);
    let ui2 = Arc::clone(&ui);
    // On a blocking thread: this is minutes of network and disk, and it must not sit
    // on the runtime that the screen is asking for progress on.
    tokio::task::spawn_blocking(move || {
        let mut outcome = Ok(());
        for m in crate::cmd::models::CATALOG.iter().filter(|m| !m.optional) {
            if let Err(e) = crate::cmd::models::fetch(&dir, m) {
                outcome = Err(format!("{e:#}"));
                break;
            }
        }
        let mut f = ui2.fetching.lock().unwrap();
        f.running = false;
        f.error = outcome.err();
    });
    Ok(Json(Report {
        ok: true,
        text: "downloading".into(),
    }))
}

/// Show the models folder in the Finder, making it if it is not there yet.
///
/// For the case the download cannot help with: a venue network that will not fetch
/// them, and a file to be dropped in by hand from a stick. Opens on the machine
/// running the server, which is the machine the folder is on.
async fn reveal_models(State(ui): State<Arc<Ui>>) -> Reply<Json<Report>> {
    let dir = models_dir(&ui);
    std::fs::create_dir_all(&dir)?;
    std::process::Command::new("open").arg(&dir).spawn()?;
    Ok(Json(Report {
        ok: true,
        text: dir.to_string_lossy().into_owned(),
    }))
}

/// Ask the show server something, on the operator's behalf.
///
/// The join page runs on this port and the show server is on another, which makes a
/// browser fetch between them cross-origin. Rather than opening the show server up to
/// requests from anywhere, this end asks — it is a process on the same machine that
/// already knows where its own child is.
async fn ask_show_server(port: u16, path: &str) -> Option<serde_json::Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .ok()?
    .ok()?;
    // HTTP/1.0 with no keep-alive: the body ends when the connection closes, so there
    // is no length to parse and nothing to get wrong.
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).await.ok()?;
    let mut raw = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        // Bounded: a reply this size that keeps arriving is a server answering a
        // different question.
        sock.take(256 * 1024).read_to_end(&mut raw),
    )
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n")?;
    if !head.lines().next()?.contains(" 200") {
        return None;
    }
    serde_json::from_str(body.trim()).ok()
}

/// The cue lists an operator can take, and which show they belong to.
///
/// Answers even when nothing is open — a tablet is as often opened before the show is
/// started as after, and "no show yet" is a better page than an error.
async fn lists(State(ui): State<Arc<Ui>>) -> Json<serde_json::Value> {
    let Some((name, port)) = live_show(&ui) else {
        return Json(serde_json::json!({ "show": null, "lists": [] }));
    };
    // Open and running are different things, and the difference is the operator's.
    // A show open for prep will never move under them, and — because prep puts the
    // page into editing — a tap on a line edits the script instead of steering it.
    // Saying "now running" over that is the worst thing this page could do.
    let run = ask_show_server(port, "/run.json").await;
    let flag = |k: &str| {
        run.as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    };
    let sheets = ask_show_server(port, "/sheets.json")
        .await
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let lists: Vec<serde_json::Value> = sheets
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
                "name": s.get("name").and_then(|v| v.as_str()).unwrap_or_default(),
            })
        })
        .collect();
    Json(serde_json::json!({
        "show": name,
        "port": port,
        "prep": flag("prep"),
        "running": flag("running"),
        "lists": lists,
    }))
}

/// `/list/<id>` — the address an operator keeps.
///
/// It lives on this port, which never moves, and points at whichever show server is
/// open now. A bookmark straight to the show server would carry a port that is chosen
/// at open time and slides upward when something else is holding it, so it would work
/// until the one night it did not.
///
/// The host comes from the request rather than from anything this end knows, because
/// the operator typed an address that reaches this machine and the redirect has to
/// keep reaching it — `localhost` would send a tablet to itself.
async fn to_list(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let Some((_, port)) = live_show(&ui) else {
        // Nothing to show yet. The join page says so, and keeps looking.
        return axum::response::Redirect::to("/").into_response();
    };
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(without_port)
        .filter(|h| !h.is_empty())
        .unwrap_or("localhost")
        .to_string();
    // An unknown id is not corrected here: the show page falls back to a sensible list
    // and says so, which is better than this end guessing what was meant.
    let url = format!(
        "http://{host}:{port}/?list={}",
        urlencode(&id)
    );
    axum::response::Redirect::to(&url).into_response()
}

/// The addresses an operator can reach this machine on, best first.
///
/// The whole point of `/list/<id>` is an address somebody keeps, and until this
/// existed there was no way for anyone to learn what it was: the only thing printed
/// was `localhost`, which is useless from a tablet, and inside the app it is printed
/// into a pipe nobody reads.
///
/// The Bonjour name comes first on purpose. It survives the DHCP lease that the
/// address does not — the number can change between the get-in and the half — and an
/// iPad resolves `.local` with nothing configured.
fn lan_addresses() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(o) = std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
    {
        let name = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !name.is_empty() {
            out.push(format!("{name}.local"));
        }
    }
    // Which interface a tablet would arrive on, asked of the routing table rather than
    // guessed from a list. A "connected" UDP socket sends no packet — it only picks the
    // route — so this costs nothing and touches the network not at all. The address is
    // in the documentation range, which is routed nowhere by design.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("192.0.2.1:9").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                let ip = addr.ip().to_string();
                if ip != "0.0.0.0" {
                    out.push(ip);
                }
            }
        }
    }
    out
}

/// A `Host` header without its port.
///
/// Only a trailing `:digits` is removed. Splitting on the last colon would eat half of
/// an IPv6 literal — `[::1]` is a host, `[::1]:8080` is a host and a port — and the
/// show would be sent to an address that does not exist.
fn without_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => host,
    }
}

/// Percent-encode the few characters a sheet id could carry into a query string.
///
/// Ids are file stems, so in practice they are already safe; this is for the day one
/// is not, rather than a reason to add a dependency.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'*' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Is there a room to listen to?
///
/// Asked before the show server is started, because afterwards nobody asks at all.
///
/// A capture that cannot open does not stop the show server: the engine runs on its
/// own thread, serving carries on regardless, and the failure becomes one line on a
/// stderr the app does not display. The child binds, the readiness check passes, and
/// this end reports success over a show that will never move. (`serve` now announces
/// the stopped engine to every connected screen, which is the other half of this; but
/// a run that never starts is better than a run that starts and says it is sorry.)
///
/// The two things that go wrong are an empty patch and an interface that is not
/// plugged in tonight, and neither is the script's fault.
///
/// The device is matched the way `capture::build` matches it — lowercased, by
/// substring — because a check that disagreed with the thing it is checking for would
/// be worse than no check at all.
fn check_the_patch(library: &std::path::Path) -> Result<()> {
    let venue = crate::audio::load(library);
    if venue.channels.is_empty() {
        anyhow::bail!(
            "nothing is patched yet — open the show for prep, then set an input against \
             a channel in the audio panel. That patch belongs to this machine and is \
             kept for every show."
        );
    }
    if venue.device.trim().is_empty() {
        // No name means the default input, which is whatever the machine says it is.
        return Ok(());
    }
    let want = venue.device.to_lowercase();
    let inputs = crate::audio::inputs();
    if inputs.iter().any(|i| i.name.to_lowercase().contains(&want)) {
        return Ok(());
    }
    let offered = if inputs.is_empty() {
        "this machine is offering no inputs at all".to_string()
    } else {
        format!(
            "what is here: {}",
            inputs
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    anyhow::bail!(
        "the patch is set to {:?}, which is not connected — {offered}. Plug it in, or \
         open the show for prep and pick another device in the audio panel.",
        venue.device
    )
}

/// The models folder for this library — `<library>/models`, honouring an explicit
/// `$CHOUFLEUR_MODELS` so a machine that keeps them elsewhere is not contradicted.
fn models_dir(ui: &Ui) -> PathBuf {
    match std::env::var_os("CHOUFLEUR_MODELS") {
        Some(dir) => PathBuf::from(dir),
        None => ui.root.join("models"),
    }
}

async fn close(State(ui): State<Arc<Ui>>) -> Reply<Json<Report>> {
    let mut slot = ui.show.lock().unwrap();
    let Some(r) = slot.take() else {
        return Ok(Json(Report {
            ok: true,
            text: "nothing was open".into(),
        }));
    };
    let name = r.name.clone();
    stop_child(r);
    Ok(Json(Report {
        ok: true,
        text: format!("closed {name}"),
    }))
}

impl Ui {
    fn new(root: PathBuf, port: u16) -> Self {
        Ui {
            root,
            port,
            show: Mutex::new(None),
            fetching: Mutex::new(Fetching::default()),
        }
    }
}

pub fn run(root: PathBuf, port: u16) -> Result<()> {
    crate::supervise::arm();
    // Before anything binds: whatever the last run left behind is still answering.
    sweep_orphans();
    let ui = Arc::new(Ui::new(root.clone(), port));
    println!("library: {}", root.display());
    if !root.exists() {
        println!("         (does not exist yet — it is made with the first show)");
    }

    let app = axum::Router::new()
        // The root belongs to the operators.
        //
        // It is the address everyone is given — the one printed on a card taped to the
        // desk — so it answers with the question they have: which list are you? The
        // library, which can create shows, restore snapshots and close the running one,
        // used to be what answered here, on the obvious port, to everyone on the venue's
        // network. It now lives at `/admin`. That is not a lock, and nothing here
        // pretends otherwise; it is the difference between a control being tucked away
        // and a control being the first thing a curious tablet finds.
        .route("/", asset_route!("join.html", "text/html; charset=utf-8"))
        .route("/admin", asset_route!("shows.html", "text/html; charset=utf-8"))
        .route("/api/lists", get(lists))
        .route("/list/{id}", get(to_list))
        .route("/app.css", asset_route!("app.css", "text/css; charset=utf-8"))
        .route("/app.js", asset_route!("app.js", "text/javascript; charset=utf-8"))
        .route("/api/state", get(state))
        .route("/api/shows", get(list).post(create))
        .route("/api/import", post(import_show))
        .route("/api/shows/{name}/versions", get(versions))
        .route("/api/shows/{name}/restore", post(restore))
        .route("/api/shows/{name}/check", post(check_show))
        .route("/api/shows/{name}/open", post(open))
        .route("/api/close", post(close))
        .route("/api/models", get(models))
        .route("/api/models/fetch", post(fetch_models))
        .route("/api/models/reveal", post(reveal_models))
        .with_state(Arc::clone(&ui));

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        // Two addresses, because they are two different doors and the second one is the
        // whole point of the first. Operators need an address to type on a tablet, and
        // until this line nothing anywhere told anybody what it was.
        println!("\n  the library:   http://localhost:{port}/admin   (ctrl-c to stop)");
        for addr in lan_addresses() {
            println!("  the operators: http://{addr}:{port}/");
        }
        println!();
        // Ctrl-C *and* a plain kill. Only handling ctrl-c is how a show server
        // outlived its parent and sat on a port for half an hour serving stale
        // code — `pkill` on the parent left the child running, and nothing was
        // left that knew about it.
        let (shut_tx, shut_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            let _ = shut_tx.send(true);
        });
        let signalled = {
            let mut rx = shut_rx.clone();
            async move {
                let _ = rx.wait_for(|v| *v).await;
            }
        };
        // With a deadline on the drain, for the same reason the show server has one:
        // a graceful shutdown waits for connections in flight, and the app's window
        // keeps one alive. Waiting for a page that is not going to close first is how
        // "quit" becomes "kill".
        let deadline = {
            let mut rx = shut_rx;
            async move {
                let _ = rx.wait_for(|v| *v).await;
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        };
        tokio::select! {
            r = axum::serve(listener, app).with_graceful_shutdown(signalled) => r?,
            _ = deadline => {}
        }
        // A show server started from here is this process's child, and leaving it
        // running after its parent has gone is a port nobody can find again.
        if let Some(r) = ui.show.lock().unwrap().take() {
            stop_child(r);
        }
        Ok::<(), anyhow::Error>(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_read_as_a_date_and_a_time() {
        assert_eq!(readable("2026-08-16T21-04-11"), "2026-08-16 21:04");
        // Two snapshots in one second: the suffix is for uniqueness, not for reading.
        assert_eq!(readable("2026-08-16T21-04-11-2"), "2026-08-16 21:04");
        assert_eq!(readable("nonsense"), "nonsense");
        assert_eq!(readable("2026-08-16T"), "2026-08-16T");
    }

    #[test]
    fn a_redirect_keeps_the_address_the_operator_typed() {
        // The tablet reached this machine by one of several names. Whichever it used
        // has to survive into the redirect: `localhost` would send it to itself.
        assert_eq!(without_port("192.168.1.40:8080"), "192.168.1.40");
        assert_eq!(without_port("choufleur.local:8080"), "choufleur.local");
        // No port at all — the default, and nothing to strip.
        assert_eq!(without_port("choufleur.local"), "choufleur.local");
        // Splitting on the last colon would have eaten half of this.
        assert_eq!(without_port("[::1]:8080"), "[::1]");
        assert_eq!(without_port("[::1]"), "[::1]");
        // Not a port.
        assert_eq!(without_port("host:"), "host:");
    }

    #[test]
    fn a_sheet_id_survives_being_put_in_a_url() {
        // What ids actually look like: file stems.
        assert_eq!(urlencode("cues-conduite-lumiere"), "cues-conduite-lumiere");
        // The stage manager's everything-at-once view, which must not be escaped into
        // something the page does not recognise.
        assert_eq!(urlencode("*"), "*");
        // And the day somebody names a cue file with a space or an accent.
        assert_eq!(urlencode("conduite son"), "conduite%20son");
        assert_eq!(urlencode("lumière"), "lumi%C3%A8re");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }
}
