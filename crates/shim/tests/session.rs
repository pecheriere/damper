//! End-to-end: the built shim against a scripted fake daemon.
#![expect(clippy::unwrap_used, reason = "test")]

use damper_proto::{ClientMsg, Event, EventKind, HoldReason, ServerMsg, SessionId, decode, encode};
use nix::sys::signal::{Signal, kill};
use nix::sys::stat::Mode;
use nix::unistd::{Pid, mkfifo};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
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
esac
exit 2
"#;

struct Setup {
    _tmp: tempfile::TempDir,
    state: PathBuf,
    run: PathBuf,
    shim: PathBuf,
    path: std::ffi::OsString,
}

fn setup() -> (Setup, UnixListener) {
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
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.shim);
        cmd.env_clear()
            .env("PATH", &self.path)
            .env("DAMPER_STATE_DIR", &self.state)
            .args(args);
        cmd
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn fallbacks(&self) -> Vec<Event> {
        let text = fs::read_to_string(self.state.join("log/fallbacks.jsonl")).unwrap_or_default();
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

/// One accepted connection, seen from the daemon's side.
struct Conn {
    reader: BufReader<UnixStream>,
}

impl Conn {
    /// The next message, or `None` once the shim has closed the socket.
    fn recv(&mut self) -> Option<ClientMsg> {
        let mut line = Vec::new();
        let n = self.reader.read_until(b'\n', &mut line).unwrap();
        (n > 0).then(|| decode(&line).unwrap())
    }

    fn send(&mut self, msg: &ServerMsg) {
        self.reader
            .get_mut()
            .write_all(&encode(msg).unwrap())
            .unwrap();
    }

    fn wait_for_close(&mut self) {
        while self.recv().is_some() {}
    }
}

fn daemon<T: Send + 'static>(
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

fn sid() -> SessionId {
    SessionId("s1".to_owned())
}

fn opened(fifo: &Path) -> ServerMsg {
    ServerMsg::Opened {
        session: sid(),
        fifo: fifo.to_path_buf(),
    }
}

const GRANTED: ServerMsg = ServerMsg::Granted {
    threads: 4,
    after_timeout: false,
};

/// Opens and grants a session, then waits for the shim to close.
/// Returns the `Open` and the `Started` messages.
fn grant_all(listener: UnixListener, fifo: PathBuf) -> JoinHandle<Vec<ClientMsg>> {
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

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn cores_minus_one() -> usize {
    thread::available_parallelism().map_or(1, std::num::NonZero::get) - 1
}

#[test]
fn granted_session_runs_cargo_with_fifo_env() {
    let (s, listener) = setup();
    let fifo = s.run.join("s-1-s1.fifo");
    let fake = grant_all(listener, fifo.clone());
    let child = s
        .command(&["build", "env"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let shim_pid = child.id();
    let out = child.wait_with_output().unwrap();
    let flags = format!("-j --jobserver-auth=fifo:{}", fifo.display());
    assert_eq!(
        stdout(&out),
        format!("flags={flags}\nmake={flags}\nsession=s1\nrtt=unset\n")
    );
    assert_eq!(out.status.code(), Some(0));
    let seen = fake.join().unwrap();
    match &seen[0] {
        ClientMsg::Open {
            subcommand,
            shim_pid: pid,
            nested,
            ..
        } => {
            assert_eq!(subcommand, "build");
            assert_eq!(*pid, shim_pid);
            assert!(!nested);
        }
        other => panic!("expected Open, got {other:?}"),
    }
    assert!(
        matches!(seen.get(1), Some(ClientMsg::Started { .. })),
        "{seen:?}"
    );
}

#[test]
fn silent_daemon_falls_back_within_timeout() {
    let (s, listener) = setup();
    let fake = daemon(listener, |mut c| {
        let _open = c.recv();
        c.wait_for_close();
    });
    let started = Instant::now();
    let out = s.output(&["build", "env"]);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).starts_with("flags=\n"), "{}", stdout(&out));
    assert!(out.stderr.is_empty(), "stderr: {:?}", out.stderr);
    fake.join().unwrap();
    let events = s.fallbacks();
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(&events[0].kind, EventKind::ShimFallback { .. }));
    assert!(events[0].ts_ms > 0);
}

#[test]
fn held_build_prints_hold_line_then_runs() {
    let (s, listener) = setup();
    let fifo = s.run.join("s-1-s1.fifo");
    let fake = daemon(listener, move |mut c| {
        let _open = c.recv();
        c.send(&opened(&fifo));
        c.send(&ServerMsg::Held {
            reason: HoldReason::MemoryNearlyFull,
        });
        thread::sleep(Duration::from_millis(300));
        c.send(&ServerMsg::Heartbeat);
        c.send(&ServerMsg::Granted {
            threads: 1,
            after_timeout: true,
        });
        c.wait_for_close();
    });
    let out = s.output(&["build", "env"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("session=s1\n"), "{}", stdout(&out));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "damper: holding this build, memory nearly full (waited 0s). \
         Run 'damper status' for details. Session s1.\n\
         damper: held for 0m, running this build with one job. \
         Run 'damper explain s1' for details.\n"
    );
    fake.join().unwrap();
}

/// Starts a held build, waits for its hold line, then sends it `sig`.
fn held_then_signalled(sig: Signal) -> (Child, JoinHandle<()>) {
    let (s, listener) = setup();
    let fifo = s.run.join("s-1-s1.fifo");
    let fake = daemon(listener, move |mut c| {
        let _open = c.recv();
        c.send(&opened(&fifo));
        c.send(&ServerMsg::Held {
            reason: HoldReason::PressureCritical,
        });
        c.wait_for_close();
    });
    let mut child = s
        .command(&["build", "env"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stderr.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("damper: holding this build"), "{line}");
    kill(Pid::from_raw(i32::try_from(child.id()).unwrap()), sig).unwrap();
    // Keep the temp dir alive until the shim has exited.
    let keep = thread::spawn(move || {
        fake.join().unwrap();
        drop(s);
    });
    (child, keep)
}

fn assert_exits_as(sig: Signal) {
    let (mut child, keep) = held_then_signalled(sig);
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(sig as i32), "{status:?}");
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(out.is_empty(), "cargo ran: {out}");
    keep.join().unwrap();
}

#[test]
fn sigterm_while_held_exits_as_sigterm() {
    assert_exits_as(Signal::SIGTERM);
}

#[test]
fn sigint_while_held_exits_as_sigint() {
    assert_exits_as(Signal::SIGINT);
}

#[test]
fn daemon_vanishing_mid_build_triggers_self_feed() {
    let (s, listener) = setup();
    let fifo = s.run.join("s-1-s1.fifo");
    mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();
    let marker = s.state.join("cargo-has-fifo");
    let tokens = cores_minus_one();
    let fake = {
        let (fifo, marker) = (fifo.clone(), marker.clone());
        daemon(listener, move |mut c| {
            let _open = c.recv();
            c.send(&opened(&fifo));
            c.send(&GRANTED);
            assert!(matches!(c.recv(), Some(ClientMsg::Started { .. })));
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            // Dropping the connection: the daemon is gone.
        })
    };
    let marker_arg = marker.to_str().unwrap();
    let count = tokens.to_string();
    let out = s.output(&["build", "feed", marker_arg, &count]);
    fake.join().unwrap();
    assert_eq!(stdout(&out), format!("tokens={}\n", "+".repeat(tokens)));
    assert_eq!(out.status.code(), Some(0));
    assert!(!fifo.exists(), "the shim unlinks a FIFO it fed itself");
}

#[test]
fn exit_code_and_signal_pass_through() {
    let (s, listener) = setup();
    let fake = grant_all(listener, s.run.join("a.fifo"));
    let out = s.output(&["build", "exit101"]);
    assert_eq!(out.status.code(), Some(101));
    fake.join().unwrap();

    let (s, listener) = setup();
    let fake = grant_all(listener, s.run.join("b.fifo"));
    let out = s.output(&["build", "sigkill"]);
    assert_eq!(out.status.signal(), Some(9), "{:?}", out.status);
    fake.join().unwrap();
}

#[test]
fn daemon_closing_while_held_falls_back() {
    let (s, listener) = setup();
    let fifo = s.run.join("s-1-s1.fifo");
    let fake = daemon(listener, move |mut c| {
        let _open = c.recv();
        c.send(&opened(&fifo));
        c.send(&ServerMsg::Held {
            reason: HoldReason::PressureCritical,
        });
    });
    let out = s.output(&["build", "env"]);
    fake.join().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).starts_with("flags=\n"), "{}", stdout(&out));
    assert_eq!(s.fallbacks().len(), 1);
}

#[test]
fn signal_while_cargo_runs_is_forwarded_and_passed_through() {
    let (s, listener) = setup();
    let fake = grant_all(listener, s.run.join("s-1-s1.fifo"));
    let marker = s.state.join("cargo-running");
    let mut child = s
        .command(&["build", "hang", marker.to_str().unwrap()])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    // Give the shell time to exec into sleep.
    thread::sleep(Duration::from_millis(100));
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap());
    kill(pid, Signal::SIGTERM).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(Signal::SIGTERM as i32), "{status:?}");
    fake.join().unwrap();
}
