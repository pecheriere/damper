//! The real binaries in a temporary state dir.
//!
//! Each test gets its own state dir under `/tmp` (short, to stay under the
//! 103-byte socket path limit; `/tmp` is sticky, so the daemon's ancestor
//! check passes), its own `damperd`, and a `PATH` of two dirs: the real
//! `damper-shim` linked as `cargo`, then `fake-cargo` linked as `cargo` and
//! hard-linked as `rustc`.
//!
//! `fake-cargo` comes from `CARGO_BIN_EXE_fake-cargo`. Cargo exposes that
//! variable only for the package's own binaries, so `damperd`, `damper-shim`
//! and `damper` are found next to it instead: cargo puts every final binary
//! of a profile in the same dir. `cargo nextest run --workspace` (the gate)
//! builds every workspace binary first; with `-p damper-e2e` alone they must
//! already be built and current: run `cargo build --workspace` first.

use damper_proto::{Event, EventKind, SessionId, SessionState, Status};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long any wait on observable state may take before the test fails.
pub const DEADLINE: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(50);

/// A workspace binary: cargo puts every final binary of a profile in one
/// dir, so it sits next to `fake-cargo`.
fn workspace_bin(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_BIN_EXE_fake-cargo"))
        .parent()
        .unwrap();
    let bin = dir.join(name);
    assert!(
        bin.is_file(),
        "{} isn't built: run `cargo build --workspace` (or `cargo nextest run --workspace`) first",
        bin.display()
    );
    bin
}

pub struct Env {
    // Dropped in declaration order: the daemon goes before the temp dir.
    daemon: Option<Daemon>,
    tmp: tempfile::TempDir,
    pub state: PathBuf,
    shim: PathBuf,
    path: OsString,
    builds: u32,
}

impl Env {
    pub fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("d")
            .tempdir_in("/tmp")
            .unwrap();
        let root = tmp.path();
        let state = root.join("s");
        // The daemon refuses a state dir that group or others can reach.
        fs::DirBuilder::new().mode(0o700).create(&state).unwrap();
        let shimdir = root.join("b");
        let fakebin = root.join("f");
        fs::create_dir(&shimdir).unwrap();
        fs::create_dir(&fakebin).unwrap();
        symlink(workspace_bin("damper-shim"), shimdir.join("cargo")).unwrap();
        let fake = env!("CARGO_BIN_EXE_fake-cargo");
        symlink(fake, fakebin.join("cargo")).unwrap();
        // The kernel names a process after the last path component exec
        // used, after symlinks: a symlink would name it `fake-cargo`. A hard
        // link keeps the name `rustc`, so the daemon sees a compiler; a copy
        // does too, when the temp dir is on another volume.
        let rustc = fakebin.join("rustc");
        if fs::hard_link(fake, &rustc).is_err() {
            fs::copy(fake, &rustc).unwrap();
        }
        let path = std::env::join_paths([&shimdir, &fakebin]).unwrap();
        Self {
            daemon: None,
            shim: shimdir.join("cargo"),
            tmp,
            state,
            path,
            builds: 0,
        }
    }

    /// A file path in the test's temp dir (outside the state dir).
    pub fn file(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    pub fn run_dir(&self) -> PathBuf {
        self.state.join("run")
    }

    /// Starts `damperd` and waits until `damper status` answers.
    pub fn start_daemon(&mut self) {
        assert!(self.daemon.is_none(), "a daemon is already running");
        let log = fs::File::create(self.file("damperd.err")).unwrap();
        let child = Command::new(workspace_bin("damperd"))
            .env_clear()
            .env("DAMPER_STATE_DIR", &self.state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .process_group(0)
            .spawn()
            .unwrap();
        self.daemon = Some(Daemon(child));
        wait_until("damperd to answer", || self.try_status());
    }

    /// Sends `sig` to the daemon and waits for it to exit.
    pub fn stop_daemon(&mut self, sig: Signal) {
        let mut daemon = self.daemon.take().unwrap();
        signal(daemon.0.id(), sig);
        daemon.0.wait().unwrap();
    }

    /// Starts the shim as `cargo <args>` in a process group of its own, with
    /// stdout discarded and stderr kept in a file.
    pub fn build(&mut self, args: &[&str]) -> Build {
        self.builds += 1;
        let stderr = self.file(&format!("build{}.err", self.builds));
        let child = Command::new(&self.shim)
            .env_clear()
            .env("PATH", &self.path)
            .env("DAMPER_STATE_DIR", &self.state)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&stderr).unwrap())
            .process_group(0)
            .spawn()
            .unwrap();
        Build {
            child,
            stderr,
            started: Instant::now(),
        }
    }

    fn damper(&self, args: &[&str]) -> Option<String> {
        let out = Command::new(workspace_bin("damper"))
            .env_clear()
            .env("DAMPER_STATE_DIR", &self.state)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap())
    }

    fn try_status(&self) -> Option<Status> {
        self.damper(&["status", "--json"])
            .map(|text| serde_json::from_str(&text).unwrap())
    }

    /// `damper status --json`.
    pub fn status(&self) -> Status {
        self.try_status().expect("damper status failed")
    }

    /// Every event in `damper log --json`.
    pub fn events(&self) -> Vec<Event> {
        let text = self.damper(&["log", "--json"]).expect("damper log failed");
        let mut doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["truncated"], false, "the log was truncated");
        serde_json::from_value(doc["events"].take()).unwrap()
    }

    /// The id of the session the shim with pid `shim_pid` opened.
    pub fn session_of(&self, shim_pid: u32) -> SessionId {
        wait_until("the session to open", || {
            self.events().into_iter().find_map(|e| match e.kind {
                EventKind::SessionOpened {
                    session,
                    shim_pid: pid,
                    ..
                } if pid == shim_pid => Some(session),
                _ => None,
            })
        })
    }

    /// The cargo pid the daemon recorded for `session`.
    pub fn cargo_pid(&self, session: &SessionId) -> u32 {
        wait_until("the session to start cargo", || {
            self.events().into_iter().find_map(|e| match e.kind {
                EventKind::SessionStarted {
                    session: s,
                    cargo_pid,
                } if &s == session => Some(cargo_pid),
                _ => None,
            })
        })
    }

    /// Waits until the daemon reports `session` compiling.
    pub fn wait_compiling(&self, session: &SessionId) {
        wait_until("the session to compile", || {
            self.status()
                .sessions
                .iter()
                .any(|s| &s.session == session && s.state == SessionState::Compiling)
                .then_some(())
        });
    }

    /// Waits for `session`'s `SessionClosed` and returns it.
    pub fn wait_closed(&self, session: &SessionId) -> Event {
        wait_until("the session to close", || {
            self.events().into_iter().find(
                |e| matches!(&e.kind, EventKind::SessionClosed { session: s, .. } if s == session),
            )
        })
    }

    /// The session FIFOs in the runtime dir.
    pub fn fifos(&self) -> Vec<PathBuf> {
        fs::read_dir(self.run_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "fifo"))
            .collect()
    }
}

