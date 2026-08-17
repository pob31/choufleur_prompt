//! What happens to a server when whoever started it goes away.
//!
//! Every case here is one that has actually gone wrong in the field: a show server
//! that outlived its parent and kept a port for half an hour, answering for a show
//! nobody had opened. The rules being checked are that a server asked to stop stops,
//! and that a server whose supervisor dies — by any means, including the ones that
//! leave nothing behind to ask — stops as well.
//!
//! Runs the real binary rather than the library, because the thing under test is
//! process behaviour: signals, pipes, and exit. No models needed; `--prep` and the
//! Shows screen both run without them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_choufleur-replay");

/// Wait for a process to exit, up to `secs`. `None` means it was still running.
fn wait_for_exit(child: &mut Child, secs: u64) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return None,
        }
    }
    None
}

/// Wait until something is listening on `port`, up to ten seconds.
fn wait_until_listening(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// A port nothing is on, released again before the caller uses it.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a free port");
    l.local_addr().unwrap().port()
}

/// Start a server the way a Mac GUI app starts one.
///
/// The difference is not cosmetic, and finding that out cost a whole diagnosis. A
/// child inherits its parent's signal dispositions and mask, and AppKit takes SIGTERM
/// over for itself — so a server spawned by the desktop shell begins life with SIGTERM
/// ignored, blocked, or both. The first version of the watchdog called `raise`, which
/// delivers to the calling thread; under a plain parent that worked and every test
/// passed, and under the app the signal went nowhere and the server kept its port.
///
/// So the tests spawn it hostile: SIGTERM ignored *and* blocked before exec. Anything
/// that only works with a friendly parent fails here.
///
/// The output pipes matter too, and that half is the one that actually bit. A
/// supervisor that dies takes the read ends of the child's stdout and stderr with it,
/// so everything the child prints from then on fails — and `println!` and `eprintln!`
/// *panic* when a write fails. The watchdog announced itself, panicked on the
/// announcement, and died before it could stop anything, leaving the very orphan it
/// exists to prevent. Sending the output to `/dev/null`, as the first version of these
/// tests did, is a pipe that never breaks, and it hid the bug completely.
///
/// So a supervised server here is started with all three pipes held, and
/// [`Supervised::supervisor_dies`] drops all three at once — which is what dying is,
/// from the far side of a pipe.
struct Supervised {
    child: Child,
    // Held, not read: the point is that they are open until the moment they are not.
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
}

impl Supervised {
    fn spawn(cmd: &mut Command) -> Self {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGTERM);
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
                Ok(())
            });
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("starting a server");
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        Supervised {
            child,
            stdout,
            stderr,
        }
    }

    /// Everything a supervisor holds, let go of at once.
    fn supervisor_dies(&mut self) {
        drop(self.child.stdin.take());
        drop(self.stdout.take());
        drop(self.stderr.take());
    }

    fn pid(&self) -> libc::pid_t {
        self.child.id() as libc::pid_t
    }
}

/// A library with one empty show in it, and the path to its manifest.
fn a_show(dir: &Path) -> PathBuf {
    let status = Command::new(BIN)
        .args(["show", "new", "Lifecycle"])
        .arg("--library")
        .arg(dir)
        .stdout(Stdio::null())
        .status()
        .expect("running show new");
    assert!(status.success(), "show new failed");
    dir.join("Lifecycle/manifest.json")
}

#[test]
fn a_supervised_server_stops_when_its_supervisor_closes_the_pipe() {
    let tmp = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut s = Supervised::spawn(
        Command::new(BIN)
            .args(["ui", "--port", &port.to_string()])
            .arg("--library")
            .arg(tmp.path())
            .env("CHOUFLEUR_SUPERVISED", "1"),
    );

    assert!(
        wait_until_listening(port),
        "the Shows server never came up on {port}"
    );

    // Not a message — the pipes closing *are* the message. This stands in for the
    // supervisor being killed outright, which is the case no signal survives.
    s.supervisor_dies();

    // Generous, because with SIGTERM ignored the way out is the watchdog's own
    // deadline rather than a clean shutdown. Late is the point; never is the bug.
    let status = wait_for_exit(&mut s.child, 15);
    if status.is_none() {
        let _ = s.child.kill();
    }
    assert!(
        status.is_some(),
        "the server outlived the supervisor whose pipes it held"
    );
}

