//! Running cargo inside a session: spawning it, forwarding signals,
//! watching the daemon and exiting as cargo did (spec §5.3 step 3, §7.1).

use crate::fallback::{open_fifo, record_fallback, self_feed};
use crate::ignored::ignored_signals;
use crate::session::{HEARTBEAT_TIMEOUT, Opened, open_session, session_env};
use crate::signals::{Phase, SharedPhase, lock, watch_signals};
use damper_core::classify::Invocation;
use damper_proto::paths::Paths;
use damper_proto::{Client, ClientMsg, ProtoError};
use nix::errno::Errno;
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
use nix::unistd::Pid;
use signal_hook::low_level::emulate_default_handler;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::process::ExitStatusExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Why the shim fed the FIFO itself once cargo was running.
const DAEMON_LOST: &str = "daemon closed or went silent mid-build; fed the FIFO itself";
/// Why the shim fed the FIFO itself when it could not watch the daemon.
const NO_WATCHER: &str = "cannot watch the daemon mid-build; fed the FIFO itself";

/// What `main` does once the session path is done.
pub enum Outcome {
    /// Cargo ran in a session; exit with this code.
    Exit(ExitCode),
    /// No session: exec this command (the real cargo, at its priority).
    Exec(Command),
}

/// Runs one invocation in a session. `cargo` builds the command that runs
/// the real cargo at its priority, with no session.
///
/// Every failure before cargo starts returns [`Outcome::Exec`] with a plain
/// command after recording why. Once cargo has started, nothing re-runs it.
pub fn run(paths: &Paths, inv: &Invocation, cargo: impl Fn() -> Command) -> Outcome {
    let cwd = std::env::current_dir().unwrap_or_default();
    let phase: SharedPhase = Arc::new(Mutex::new(Phase::Waiting));
    // Without the inherited ignores no handler can be installed safely.
    let watching = ignored_signals().and_then(|ignored| watch_signals(Arc::clone(&phase), ignored));
    if let Err(e) = watching {
        record_fallback(paths, &format!("cannot watch signals: {}", e.kind()), &cwd);
        return Outcome::Exec(cargo());
    }
    let mut stderr = io::stderr().lock();
    let (session, fifo, threads, client) = match open_session(paths, inv, &cwd, &mut stderr) {
        Opened::Granted {
            session,
            fifo,
            threads,
            client,
        } => (session, fifo, threads, client),
        Opened::Unavailable(reason) => {
            record_fallback(paths, &reason, &cwd);
            return Outcome::Exec(cargo());
        }
    };
    drop(stderr);
    let fed = match open_fifo(&fifo, &paths.run) {
        Ok(file) => Arc::new(Fifo {
            file,
            self_fed: AtomicBool::new(false),
            paths: paths.clone(),
            cwd: cwd.clone(),
        }),
        Err(e) => {
            record_fallback(paths, &format!("unusable session FIFO: {}", e.kind()), &cwd);
            return Outcome::Exec(cargo());
        }
    };
    let mut command = cargo();
    command.envs(session_env(&fifo, &session, inv, threads));
    let child = match spawn(&mut command, &phase) {
        Ok(child) => child,
        Err(e) => {
            record_fallback(paths, &format!("cannot start cargo: {}", e.kind()), &cwd);
            return Outcome::Exec(cargo());
        }
    };
    // Cargo is running: from here a panic must never run it again, so each
    // step's panic is caught and the shim goes on to wait for cargo.
    let pid = child.id();
    let _ = catch_unwind(AssertUnwindSafe(|| start_watching(pid, client, &fed)));
    let exited = catch_unwind(|| await_exit(pid)).unwrap_or(false);
    let status = reap(child, &phase, exited);
    if fed.self_fed.load(Ordering::SeqCst) {
        unlink_if_same(&fifo, &fed.file);
    }
    Outcome::Exit(status.map_or(ExitCode::FAILURE, exit_as))
}

/// Reaps cargo and ends the `Running` phase.
///
/// When `exited` (cargo has exited, unreaped), the phase becomes `Done`
/// first, so its pid can't be reused while the signal thread may still
/// forward to it. Otherwise the wait for its exit failed and cargo may still
/// be running: signals keep reaching it until it is reaped, accepting the
/// short window after the reap in this degraded path only.
fn reap(mut child: Child, phase: &SharedPhase, exited: bool) -> io::Result<ExitStatus> {
    if exited {
        *lock(phase) = Phase::Done;
        return child.wait();
    }
    let status = child.wait();
    *lock(phase) = Phase::Done;
    status
}

/// The session FIFO, held open by the shim until cargo exits.
struct Fifo {
    file: File,
    /// Set once the shim has fed the FIFO itself, so it unlinks it at exit.
    self_fed: AtomicBool,
    /// Where a self-feed is recorded.
    paths: Paths,
    /// The invocation's directory, for the record.
    cwd: PathBuf,
}

