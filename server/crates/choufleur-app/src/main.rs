//! The desktop shell: a window for the library, and the process that owns the rest.
//!
//! Choufleur is a server and a browser page, and it works that way on purpose —
//! operators watch the show on their own tablets, over the venue's network, from
//! whatever they already have in their hands. This window changes none of that. What
//! it adds is the two things a server on its own cannot do.
//!
//! The first is the microphone. macOS gives a plain binary a stream that runs and
//! delivers nothing at all — no error, no prompt, nothing in System Settings to
//! allow — and only a signed bundle carrying `NSMicrophoneUsageDescription` is ever
//! asked about. Live capture does not work without this app; that is why it exists.
//!
//! The second is an ending. A server started from a terminal stops when somebody
//! stops it, and a machine at the back of a theatre is not somewhere anybody goes to
//! type. So this process holds the whole tree's lifetime: closing the window ends
//! everything, and if this process is killed outright, its children notice through
//! the pipes they hold and end themselves (see `choufleur-replay`'s `supervise`).
//!
//! It deliberately does *not* reimplement any of the server. It picks a port, starts
//! `choufleur-replay ui`, and points a webview at it. Everything the window shows is
//! served by the sidecar and is the same page a tablet gets.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_shell::process::CommandChild;
use tauri_plugin_shell::ShellExt;

mod loopback;

/// Where the library server is asked to listen first.
///
/// Operators bookmark a URL, and a port that moves is a bookmark that stops working
/// on the one night nobody has time to look it up. It moves only if something else
/// is already there.
const PREFERRED_PORT: u16 = 8080;

/// What the shell holds for as long as it runs.
struct Shell {
    /// The library server. Dropping this closes the pipe it reads, which is how it
    /// learns we are gone even when nothing gets to say so.
    child: Mutex<Option<CommandChild>>,
    port: u16,
    /// Set once a quit has been agreed to, so the window's close and the app's exit
    /// do not each ask the same question.
    quitting: AtomicBool,
}

impl Shell {
    /// Stop the library server, asking before insisting.
    ///
    /// It stops its own show server the same way, so this ends the whole tree. Both
    /// steps are best-effort: a server that has already died is the outcome we want.
    fn stop(&self) {
        let Some(child) = self.child.lock().unwrap().take() else {
            return;
        };
        let pid = child.pid() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        // Closing the pipe as well: it reaches the child even if the signal did not,
        // and it is the same shutdown either way.
        drop(child);
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            // `kill(pid, 0)` asks whether the process is still there without
            // disturbing it. The child is ours, so nothing else can reap it and
            // leave the number pointing at a stranger.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("the library server did not stop when asked; killing it");
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// The first port from `from` upward that nothing is listening on.
///
/// Bound and released rather than probed, because the question is whether we can
/// have it, not whether somebody answers on it.
fn free_port(from: u16) -> u16 {
    for port in from..from.saturating_add(40) {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    // Nothing free in the range anybody would look for. Let the OS choose, and the
    // Shows screen will say where it landed.
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(PREFERRED_PORT)
}

/// Poll until the library server answers.
fn wait_until_listening(port: u16, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Is a show running right now — listening to a room, or tracking a run?
///
/// Asked of the servers rather than remembered here, because the shell is not what
/// starts or stops a run, and anything it remembered would be a second opinion that
/// could disagree with the one that matters.
///
/// A show server that answers something unreadable counts as live: at that point the
/// only honest answer is that we do not know, and the cost of asking an unnecessary
/// question is nothing next to ending a performance without one. A show server that
/// cannot be reached at all is a different matter — it has already stopped, and
/// there is nothing left to interrupt.
fn a_show_is_live(port: u16) -> bool {
    let Some(state) = loopback::get_json(port, "/api/state") else {
        return false;
    };
    let Some(show_port) = state["open"]["port"].as_u64() else {
        return false; // nothing open
    };
    let show_port = show_port as u16;
    if !loopback::is_listening(show_port) {
        return false;
    }
    match loopback::get_json(show_port, "/run.json") {
        Some(run) => {
            run["capture"].as_bool().unwrap_or(false) || run["running"].as_bool().unwrap_or(false)
        }
        None => true,
    }
}

/// Ask, then quit — or leave everything exactly as it was.
///
/// Runs on its own thread: the dialog blocks, and blocking the main thread is how a
/// window stops redrawing. The one question worth interrupting for is the one where
/// the answer costs a performance.
fn quit_after_asking(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let shell = app.state::<Arc<Shell>>();
        if shell.quitting.load(Ordering::SeqCst) {
            return;
        }
        if a_show_is_live(shell.port) {
            let stay = app
                .dialog()
                .message(
                    "Tracking will stop, and every screen following the show will stop \
                     with it. Operators watching on their own devices will lose the \
                     position.",
                )
                .title("A show is running")
                .kind(MessageDialogKind::Warning)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "Keep running".into(),
                    "Quit anyway".into(),
                ))
                .blocking_show();
            if stay {
                return;
            }
        }
        shell.quitting.store(true, Ordering::SeqCst);
        shell.stop();
        app.exit(0);
    });
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // One library, one window. A second launch — from the Dock, from a DMG
            // still mounted — brings back the one already running rather than
            // starting a second server on a second port.
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let port = free_port(PREFERRED_PORT);

