//! Running cargo inside a session: spawning it, forwarding signals,
//! watching the daemon and exiting as cargo did (spec §5.3 step 3, §7.1).

use crate::fallback::{record_fallback, self_feed};
use crate::session::{HEARTBEAT_TIMEOUT, Opened, open_session, session_env};
use damper_core::classify::Invocation;
use damper_proto::paths::Paths;
use damper_proto::{Client, ClientMsg, ProtoError};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use signal_hook::low_level::emulate_default_handler;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::{Child, Command, ExitCode, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

/// The signals forwarded to cargo once it runs. Before that, each one ends
/// the shim as that signal would.
const FORWARDED: [i32; 4] = [SIGINT, SIGTERM, SIGHUP, SIGQUIT];

/// What `main` does once the session path is done.
pub enum Outcome {
    /// Cargo ran in a session; exit with this code.
    Exit(ExitCode),
    /// No session: exec this command (the real cargo, at its priority).
    Exec(Command),
}

/// Where cargo is in its life, shared with the signal thread.
#[derive(Clone, Copy)]
enum Phase {
    /// Not started: a signal ends the shim as that signal.
    Waiting,
    /// Running: signals go to cargo.
    Running(Pid),
    /// Exited and reaped: the shim is about to exit as cargo did.
    Done,
}

type SharedPhase = Arc<Mutex<Phase>>;

/// Runs one invocation in a session. `cargo` builds the command that runs
/// the real cargo at its priority, with no session.
///
/// Every failure before cargo starts returns [`Outcome::Exec`] with a plain
/// command after recording why. Once cargo has started, nothing re-runs it.
pub fn run(paths: &Paths, inv: &Invocation, cargo: impl Fn() -> Command) -> Outcome {
    let cwd = std::env::current_dir().unwrap_or_default();
    let phase: SharedPhase = Arc::new(Mutex::new(Phase::Waiting));
    if let Err(e) = watch_signals(Arc::clone(&phase)) {
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
    let mut command = cargo();
    command.envs(session_env(&fifo, &session, inv, threads));
    let mut child = match spawn(&mut command, &phase) {
        Ok(child) => child,
        Err(e) => {
            record_fallback(paths, &format!("cannot start cargo: {}", e.kind()), &cwd);
            return Outcome::Exec(cargo());
        }
    };
    // Cargo is running: from here a panic must never run it again, so the
    // shim falls back to waiting for it.
    let status = catch_unwind(AssertUnwindSafe(|| supervise(&mut child, client, &fifo)))
        .unwrap_or_else(|_| child.wait());
    *lock(&phase) = Phase::Done;
    Outcome::Exit(status.map_or(ExitCode::FAILURE, exit_as))
}

/// Tells the daemon cargo's pid, watches the daemon, and waits for cargo.
/// Unlinks the FIFO afterwards if the shim fed it itself.
fn supervise(child: &mut Child, mut client: Client, fifo: &Path) -> io::Result<ExitStatus> {
    // A failed send poisons the client, so the watcher then self-feeds.
    let _ = client.send(&ClientMsg::Started {
        cargo_pid: child.id(),
    });
    let self_fed = Arc::new(AtomicBool::new(false));
    let watched = {
        let (fifo, self_fed) = (fifo.to_path_buf(), Arc::clone(&self_fed));
        thread::Builder::new()
            .name("damper-daemon-watch".to_owned())
            .spawn(move || watch_daemon(client, &fifo, &self_fed))
    };
    if watched.is_err() {
        // Without a watcher nobody notices a dead daemon, so feed now.
        let _ = self_feed(fifo, spare_cores());
        self_fed.store(true, Ordering::SeqCst);
    }
    let status = child.wait();
    if self_fed.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(fifo);
    }
    status
}

/// Spawns cargo as the shim's direct child, under the phase lock so a
/// signal arriving meanwhile is forwarded rather than ending the shim.
fn spawn(command: &mut Command, phase: &SharedPhase) -> io::Result<Child> {
    let mut guard = lock(phase);
    let child = command.spawn()?;
    let pid = i32::try_from(child.id()).map_err(io::Error::other)?;
    *guard = Phase::Running(Pid::from_raw(pid));
    Ok(child)
}

fn watch_signals(phase: SharedPhase) -> io::Result<()> {
    let mut signals = Signals::new(FORWARDED)?;
    thread::Builder::new()
        .name("damper-signals".to_owned())
        .spawn(move || {
            for sig in signals.forever() {
                on_signal(&phase, sig);
            }
        })?;
    Ok(())
}

fn on_signal(phase: &SharedPhase, sig: i32) {
    let guard = lock(phase);
    match *guard {
        // Before cargo starts the shim dies as the signal would kill it;
        // the daemon takes the session's tokens back when the socket closes.
        Phase::Waiting => {
            let _ = emulate_default_handler(sig);
        }
        Phase::Running(pid) => {
            if let Ok(signal) = Signal::try_from(sig) {
                let _ = kill(pid, signal);
            }
        }
        Phase::Done => {}
    }
}

/// Reads the daemon's messages until it closes or stays silent for
/// `HEARTBEAT_TIMEOUT`, then feeds the FIFO once so cargo finishes at full
/// speed. Malformed or other-version messages are still signs of life.
fn watch_daemon(mut client: Client, fifo: &Path, self_fed: &AtomicBool) {
    while let Ok(_) | Err(ProtoError::Json(_) | ProtoError::Version { .. }) =
        client.recv(Some(HEARTBEAT_TIMEOUT))
    {}
    let _ = self_feed(fifo, spare_cores());
    self_fed.store(true, Ordering::SeqCst);
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

fn lock(phase: &SharedPhase) -> MutexGuard<'_, Phase> {
    phase
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