impl Fifo {
    /// Feeds the FIFO once with `tokens` and records why. The flag is set
    /// first, so cargo exiting mid-write can't leave the FIFO behind.
    fn feed(&self, tokens: u32, reason: &str) {
        self.self_fed.store(true, Ordering::SeqCst);
        record_fallback(&self.paths, reason, &self.cwd);
        let _ = self_feed(&self.file, tokens);
    }
}

/// Tells the daemon cargo's pid and starts watching the daemon.
fn start_watching(pid: u32, mut client: Client, fifo: &Arc<Fifo>) {
    // A failed send poisons the client, so the watcher then self-feeds.
    let _ = client.send(&ClientMsg::Started { cargo_pid: pid });
    let watched = {
        let fifo = Arc::clone(fifo);
        thread::Builder::new()
            .name("damper-daemon-watch".to_owned())
            .spawn(move || watch_daemon(client, &fifo, HEARTBEAT_TIMEOUT))
    };
    if watched.is_err() {
        // Without a watcher nobody notices a dead daemon, so feed now. The
        // client went with the closure, so the daemon is not told: its
        // `observe` adopts what it finds above `given`.
        fifo.feed(spare_cores(), NO_WATCHER);
    }
}

/// Spawns cargo as the shim's direct child, under the phase lock so a
/// signal arriving meanwhile is forwarded rather than ending the shim.
fn spawn(command: &mut Command, phase: &SharedPhase) -> io::Result<Child> {
    let mut guard = lock(phase);
    let child = command.spawn()?;
    // std's id is the kernel's pid_t reinterpreted as u32: the reverse is
    // exact and has no failure path that could lead to running cargo twice.
    let pid = i32::from_ne_bytes(child.id().to_ne_bytes());
    *guard = Phase::Running(Pid::from_raw(pid));
    Ok(child)
}

/// Blocks until the child `pid` exits, without reaping it. True once it has
/// exited (`ESRCH` included: it already had); false if the wait failed and
/// it may still be running.
fn await_exit(pid: u32) -> bool {
    wait_on(Kqueue::new(), pid)
}

/// [`await_exit`] on a given kqueue, so a failure can be injected.
fn wait_on(kq: nix::Result<Kqueue>, pid: u32) -> bool {
    let (Ok(ident), Ok(kq)) = (usize::try_from(pid), kq) else {
        return false;
    };
    let exit = KEvent::new(
        ident,
        EventFilter::EVFILT_PROC,
        EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
        FilterFlag::NOTE_EXIT,
        0,
        0,
    );
    match kq.kevent(&[exit], &mut [], None) {
        Ok(_) => {}
        Err(Errno::ESRCH) => return true,
        Err(_) => return false,
    }
    let mut events = [exit];
    loop {
        match kq.kevent(&[], &mut events, None) {
            Ok(0) | Err(Errno::EINTR) => {}
            Ok(_) => return true,
            Err(_) => return false,
        }
    }
}

/// Unlinks `path` only if it is still the FIFO the shim holds open.
fn unlink_if_same(path: &Path, held: &File) {
    let same = match (held.metadata(), fs::symlink_metadata(path)) {
        (Ok(held), Ok(found)) => {
            found.file_type().is_fifo() && held.dev() == found.dev() && held.ino() == found.ino()
        }
        _ => false,
    };
    if same {
        let _ = fs::remove_file(path);
    }
}

/// Reads the daemon's messages until it closes or stays silent for
/// `silence`, then feeds the FIFO once so cargo finishes at full speed.
/// Malformed or other-version messages are still signs of life.
///
/// Before feeding it tells the daemon how many tokens it adds, best effort:
/// a hung daemon reads the line when it recovers and counts them as given,
/// so it can drain them under pressure; a dead one fails the send.
fn watch_daemon(mut client: Client, fifo: &Fifo, silence: Duration) {
    while let Ok(_) | Err(ProtoError::Json(_) | ProtoError::Version { .. }) =
        client.recv(Some(silence))
    {}
    let tokens = spare_cores();
    let _ = client.send(&ClientMsg::SelfFed { tokens });
    drop(client);
    fifo.feed(tokens, DAEMON_LOST);
}

/// One token per core beyond cargo's implicit slot.
fn spare_cores() -> u32 {
    thread::available_parallelism()
        .map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .saturating_sub(1)
}

/// Exits with cargo's code, or dies by cargo's signal.
fn exit_as(status: ExitStatus) -> ExitCode {
    if let Some(code) = status.code() {
        return ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX));
    }
    match status.signal() {
        Some(sig) => {
            let _ = emulate_default_handler(sig);
            // Reached only for a signal whose default is to be ignored.
            ExitCode::from(u8::try_from(128 + sig).unwrap_or(u8::MAX))
        }
        None => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests;