#[test]
fn an_unsupervised_server_ignores_its_stdin() {
    // The watchdog is a contract about stdin, and a server started from a terminal
    // has not signed it: closing the terminal's stdin must not be read as a death.
    let tmp = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = Command::new(BIN)
        .args(["ui", "--port", &port.to_string()])
        .arg("--library")
        .arg(tmp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("starting the Shows server");

    assert!(wait_until_listening(port), "the Shows server never came up");
    drop(child.stdin.take());
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "an unsupervised server stopped when its stdin closed"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_show_server_stops_when_asked_with_sigterm() {
    // Ctrl-c used to be the only thing this listened for, so a `kill` left it
    // running: the orphan that answered for every show.
    let tmp = tempfile::tempdir().unwrap();
    let manifest = a_show(tmp.path());
    let port = free_port();
    let mut child = Command::new(BIN)
        .arg("serve")
        .arg(&manifest)
        .args(["--prep", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("starting a show server");

    assert!(wait_until_listening(port), "the show server never came up");

    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }

    let status = wait_for_exit(&mut child, 8);
    if status.is_none() {
        let _ = child.kill();
    }
    assert!(status.is_some(), "the show server ignored SIGTERM");
}

#[test]
fn a_show_server_stops_with_a_page_still_connected() {
    // A graceful shutdown waits for connections in flight, and an operator's page
    // holds its socket open for the whole show — so without a deadline on the drain,
    // "ask nicely" never returns and every quit ends in a kill.
    let tmp = tempfile::tempdir().unwrap();
    let manifest = a_show(tmp.path());
    let port = free_port();
    let mut child = Command::new(BIN)
        .arg("serve")
        .arg(&manifest)
        .args(["--prep", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("starting a show server");

    assert!(wait_until_listening(port), "the show server never came up");

    // A hand-written upgrade is enough: the server only has to believe a socket is
    // open, which is all a tablet on the far side of a venue is doing either way.
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connecting");
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    write!(
        sock,
        "GET /ws HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\r\n"
    )
    .expect("sending the upgrade");
    sock.flush().ok();
    std::thread::sleep(Duration::from_millis(300));

    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }

    let status = wait_for_exit(&mut child, 8);
    if status.is_none() {
        let _ = child.kill();
    }
    drop(sock);
    assert!(
        status.is_some(),
        "an open page held the shutdown open indefinitely"
    );
}

#[test]
fn killing_the_shows_server_takes_its_show_server_with_it() {
    // The whole point. A parent killed outright cannot tidy up, so the child has to
    // notice by itself — which it does through the pipe, not through a signal.
    let tmp = tempfile::tempdir().unwrap();
    let manifest = a_show(tmp.path());
    let ui_port = free_port();
    let mut ui = Supervised::spawn(
        Command::new(BIN)
            .args(["ui", "--port", &ui_port.to_string()])
            .arg("--library")
            .arg(tmp.path())
            .env("CHOUFLEUR_SUPERVISED", "1"),
    );
    assert!(wait_until_listening(ui_port), "the Shows server never came up");
    assert!(manifest.exists(), "the show was not created");

    // Open the show the way the screen does, and find out where it landed.
    let out = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            r#"{"prep":true}"#,
            &format!("http://127.0.0.1:{ui_port}/api/shows/Lifecycle/open"),
        ])
        .output()
        .expect("asking the Shows server to open a show");
    let body = String::from_utf8_lossy(&out.stdout);
    let reply: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|_| panic!("open replied {body}"));
    let show_port = reply["port"].as_u64().expect("a port in the reply") as u16;
    assert!(wait_until_listening(show_port), "the show server never came up");

    // Not SIGTERM: the case that used to leave an orphan is the one where nothing
    // gets to run any cleanup at all. The pipes go with it, exactly as they would if
    // the app had been force-quit — including the ones the show server writes to,
    // which it inherited from the server being killed here.
    unsafe {
        libc::kill(ui.pid(), libc::SIGKILL);
    }
    let _ = ui.child.wait();
    ui.supervisor_dies();

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut gone = false;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", show_port)).is_err() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        gone,
        "the show server on {show_port} outlived the process that started it"
    );
}