            // Piped stdin is the contract that comes with the variable: the child
            // watches it for EOF and stops when it arrives, which is what makes a
            // force-quit of this app take the servers with it. `tauri-plugin-shell`
            // pipes it, and `CommandChild` holds the far end for as long as we do.
            let (mut rx, child) = app
                .shell()
                .sidecar("choufleur-replay")?
                .args(["ui", "--port", &port.to_string()])
                .env("CHOUFLEUR_SUPERVISED", "1")
                .spawn()?;

            // The server's own words, on our stderr.
            //
            // Everything it has to say about a script that will not load, a port it
            // could not take, or a model it could not find is printed rather than
            // returned, and a sidecar's output goes to this channel instead of to a
            // terminal. Dropped, it is a server debugged by guesswork — which is
            // exactly how long an afternoon can get.
            tauri::async_runtime::spawn(async move {
                use tauri_plugin_shell::process::CommandEvent;
                while let Some(event) = rx.recv().await {
                    match event {
                        CommandEvent::Stdout(line) | CommandEvent::Stderr(line) => {
                            eprint!("{}", String::from_utf8_lossy(&line));
                        }
                        CommandEvent::Error(e) => eprintln!("server error: {e}"),
                        CommandEvent::Terminated(p) => {
                            eprintln!("the library server stopped ({p:?})");
                        }
                        _ => {}
                    }
                }
            });

            let shell = Arc::new(Shell {
                child: Mutex::new(Some(child)),
                port,
                quitting: AtomicBool::new(false),
            });
            app.manage(Arc::clone(&shell));

            // Injected at document start on every page this window loads, including
            // the show server's on its own port. It is how the pages tell "inside the
            // app" from "a tablet at the back of the stalls" — one window, so a show
            // opens in place rather than in a popup a webview would refuse to make,
            // and the way back to the library is a link rather than a bookmark.
            let flag = format!("window.__CHOUFLEUR_SHELL__ = {{ uiPort: {port} }};");
            let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Choufleur")
                .inner_size(1280.0, 860.0)
                .min_inner_size(760.0, 520.0)
                .initialization_script(&flag)
                .build()?;

            // The window is up showing the splash; the server needs a moment. Waiting
            // on another thread so the window is drawn and draggable meanwhile.
            let w = window.clone();
            std::thread::spawn(move || {
                if wait_until_listening(port, 20) {
                    // `/admin`, not `/`: the root is the operators' entry, the page a
                    // tablet lands on to pick a cue list. This window is the machine
                    // running the show, and what it wants is the library.
                    let url = format!("http://localhost:{port}/admin");
                    if let Ok(url) = url.parse() {
                        let _ = w.navigate(url);
                    }
                } else {
                    // The splash doubles as the error screen, because a window that
                    // says "Starting…" forever is the least useful failure there is.
                    let _ = w.eval(
                        "document.body.dataset.state='failed';\
                         document.getElementById('why').textContent=\
                         'It did not answer within twenty seconds. If a copy is already \
                          running, quit it and open Choufleur again.';",
                    );
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                if app.state::<Arc<Shell>>().quitting.load(Ordering::SeqCst) {
                    return;
                }
                // Closing the window is quitting the app: there is nothing left to
                // look at, and a server nobody can see is the thing this whole change
                // exists to prevent.
                api.prevent_close();
                quit_after_asking(app.clone());
            }
        })
        .build(tauri::generate_context!())
        .expect("starting Choufleur")
        .run(|app, event| {
            if let RunEvent::ExitRequested { api, .. } = event {
                if app.state::<Arc<Shell>>().quitting.load(Ordering::SeqCst) {
                    return;
                }
                api.prevent_exit();
                quit_after_asking(app.clone());
            }
        });
}
