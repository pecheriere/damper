//! A stand-in for cargo in damper's end-to-end tests.
//!
//! Invoked as `cargo <subcommand> [mode]`: the subcommand is what the shim
//! classifies and is otherwise ignored. Modes:
//!
//! - none: exit 0 (a passthrough such as `cargo metadata`).
//! - `exit <code>`: exit with that code.
//! - `jobs <n> <ms> [--report <path>] [--wait <path>]`: run `n` jobs of
//!   `<ms>` each through the jobserver FIFO in `CARGO_MAKEFLAGS`, like cargo:
//!   one implicit job without a token, one more per token taken. While the
//!   jobs run, a child process named `rustc` stays alive, so the daemon sees a
//!   compiling session (ruling R6). With `--wait`, the jobs start only once
//!   that file exists (the `rustc` child already runs while it waits). Every
//!   token goes back to the FIFO before the process exits. With `--report`,
//!   it writes the concurrency timeline there (see [`Timeline::render`]).
//!
//! Invoked through a hard link named `rustc`, it is that child: it waits for its
//! stdin to close, so it dies with its parent, even one killed by `SIGKILL`.
//!
//! Token reads poll the FIFO (`poll(2)` with a short timeout, then a
//! non-blocking read) instead of blocking in `read(2)`: the dispatcher must
//! stop reading once every job is claimed, and a read blocked forever can't
//! be cancelled without `unsafe`. Seen from the daemon it is the same: the
//! tokens are taken as soon as they arrive.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

/// How long one poll for a token waits before the dispatcher looks again.
const TOKEN_POLL_MS: u8 = 20;
/// How often `--wait` checks for its file.
const WAIT_POLL: Duration = Duration::from_millis(10);

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let invoked_as = args.first().map(PathBuf::from).unwrap_or_default();
    if invoked_as.file_name().is_some_and(|n| n == "rustc") {
        return rustc_child();
    }
    match run(&invoked_as, args.get(2..).unwrap_or_default()) {
        Ok(code) => code,
        Err(e) => {
            // Nothing else can be done if stderr is gone too.
            let _ = writeln!(io::stderr().lock(), "fake-cargo: {e}");
            ExitCode::from(2)
        }
    }
}

/// The `rustc` child: lives until its stdin closes.
fn rustc_child() -> ExitCode {
    let mut sink = Vec::new();
    let _ = io::stdin().lock().read_to_end(&mut sink);
    ExitCode::SUCCESS
}

fn run(invoked_as: &Path, mode: &[OsString]) -> Result<ExitCode, String> {
    let words: Vec<&str> = mode
        .iter()
        .map(|a| a.to_str().ok_or("non-UTF-8 argument"))
        .collect::<Result<_, _>>()?;
    match words.as_slice() {
        [] => Ok(ExitCode::SUCCESS),
        ["exit", code] => Ok(ExitCode::from(
            code.parse::<u8>().map_err(|e| format!("exit code: {e}"))?,
        )),
        ["jobs", n, ms, rest @ ..] => {
            let jobs = Jobs {
                count: n.parse().map_err(|e| format!("job count: {e}"))?,
                each: Duration::from_millis(ms.parse().map_err(|e| format!("job time: {e}"))?),
                report: flag(rest, "--report")?,
                wait: flag(rest, "--wait")?,
            };
            jobs.run(invoked_as)?;
            Ok(ExitCode::SUCCESS)
        }
        _ => Err("unknown mode".to_owned()),
    }
}

/// The value after `name` in `rest`, if `name` is there.
fn flag(rest: &[&str], name: &str) -> Result<Option<PathBuf>, String> {
    match rest.iter().position(|a| *a == name) {
        None => Ok(None),
        Some(i) => rest
            .get(i + 1)
            .map(|v| Some(PathBuf::from(v)))
            .ok_or_else(|| format!("{name} needs a value")),
    }
}

struct Jobs {
    count: u32,
    each: Duration,
    report: Option<PathBuf>,
    wait: Option<PathBuf>,
}

impl Jobs {
    fn run(&self, invoked_as: &Path) -> Result<(), String> {
        let rustc = spawn_rustc(invoked_as)?;
        let fifo = open_jobserver()?;
        if let Some(path) = &self.wait {
            while !path.exists() {
                thread::sleep(WAIT_POLL);
            }
        }
        let state = Arc::new(Mutex::new(State::new(self.count)));
        let mut workers = vec![self.implicit_worker(&state)];
        if let Some(fifo) = &fifo {
            self.dispatch(fifo, &state, &mut workers)?;
        }
        for w in workers {
            w.join().map_err(|_| "a job thread panicked")??;
        }
        if let Some(path) = &self.report {
            let text = lock(&state).timeline.render();
            write_atomically(path, &text).map_err(|e| format!("writing the report: {e}"))?;
        }
        stop_rustc(rustc)
    }

