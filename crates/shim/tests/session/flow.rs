//! Opening sessions, holds, fallbacks and self-feed.

use crate::harness::{
    GRANTED, cores_minus_one, daemon, grant_all, make_fifo, opened, setup, stdout,
};
use damper_proto::{ClientMsg, EventKind, HoldReason, ServerMsg};
use nix::sys::stat::Mode;
use nix::unistd::mkfifo;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

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
    // Timed from the daemon's accept, so a slow first launch of the shim
    // binary doesn't count.
    let fake = daemon(listener, |mut c| {
        let accepted = Instant::now();
        let _open = c.recv();
        c.wait_for_close();
        accepted
    });
    let out = s.output(&["build", "env"]);
    let exited = Instant::now();
    let accepted = fake.join().unwrap();
    let took = exited - accepted;
    assert!(took < Duration::from_secs(1), "{took:?}");
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).starts_with("flags=\n"), "{}", stdout(&out));
    assert!(out.stderr.is_empty(), "stderr: {:?}", out.stderr);
    let events = s.fallbacks();
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(&events[0].kind, EventKind::ShimFallback { .. }));
    assert!(events[0].ts_ms > 0);
}

#[test]
fn held_build_prints_hold_line_then_runs() {
    let (s, listener) = setup();
    let fifo = s.run.join("s-1-s1.fifo");
    make_fifo(&fifo);
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
fn daemon_error_and_second_grant_after_started_are_ignored() {
    let (s, listener) = setup();
    let fifo = s.run.join("s-1-s1.fifo");
    make_fifo(&fifo);
    let fake = daemon(listener, move |mut c| {
        let _open = c.recv();
        c.send(&opened(&fifo));
        c.send(&GRANTED);
        assert!(matches!(c.recv(), Some(ClientMsg::Started { .. })));
        c.send(&ServerMsg::Error {
            message: "late".to_owned(),
        });
        c.send(&GRANTED);
        c.wait_for_close();
    });
    let out = s.output(&["build", "env"]);
    fake.join().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("session=s1\n"), "{}", stdout(&out));
    assert!(out.stderr.is_empty(), "stderr: {:?}", out.stderr);
    let fallbacks = s.fallbacks();
    assert!(fallbacks.is_empty(), "{fallbacks:?}");
}

#[test]
fn daemon_gone_before_cargo_opens_the_fifo_still_self_feeds() {
    let (s, listener) = setup();
    let fifo = s.run.join("s-1-s1.fifo");
    mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();
    let gone = s.state.join("daemon-gone");
    let fake = {
        let (fifo, gone) = (fifo.clone(), gone.clone());
        daemon(listener, move |mut c| {
            let _open = c.recv();
            c.send(&opened(&fifo));
            c.send(&GRANTED);
            assert!(matches!(c.recv(), Some(ClientMsg::Started { .. })));
            drop(c);
            std::fs::write(&gone, "").unwrap();
        })
    };
    let tokens = cores_minus_one();
    let count = tokens.to_string();
    let child = s
        .command(&["build", "latefeed", gone.to_str().unwrap(), &count])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = child.wait_with_output().unwrap();
    fake.join().unwrap();
    assert_eq!(stdout(&out), format!("tokens={}\n", "+".repeat(tokens)));
    assert_eq!(out.status.code(), Some(0));
    assert!(!fifo.exists(), "the shim unlinks a FIFO it fed itself");
}
