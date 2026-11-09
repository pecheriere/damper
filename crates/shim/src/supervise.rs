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
use std::path::Path;
use std::process::{Child, Command, ExitCode, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
        }),
        Err(e) => {
            record_fallback(paths, &format!("unusable session FIFO: {}", e.kind()), &cwd);
            return Outcome::Exec(cargo());
        }
    };
    let mut command = cargo();
    command.envs(session_env(&fifo, &session, inv, threads));
    let mut child = match spawn(&mut command, &phase) {
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
    let _ = catch_unwind(|| await_exit(pid));
    // Cargo has exited but is not reaped, so its pid can't be reused while
    // the signal thread may still forward to it.
    *lock(&phase) = Phase::Done;
    let status = child.wait();
    if fed.self_fed.load(Ordering::SeqCst) {
        unlink_if_same(&fifo, &fed.file);
    }
    Outcome::Exit(status.map_or(ExitCode::FAILURE, exit_as))
}

/// The session FIFO, held open by the shim until cargo exits.
struct Fifo {
    file: File,
    /// Set once the shim has fed the FIFO itself, so it unlinks it at exit.
    self_fed: AtomicBool,
}

impl Fifo {
    /// Feeds the FIFO once with a token per spare core. The flag is set
    /// first, so cargo exiting mid-write can't leave the FIFO behind.
    fn feed(&self) {
        self.self_fed.store(true, Ordering::SeqCst);
        let _ = self_feed(&self.file, spare_cores());
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
        // Without a watcher nobody notices a dead daemon, so feed now.
        fifo.feed();
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

/// Blocks until the child `pid` exits, without reaping it. Returns early on
/// a kqueue error: `ESRCH` means it has already exited, and for any other
/// error the caller just reaps it, as it would without this wait.
fn await_exit(pid: u32) {
    let Ok(ident) = usize::try_from(pid) else {
        return;
    };
    let Ok(kq) = Kqueue::new() else {
        return;
    };
    let exit = KEvent::new(
        ident,
        EventFilter::EVFILT_PROC,
        EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
        FilterFlag::NOTE_EXIT,
        0,
        0,
    );
    if kq.kevent(&[exit], &mut [], None).is_err() {
        return;
    }
    let mut events = [exit];
    loop {
        match kq.kevent(&[], &mut events, None) {
            Ok(0) | Err(Errno::EINTR) => {}
            Ok(_) | Err(_) => return,
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
fn watch_daemon(mut client: Client, fifo: &Fifo, silence: Duration) {
    while let Ok(_) | Err(ProtoError::Json(_) | ProtoError::Version { .. }) =
        client.recv(Some(silence))
    {}
    fifo.feed();
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
#[expect(clippy::unwrap_used, reason = "test")]
mod tests {
    use super::{Fifo, await_exit, spare_cores, watch_daemon};
    use crate::fallback::open_fifo;
    use damper_proto::{Client, ServerMsg, encode};
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn a_silent_daemon_gets_the_fifo_fed_after_the_heartbeat_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let path = tmp.path().join("s.fifo");
        mkfifo(&path, Mode::from_bits_truncate(0o600)).unwrap();
        let fifo = Fifo {
            file: open_fifo(&path, tmp.path()).unwrap(),
            self_fed: AtomicBool::new(false),
        };
        let client = Client::connect(&socket, Duration::from_secs(5)).unwrap();
        let (mut daemon, _) = listener.accept().unwrap();
        // Alive at first: a heartbeat, then silence with the socket open.
        daemon
            .write_all(&encode(&ServerMsg::Heartbeat).unwrap())
            .unwrap();
        let started = Instant::now();
        watch_daemon(client, &fifo, Duration::from_millis(300));
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(fifo.self_fed.load(Ordering::SeqCst));
        let spare = usize::try_from(spare_cores()).unwrap();
        let mut reader = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut tokens = vec![0u8; spare];
        reader.read_exact(&mut tokens).unwrap();
        assert!(tokens.iter().all(|&t| t == b'+'), "{tokens:?}");
        drop(daemon);
    }

    #[test]
    fn await_exit_returns_for_a_child_that_already_exited() {
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id();
        // Long enough for it to exit and become an unreaped zombie.
        thread::sleep(Duration::from_millis(300));
        let (done, waited) = mpsc::channel();
        thread::spawn(move || {
            await_exit(pid);
            done.send(()).unwrap();
        });
        waited.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn await_exit_waits_for_a_running_child() {
        let mut child = Command::new("/bin/sleep").arg("0.3").spawn().unwrap();
        let started = Instant::now();
        await_exit(child.id());
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(child.wait().unwrap().success());
    }
}