    /// Runs jobs one after another in the implicit slot until none is left.
    fn implicit_worker(&self, state: &Arc<Mutex<State>>) -> thread::JoinHandle<Result<(), String>> {
        let state = Arc::clone(state);
        let each = self.each;
        thread::spawn(move || {
            while lock(&state).claim() {
                thread::sleep(each);
                lock(&state).finish();
            }
            Ok(())
        })
    }

    /// Takes tokens while jobs are unclaimed; each token runs one job and
    /// goes back to the FIFO when it ends.
    fn dispatch(
        &self,
        fifo: &File,
        state: &Arc<Mutex<State>>,
        workers: &mut Vec<thread::JoinHandle<Result<(), String>>>,
    ) -> Result<(), String> {
        while lock(state).unclaimed > 0 {
            let Some(token) = take_token(fifo)? else {
                continue;
            };
            let fifo = fifo
                .try_clone()
                .map_err(|e| format!("cloning the FIFO: {e}"))?;
            if !lock(state).claim() {
                return give_back(&fifo, token);
            }
            let state = Arc::clone(state);
            let each = self.each;
            workers.push(thread::spawn(move || {
                thread::sleep(each);
                lock(&state).finish();
                give_back(&fifo, token)
            }));
        }
        Ok(())
    }
}

/// The job accounting shared by the workers.
struct State {
    unclaimed: u32,
    running: u32,
    timeline: Timeline,
}

impl State {
    fn new(count: u32) -> Self {
        let mut timeline = Timeline::default();
        timeline.lines.push(format!("start {}", now_ms()));
        Self {
            unclaimed: count,
            running: 0,
            timeline,
        }
    }

    /// Claims a job; false when none is left.
    fn claim(&mut self) -> bool {
        if self.unclaimed == 0 {
            return false;
        }
        self.unclaimed -= 1;
        self.running += 1;
        self.timeline.concurrency(self.running);
        if self.unclaimed == 0 {
            self.timeline.lines.push(format!("claimed {}", now_ms()));
        }
        true
    }

    fn finish(&mut self) {
        self.running -= 1;
        self.timeline.concurrency(self.running);
    }
}

/// The report: one event per line, times in milliseconds since the epoch.
#[derive(Default)]
struct Timeline {
    lines: Vec<String>,
    peak: u32,
}

impl Timeline {
    fn concurrency(&mut self, running: u32) {
        self.peak = self.peak.max(running);
        self.lines.push(format!("c {} {running}", now_ms()));
    }

    /// `start <t>` when the jobs start, `c <t> <n>` at each change in the
    /// number of running jobs, `claimed <t>` once every job is claimed, then
    /// `peak <n>` and `end <t>`.
    fn render(&self) -> String {
        let lines = self.lines.join("\n");
        format!("{lines}\npeak {}\nend {}\n", self.peak, now_ms())
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Starts the `rustc` child through the `rustc` link next to the `cargo` this
/// process was run as, so the kernel names the process `rustc`.
fn spawn_rustc(invoked_as: &Path) -> Result<Child, String> {
    let link = invoked_as
        .parent()
        .ok_or("run without a directory")?
        .join("rustc");
    Command::new(&link)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("starting {}: {e}", link.display()))
}

fn stop_rustc(mut rustc: Child) -> Result<(), String> {
    drop(rustc.stdin.take());
    rustc
        .wait()
        .map(drop)
        .map_err(|e| format!("waiting for rustc: {e}"))
}

/// The jobserver FIFO named in `CARGO_MAKEFLAGS`, opened read-write and
/// non-blocking, or `None` without one (damper fell back: the implicit slot
/// alone runs the jobs).
fn open_jobserver() -> Result<Option<File>, String> {
    let Some(flags) = std::env::var_os("CARGO_MAKEFLAGS") else {
        return Ok(None);
    };
    let flags = flags.to_str().ok_or("CARGO_MAKEFLAGS isn't UTF-8")?;
    let Some(path) = flags
        .split(' ')
        .find_map(|w| w.strip_prefix("--jobserver-auth=fifo:"))
    else {
        return Ok(None);
    };
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(OFlag::O_NONBLOCK.bits())
        .open(path)
        .map(Some)
        .map_err(|e| format!("opening the jobserver FIFO: {e}"))
}

/// One token, or `None` if none arrived within [`TOKEN_POLL_MS`].
fn take_token(mut fifo: &File) -> Result<Option<u8>, String> {
    let mut fds = [PollFd::new(fifo.as_fd(), PollFlags::POLLIN)];
    match poll(&mut fds, PollTimeout::from(TOKEN_POLL_MS)) {
        Ok(0) | Err(nix::errno::Errno::EINTR) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err(format!("polling the FIFO: {e}")),
    }
    let mut byte = [0_u8];
    match fifo.read(&mut byte) {
        Ok(1) => Ok(Some(byte[0])),
        Ok(_) => Ok(None),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(format!("reading a token: {e}")),
    }
}

fn give_back(mut fifo: &File, token: u8) -> Result<(), String> {
    fifo.write_all(&[token])
        .map_err(|e| format!("returning a token: {e}"))
}

/// Writes `text` to `path` through a temporary file and a rename, so a reader
/// never sees half a report.
fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(tmp, path)
}
