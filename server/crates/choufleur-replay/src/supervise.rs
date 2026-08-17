//! Dying with whoever started us.
//!
//! A signal is a request, and a request needs somebody left to make it. When the
//! parent is killed outright — force-quit, crash, `kill -9` — nothing is left to ask
//! its children to stop, and a show server carries on holding a port and serving a
//! stale script. That has happened twice on this project, most memorably as every
//! show opening as Hécube: an orphan answered before the server that had just been
//! started could.
//!
//! A pipe outlives the process that held it. If the parent keeps the write end of our
//! stdin and never writes to it, then the read end goes to EOF at exactly one moment:
//! when the parent is gone, however it went. So a thread sits on that read, and turns
//! the EOF into a SIGTERM to ourselves — the same shutdown the operator's ctrl-c
//! takes, rather than a second path that would need testing separately.
//!
//! Opt-in through `CHOUFLEUR_SUPERVISED`, because it is a contract about stdin:
//! **whoever sets the variable must give the child a pipe for stdin.** Armed against
//! an inherited terminal instead, a backgrounded job would read from the tty and be
//! stopped with SIGTTIN. That is why this is not simply always on.
//!
//! Three details here are not decoration, and all three were paid for by the same
//! bug. The signal is sent to the *process* rather than raised on this thread; there
//! is a deadline after it; and nothing on this path may panic — including printing.
//!
//! That last one is the subtle one. The supervisor going away usually takes our
//! stdout and stderr with it, because they were its pipes too. A write to a pipe with
//! no reader fails, and `println!` and `eprintln!` *panic* when a write fails. So the
//! first version announced itself, panicked on the announcement, and the thread died
//! before it could do the one thing it exists to do — leaving exactly the orphaned
//! server it was written to prevent, in exactly the case it was written for. Under a
//! parent that redirected the pipes elsewhere it worked perfectly, which is why the
//! tests were happy and the app was not.

/// Watch stdin for the parent's death, if we were started under supervision.
///
/// Called at the top of the long-running subcommands. Cheap and silent when the
/// variable is unset, which is every run from a terminal.
pub fn arm() {
    if std::env::var_os("CHOUFLEUR_SUPERVISED").is_none() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("choufleur-supervise".into())
        .spawn(|| {
            use std::io::Read;
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 64];
            loop {
                match stdin.read(&mut buf) {
                    // EOF: the far end of the pipe is closed, so the parent is gone.
                    Ok(0) => break,
                    // A supervisor is not expected to write anything, but reading it
                    // and carrying on is kinder than treating it as a shutdown.
                    Ok(_) => {}
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    // A broken or unreadable stdin is the same news as EOF.
                    Err(_) => break,
                }
            }
            // Said, if anyone is still there to hear it, and never depended on.
            say("supervisor gone — shutting down");
            // To the process, not to this thread.
            //
            // `raise` sends the signal to whichever thread calls it, and a thread that
            // inherited SIGTERM blocked or ignored — which is what a Mac GUI parent
            // hands its children, because AppKit takes SIGTERM over for itself —
            // swallows it in silence. That is not a theory: under the desktop shell
            // this thread reached exactly here, the signal went nowhere, and the
            // server kept serving on its port for as long as it was left alone, which
            // is the whole bug this file exists to prevent. Sent to the process, the
            // kernel gives it to any thread that will have it.
            unsafe {
                libc::kill(std::process::id() as libc::pid_t, libc::SIGTERM);
            }
            // And if even that is refused, leave anyway.
            //
            // Long enough for an honest shutdown — a show server drains for three
            // seconds and then gives its engine five to come out of a decode — and
            // then it stops being a shutdown and starts being an orphan. Outliving
            // the supervisor is the one outcome that is not allowed. Late is fine.
            std::thread::sleep(std::time::Duration::from_secs(10));
            say("shutting down took too long — leaving now");
            unsafe { libc::_exit(0) }
        });
    if spawned.is_err() {
        say("could not watch for the supervisor; this process will outlive it");
    }
}

/// Print a line to stderr, or do not. Never panic over it.
///
/// `eprintln!` panics when the write fails, and on this path the write failing is the
/// expected case rather than the surprising one — the reader was our supervisor, and
/// the supervisor is why we are here. Anything else that runs while shutting down
/// should say things this way for the same reason.
pub fn say(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{line}");
}