/// The daemon, killed on drop if still running.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One shim run; its whole process group is killed on drop.
pub struct Build {
    child: Child,
    stderr: PathBuf,
    pub started: Instant,
}

impl Build {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether the shim is still running.
    pub fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Waits for the shim to exit, failing the test after [`DEADLINE`].
    pub fn wait(&mut self) -> ExitStatus {
        wait_until("the build to exit", || self.child.try_wait().unwrap())
    }

    pub fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr).unwrap()
    }
}

impl Drop for Build {
    fn drop(&mut self) {
        let group = Pid::from_raw(i32::try_from(self.child.id()).unwrap());
        let _ = killpg(group, Signal::SIGKILL);
        let _ = self.child.wait();
    }
}

/// Polls `f` until it returns `Some`, failing the test after [`DEADLINE`].
pub fn wait_until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(POLL);
    }
}

/// Sends `sig` to process `pid` alone.
pub fn signal(pid: u32, sig: Signal) {
    kill(Pid::from_raw(i32::try_from(pid).unwrap()), sig).unwrap();
}

/// Whether process `pid` exists (a zombie counts).
pub fn alive(pid: u32) -> bool {
    kill(Pid::from_raw(i32::try_from(pid).unwrap()), None).is_ok()
}

/// A `fake-cargo --report`: see its module docs for the format.
pub struct Report {
    pub start: u64,
    /// `(time, running jobs)` at each change.
    pub series: Vec<(u64, u32)>,
    pub claimed: u64,
    pub peak: u32,
    pub end: u64,
}

impl Report {
    /// Waits for the report at `path` and parses it.
    pub fn read(path: &Path) -> Self {
        let text = wait_until("the report", || fs::read_to_string(path).ok());
        let mut r = Report {
            start: 0,
            series: Vec::new(),
            claimed: 0,
            peak: 0,
            end: 0,
        };
        for line in text.lines() {
            let words: Vec<&str> = line.split(' ').collect();
            let n = |i: usize| words[i].parse::<u64>().unwrap();
            match words[0] {
                "start" => r.start = n(1),
                "c" => r.series.push((n(1), u32::try_from(n(2)).unwrap())),
                "claimed" => r.claimed = n(1),
                "peak" => r.peak = u32::try_from(n(1)).unwrap(),
                "end" => r.end = n(1),
                other => panic!("unknown report line {other}"),
            }
        }
        r
    }

    /// The most jobs running at once between `from` and `to` (inclusive),
    /// counting what was running at `from`.
    pub fn peak_between(&self, from: u64, to: u64) -> u32 {
        let at_start = self
            .series
            .iter()
            .take_while(|(t, _)| *t <= from)
            .last()
            .map_or(0, |(_, n)| *n);
        self.series
            .iter()
            .filter(|(t, _)| (from..=to).contains(t))
            .map(|(_, n)| *n)
            .fold(at_start, u32::max)
    }
}
