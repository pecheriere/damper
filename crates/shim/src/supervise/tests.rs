#![expect(clippy::unwrap_used, reason = "test")]

use super::{DAEMON_LOST, Fifo, await_exit, reap, spare_cores, wait_on, watch_daemon};
use crate::fallback::open_fifo;
use crate::signals::{Phase, SharedPhase, lock};
use damper_proto::paths::{Paths, resolve_paths_from};
use damper_proto::{Client, ClientMsg, Event, EventKind, ServerMsg, decode, encode};
use nix::errno::Errno;
use nix::sys::stat::Mode;
use nix::unistd::{Pid, mkfifo};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Paths under `dir`, with the log directory in place.
fn paths_in(dir: &Path) -> Paths {
    let paths = resolve_paths_from(Some(dir)).unwrap();
    std::fs::create_dir_all(&paths.log).unwrap();
    paths
}

fn fallbacks(paths: &Paths) -> Vec<Event> {
    std::fs::read_to_string(paths.log.join("fallbacks.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_silent_daemon_gets_the_fifo_fed_after_the_heartbeat_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let socket = tmp.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let path = tmp.path().join("s.fifo");
    mkfifo(&path, Mode::from_bits_truncate(0o600)).unwrap();
    let fifo = Fifo {
        file: open_fifo(&path, tmp.path()).unwrap(),
        self_fed: AtomicBool::new(false),
        paths: paths.clone(),
        cwd: "/w".into(),
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
    let spare = spare_cores();
    let mut reader = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut tokens = vec![0u8; usize::try_from(spare).unwrap()];
    reader.read_exact(&mut tokens).unwrap();
    assert!(tokens.iter().all(|&t| t == b'+'), "{tokens:?}");

    // A daemon that recovers reads how many tokens the shim fed, then EOF.
    let mut lines = BufReader::new(daemon);
    let mut line = Vec::new();
    lines.read_until(b'\n', &mut line).unwrap();
    assert_eq!(
        decode::<ClientMsg>(&line).unwrap(),
        ClientMsg::SelfFed { tokens: spare }
    );
    line.clear();
    assert_eq!(lines.read_until(b'\n', &mut line).unwrap(), 0);

    // The self-feed is on record for `damper log`.
    let events = fallbacks(&paths);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        events[0].kind,
        EventKind::ShimFallback {
            reason: DAEMON_LOST.to_owned(),
            cwd: "/w".into(),
        }
    );
}

#[test]
fn a_dead_daemon_still_gets_the_fifo_fed_and_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let socket = tmp.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let path = tmp.path().join("s.fifo");
    mkfifo(&path, Mode::from_bits_truncate(0o600)).unwrap();
    let fifo = Fifo {
        file: open_fifo(&path, tmp.path()).unwrap(),
        self_fed: AtomicBool::new(false),
        paths: paths.clone(),
        cwd: "/w".into(),
    };
    let client = Client::connect(&socket, Duration::from_secs(5)).unwrap();
    let (daemon, _) = listener.accept().unwrap();
    drop(daemon);
    drop(listener);
    watch_daemon(client, &fifo, Duration::from_secs(5));
    assert!(fifo.self_fed.load(Ordering::SeqCst));
    assert_eq!(fallbacks(&paths).len(), 1);
}

#[test]
fn await_exit_returns_for_a_child_that_already_exited() {
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id();
    // Long enough for it to exit and become an unreaped zombie.
    thread::sleep(Duration::from_millis(300));
    let (done, waited) = mpsc::channel();
    thread::spawn(move || {
        done.send(await_exit(pid)).unwrap();
    });
    assert!(waited.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(child.wait().unwrap().success());
}

#[test]
fn await_exit_waits_for_a_running_child() {
    let mut child = Command::new("/bin/sleep").arg("0.3").spawn().unwrap();
    let started = Instant::now();
    assert!(await_exit(child.id()));
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert!(child.wait().unwrap().success());
}

#[test]
fn a_kqueue_failure_is_not_an_exit() {
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    assert!(!wait_on(Err(Errno::EMFILE), child.id()));
    assert!(child.wait().unwrap().success());
}

#[test]
fn signals_still_reach_cargo_until_reaped_when_the_wait_failed() {
    // `cat` runs until the test closes its stdin, so nothing races the check.
    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let release = child.stdin.take().unwrap();
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap());
    let phase: SharedPhase = Arc::new(Mutex::new(Phase::Running(pid)));
    let reaper = {
        let phase = Arc::clone(&phase);
        thread::spawn(move || reap(child, &phase, false))
    };
    // Give the reaper time to block in its wait; the child cannot exit yet.
    thread::sleep(Duration::from_millis(100));
    assert!(!reaper.is_finished());
    assert!(matches!(*lock(&phase), Phase::Running(p) if p == pid));
    drop(release);
    assert!(reaper.join().unwrap().unwrap().success());
    assert!(matches!(*lock(&phase), Phase::Done));
}

#[test]
fn an_exited_child_is_marked_done_before_reaping() {
    let child = Command::new("/usr/bin/true").spawn().unwrap();
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap());
    assert!(await_exit(child.id()));
    let phase: SharedPhase = Arc::new(Mutex::new(Phase::Running(pid)));
    let status = reap(child, &phase, true).unwrap();
    assert!(status.success());
    assert!(matches!(*lock(&phase), Phase::Done));
}
