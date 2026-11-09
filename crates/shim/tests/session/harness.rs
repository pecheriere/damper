//! The built shim against a scripted fake daemon: shared harness.

use damper_proto::{ClientMsg, Event, ServerMsg, SessionId, decode, encode};
use nix::sys::signal::{Signal, kill};
use nix::sys::stat::Mode;
use nix::unistd::{Pid, mkfifo};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A fake cargo: `cargo build <mode> [args]`.
const FAKE_CARGO: &str = r#"#!/bin/sh
case "$2" in
env)
    printf 'flags=%s\n' "$CARGO_MAKEFLAGS"
    printf 'make=%s\n' "$MAKEFLAGS"
    printf 'session=%s\n' "$DAMPER_SESSION"
    printf 'rtt=%s\n' "${RUST_TEST_THREADS-unset}"
    exit 0 ;;
exit101) exit 101 ;;
sigkill) kill -9 $$ ;;
feed)
    fifo="${CARGO_MAKEFLAGS#*fifo:}"
    exec 3<>"$fifo"
    : > "$3"
    tokens=$(/bin/dd bs=1 count="$4" <&3 2>/dev/null)
    printf 'tokens=%s\n' "$tokens"
    exit 0 ;;
hang)
    : > "$3"
    exec /bin/sleep 30 ;;
waitfor)
    : > "$3"
    while [ ! -e "$4" ]; do /bin/sleep 0.02; done
    exit 0 ;;
count)
    trap 'echo int >> "$5"' INT
    : > "$3"
    while [ ! -e "$4" ]; do /bin/sleep 0.02; done
    exit 0 ;;
latefeed)
    while [ ! -e "$3" ]; do /bin/sleep 0.02; done
    /bin/sleep 0.3
    fifo="${CARGO_MAKEFLAGS#*fifo:}"
    exec 3<>"$fifo"
    tokens=$(/bin/dd bs=1 count="$4" <&3 2>/dev/null)
    printf 'tokens=%s\n' "$tokens"
    exit 0 ;;
esac
exit 2
"#;

pub struct Setup {
    _tmp: tempfile::TempDir,
    pub state: PathBuf,
    pub run: PathBuf,
    pub shim: PathBuf,
    pub path: std::ffi::OsString,
}

pub fn setup() -> (Setup, UnixListener) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let state = root.join("state");
    let run = state.join("run");
    fs::create_dir_all(&run).unwrap();
    fs::create_dir_all(state.join("log")).unwrap();
    let fakebin = root.join("fakebin");
    fs::create_dir_all(&fakebin).unwrap();
    let cargo = fakebin.join("cargo");
    fs::write(&cargo, FAKE_CARGO).unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    let shimdir = root.join("shimdir");
    fs::create_dir_all(&shimdir).unwrap();
    symlink(env!("CARGO_BIN_EXE_damper-shim"), shimdir.join("cargo")).unwrap();
    let listener = UnixListener::bind(run.join("damperd.sock")).unwrap();
    let path = std::env::join_paths([&shimdir, &fakebin]).unwrap();
    let s = Setup {
        _tmp: tmp,
        state,
        run,
        shim: shimdir.join("cargo"),
        path,
    };
    (s, listener)
}

impl Setup {
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.shim);
        cmd.env_clear()
            .env("PATH", &self.path)
            .env("DAMPER_STATE_DIR", &self.state)
            .args(args);
        cmd
    }

    pub fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    pub fn fallbacks(&self) -> Vec<Event> {
        let text = fs::read_to_string(self.state.join("log/fallbacks.jsonl")).unwrap_or_default();
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

/// One accepted connection, seen from the daemon's side.
pub struct Conn {
    reader: BufReader<UnixStream>,
}

impl Conn {
    /// The next message, or `None` once the shim has closed the socket.
    pub fn recv(&mut self) -> Option<ClientMsg> {
        let mut line = Vec::new();
        let n = self.reader.read_until(b'\n', &mut line).unwrap();
        (n > 0).then(|| decode(&line).unwrap())
    }

    pub fn send(&mut self, msg: &ServerMsg) {
        self.reader
            .get_mut()
            .write_all(&encode(msg).unwrap())
            .unwrap();
    }

    pub fn wait_for_close(&mut self) {
        while self.recv().is_some() {}
    }
}

pub fn daemon<T: Send + 'static>(
    listener: UnixListener,
    script: impl FnOnce(Conn) -> T + Send + 'static,
) -> JoinHandle<T> {
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        script(Conn {
            reader: BufReader::new(stream),
        })
    })
}

pub fn sid() -> SessionId {
    SessionId("s1".to_owned())
}

pub fn opened(fifo: &Path) -> ServerMsg {
    ServerMsg::Opened {
        session: sid(),
        fifo: fifo.to_path_buf(),
    }
}

pub const GRANTED: ServerMsg = ServerMsg::Granted {
    threads: 4,
    after_timeout: false,
};

/// Opens and grants a session, then waits for the shim to close.
/// Returns the `Open` and the `Started` messages.
pub fn grant_all(listener: UnixListener, fifo: PathBuf) -> JoinHandle<Vec<ClientMsg>> {
    make_fifo(&fifo);
    daemon(listener, move |mut c| {
        let open = c.recv().unwrap();
        c.send(&opened(&fifo));
        c.send(&GRANTED);
        let mut seen = vec![open];
        seen.extend(c.recv());
        c.wait_for_close();
        seen
    })
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

pub fn cores_minus_one() -> usize {
    thread::available_parallelism().map_or(1, std::num::NonZero::get) - 1
}

/// Makes the session FIFO, as the daemon does before `Opened`.
pub fn make_fifo(path: &Path) {
    mkfifo(path, Mode::from_bits_truncate(0o600)).unwrap();
}

/// Waits up to 10 s for `path` to exist; panics if it never does.
pub fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Sends `sig` to the process `pid` alone.
pub fn signal(pid: u32, sig: Signal) {
    kill(Pid::from_raw(i32::try_from(pid).unwrap()), sig).unwrap();
}

impl Setup {
    /// Like [`Setup::command`], in a process group of its own, so the shim
    /// is never a terminal's foreground job, whatever runs the tests.
    pub fn background(&self, args: &[&str]) -> Command {
        let mut cmd = self.command(args);
        cmd.process_group(0);
        cmd
    }
}
