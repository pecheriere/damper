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
//!   that file exists (the `rustc` child already runs while it waits), or
//!   fake-cargo fails after [`WAIT_LIMIT`]. Every token goes back to the FIFO
//!   before the process exits, on error paths too. With `--report`, it keeps
//!   the concurrency timeline there, rewritten at each change so a test can
//!   follow a running build (see [`Timeline::render`]).
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

/// How long one poll for a token waits before the dispatcher looks again.
const TOKEN_POLL_MS: u8 = 20;
/// How often `--wait` checks for its file.
const WAIT_POLL: Duration = Duration::from_millis(10);
/// How long `--wait` waits for its file before giving up, so a test that
/// never writes it can't leave fake-cargo running forever.
const WAIT_LIMIT: Duration = Duration::from_secs(60);

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
            wait_for(path)?;
        }
        let state = Arc::new(Mutex::new(State::new(self.count, self.report.clone())));
        let mut workers = vec![self.implicit_worker(&state)];
        // Whatever the dispatcher ran into, every job started is waited for,
        // so each token it holds goes back to the FIFO before fake-cargo exits.
        let dispatched = fifo
            .as_ref()
            .map_or(Ok(()), |fifo| self.dispatch(fifo, &state, &mut workers));
        let mut outcome = dispatched;
        for w in workers {
            let joined = w.join().map_err(|_| "a job thread panicked".to_owned());
            outcome = outcome.and(joined.and_then(|done| done));
        }
        outcome?;
        lock(&state).timeline.finish()?;
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
            let fifo = match fifo.try_clone() {
                Ok(clone) => clone,
                Err(e) => {
                    give_back(fifo, token)?;
                    return Err(format!("cloning the FIFO: {e}"));
                }
            };
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
    fn new(count: u32, report: Option<PathBuf>) -> Self {
        let mut timeline = Timeline {
            lines: Vec::new(),
            peak: 0,
            report,
            error: None,
        };
        timeline.push(format!("start {}", now_ms()));
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
            self.timeline.push(format!("claimed {}", now_ms()));
        }
        true
    }

    fn finish(&mut self) {
        self.running -= 1;
        self.timeline.concurrency(self.running);
    }
}

/// The report: one event per line, times in milliseconds since the epoch.
struct Timeline {
    lines: Vec<String>,
    peak: u32,
    /// Where `--report` keeps the timeline, if anywhere.
    report: Option<PathBuf>,
    /// The first failure to write the report, returned by [`Self::finish`].
    error: Option<String>,
}

impl Timeline {
    fn concurrency(&mut self, running: u32) {
        self.peak = self.peak.max(running);
        self.push(format!("c {} {running}", now_ms()));
    }

    /// Adds an event and rewrites the report with it.
    fn push(&mut self, line: String) {
        self.lines.push(line);
        self.write(None);
    }

    /// Writes the final report, with its `end` line.
    fn finish(&mut self) -> Result<(), String> {
        self.write(Some(now_ms()));
        self.error.take().map_or(Ok(()), Err)
    }

    fn write(&mut self, end: Option<u128>) {
        let Some(path) = &self.report else {
            return;
        };
        if self.error.is_some() {
            return;
        }
        if let Err(e) = write_atomically(path, &self.render(end)) {
            self.error = Some(format!("writing the report: {e}"));
        }
    }

    /// `start <t>` when the jobs start, `c <t> <n>` at each change in the
    /// number of running jobs, `claimed <t>` once every job is claimed, then
    /// `peak <n>` so far and, once every job is done, `end <t>`.
    fn render(&self, end: Option<u128>) -> String {
        let lines = self.lines.join("\n");
        let end = end.map(|t| format!("end {t}\n")).unwrap_or_default();
        format!("{lines}\npeak {}\n{end}", self.peak)
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

/// Waits for `path` to exist, for at most [`WAIT_LIMIT`].
fn wait_for(path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + WAIT_LIMIT;
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(format!("{} never appeared", path.display()));
        }
        thread::sleep(WAIT_POLL);
    }
    Ok(())
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
