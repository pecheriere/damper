//! End-to-end tests of the `damper` binary against a temporary state
//! directory (via `DAMPER_STATE_DIR`) and a scripted fake daemon.

#![expect(clippy::unwrap_used, reason = "test")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};
use std::thread::{self, JoinHandle};

use damper_proto::paths::resolve_paths_from;
use damper_proto::{ClientMsg, Event, EventKind, ServerMsg, SessionId, decode, encode};

fn damper(state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_damper"))
        .args(args)
        .env("DAMPER_STATE_DIR", state)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Serves one connection: records the request, sends `reply`.
fn fake_daemon(state: &Path, reply: ServerMsg) -> JoinHandle<ClientMsg> {
    let paths = resolve_paths_from(Some(state)).unwrap();
    fs::create_dir_all(&paths.run).unwrap();
    fs::set_permissions(&paths.run, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(&paths.socket).unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = Vec::new();
        BufReader::new(&stream)
            .read_until(b'\n', &mut line)
            .unwrap();
        let request = decode::<ClientMsg>(&line).unwrap();
        (&stream).write_all(&encode(&reply).unwrap()).unwrap();
        request
    })
}

fn event(ts_ms: u64) -> Event {
    Event {
        ts_ms,
        kind: EventKind::Error {
            message: "boom".into(),
        },
    }
}

#[test]
fn status_without_daemon_exits_2() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = resolve_paths_from(Some(tmp.path())).unwrap().socket;
    let expected = format!(
        "damper: damperd is not running (socket {}). Start it with 'brew services start damper'.\n",
        socket.display()
    );
    for args in [&["status"][..], &["log"], &["explain", "1-1"]] {
        let out = damper(tmp.path(), args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(text(&out.stderr), expected, "{args:?}");
        assert_eq!(text(&out.stdout), "");
    }
}

#[test]
fn enable_disable_toggle_flag() {
    let tmp = tempfile::tempdir().unwrap();
    // A fresh state directory, which `disable` creates privately.
    let state = tmp.path().join("state");
    let flag = state.join("disabled");
    let out = damper(&state, &["disable"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(flag.exists());
    assert_eq!(damper(&state, &["disable"]).status.code(), Some(0));
    let out = damper(&state, &["enable"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!flag.exists());
    assert_eq!(damper(&state, &["enable"]).status.code(), Some(0));
}

#[test]
fn log_sends_since_and_session_and_prints_json() {
    let tmp = tempfile::tempdir().unwrap();
    let server = fake_daemon(
        tmp.path(),
        ServerMsg::Events {
            events: vec![event(1)],
            truncated: true,
        },
    );
    let out = damper(
        tmp.path(),
        &["log", "--json", "--since", "1h", "--session", "7-1"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        server.join().unwrap(),
        ClientMsg::Log {
            since_ms: Some(3_600_000),
            session: Some(SessionId("7-1".into())),
        }
    );
    let json: serde_json::Value = serde_json::from_str(&text(&out.stdout)).unwrap();
    assert_eq!(json["truncated"], true);
    assert_eq!(json["events"][0]["type"], "error");
}

#[test]
fn log_human_notes_truncation() {
    let tmp = tempfile::tempdir().unwrap();
    let server = fake_daemon(
        tmp.path(),
        ServerMsg::Events {
            events: vec![event(1)],
            truncated: true,
        },
    );
    let out = damper(tmp.path(), &["log"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        server.join().unwrap(),
        ClientMsg::Log {
            since_ms: None,
            session: None
        }
    );
    let stdout = text(&out.stdout);
    assert!(stdout.contains("older events were omitted"), "{stdout}");
    assert!(stdout.contains("--since"));
    assert!(stdout.contains("boom"));
}

#[test]
fn log_rejects_bad_duration() {
    let tmp = tempfile::tempdir().unwrap();
    let out = damper(tmp.path(), &["log", "--since", "5d"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("--since"));
}

#[test]
fn daemon_error_prints_message_and_exits_1() {
    let tmp = tempfile::tempdir().unwrap();
    let server = fake_daemon(
        tmp.path(),
        ServerMsg::Error {
            message: "no such session".into(),
        },
    );
    let out = damper(tmp.path(), &["explain", "9-9"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stderr), "damper: no such session\n");
    assert_eq!(
        server.join().unwrap(),
        ClientMsg::Explain {
            session: SessionId("9-9".into())
        }
    );
}

#[test]
fn config_check_exit_codes() {
    let tmp = tempfile::tempdir().unwrap();
    let out = damper(tmp.path(), &["config", "check"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("max_hold"));

    fs::write(tmp.path().join("config.toml"), "bogus = 1\n").unwrap();
    let out = damper(tmp.path(), &["config", "check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stdout).contains("bogus"));
}

#[test]
fn doctor_exits_1_without_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let out = damper(tmp.path(), &["doctor"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stdout).contains("daemon"));
}
