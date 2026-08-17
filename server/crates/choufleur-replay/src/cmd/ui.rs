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
    open: Option<OpenDto>,
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

async fn state(State(ui): State<Arc<Ui>>) -> Json<StateDto> {
    let open = ui.show.lock().unwrap().as_ref().map(|r| OpenDto {
        name: r.name.clone(),
        port: r.port,
    });
    Json(StateDto {
        root: ui.root.to_string_lossy().into_owned(),
        open,
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

    wait_until_listening(port, &e.name).await?;
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
        .route("/", asset_route!("shows.html", "text/html; charset=utf-8"))
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
        .with_state(Arc::clone(&ui));

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        println!("\n  open it at http://localhost:{port}   (ctrl-c to stop)\n");
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
}
