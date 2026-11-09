//! Signals: before cargo starts, while it runs, and inherited ignores.

use crate::harness::{daemon, grant_all, opened, setup, signal, wait_for};
use damper_proto::{HoldReason, ServerMsg};
use nix::sys::signal::Signal;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
    signal(child.id(), sig);
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
fn sigterm_while_cargo_runs_is_forwarded_and_passed_through() {
    assert_forwarded_and_passed_through(Signal::SIGTERM);
}

#[test]
fn sigint_while_cargo_runs_exits_as_cargo_does() {
    assert_forwarded_and_passed_through(Signal::SIGINT);
}

/// Sends `sig` to the shim while cargo (`exec sleep`) runs: cargo dies of
/// it and the shim dies the same way.
fn assert_forwarded_and_passed_through(sig: Signal) {
    let (s, listener) = setup();
    let fake = grant_all(listener, s.run.join("s-1-s1.fifo"));
    let marker = s.state.join("cargo-running");
    let mut child = s
        .background(&["build", "hang", marker.to_str().unwrap()])
        .spawn()
        .unwrap();
    wait_for(&marker);
    // Give the shell time to exec into sleep.
    thread::sleep(Duration::from_millis(200));
    signal(child.id(), sig);
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(sig as i32), "{status:?}");
    fake.join().unwrap();
}

#[test]
fn sigint_to_a_background_shim_reaches_cargo_once() {
    let (s, listener) = setup();
    let fake = grant_all(listener, s.run.join("s-1-s1.fifo"));
    let (marker, stop, count) = (
        s.state.join("cargo-running"),
        s.state.join("stop"),
        s.state.join("sigints"),
    );
    let mut child = s
        .background(&[
            "build",
            "count",
            marker.to_str().unwrap(),
            stop.to_str().unwrap(),
            count.to_str().unwrap(),
        ])
        .spawn()
        .unwrap();
    wait_for(&marker);
    signal(child.id(), Signal::SIGINT);
    wait_for(&count);
    // Room for a second, unwanted delivery to show up.
    thread::sleep(Duration::from_millis(500));
    std::fs::write(&stop, "").unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(std::fs::read_to_string(&count).unwrap(), "int\n");
    fake.join().unwrap();
}

#[test]
fn sighup_ignored_by_the_parent_stays_ignored() {
    let (s, listener) = setup();
    let fake = grant_all(listener, s.run.join("s-1-s1.fifo"));
    let (marker, stop) = (s.state.join("cargo-running"), s.state.join("stop"));
    // As `nohup cargo build` would start it.
    let mut child = Command::new("/bin/sh")
        .args(["-c", "trap '' HUP; exec \"$0\" \"$@\""])
        .arg(&s.shim)
        .args([
            "build",
            "waitfor",
            marker.to_str().unwrap(),
            stop.to_str().unwrap(),
        ])
        .env_clear()
        .env("PATH", &s.path)
        .env("DAMPER_STATE_DIR", &s.state)
        .process_group(0)
        .spawn()
        .unwrap();
    wait_for(&marker);
    signal(child.id(), Signal::SIGHUP);
    thread::sleep(Duration::from_millis(300));
    std::fs::write(&stop, "").unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "{status:?}");
    fake.join().unwrap();
}
