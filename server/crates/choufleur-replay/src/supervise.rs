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
            eprintln!("supervisor gone — shutting down");
            // Deliberately a signal to ourselves rather than a direct call into the
            // shutdown code: there is then one way this process stops, already tested
            // by every ctrl-c.
            unsafe {
                libc::raise(libc::SIGTERM);
            }
        });
    if spawned.is_err() {
        eprintln!("could not watch for the supervisor; this process will outlive it");
    }
}
