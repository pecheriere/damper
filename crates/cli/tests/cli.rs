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
use std::time::{SystemTime, UNIX_EPOCH};

use damper_proto::paths::resolve_paths_from;
use damper_proto::{
    ClientMsg, Event, EventKind, InputsSummary, LimitReason, Pressure, ServerMsg, SessionId,
    SessionState, SessionStatus, Status, decode, encode,
};

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

fn epoch_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

#[test]
fn log_sends_since_and_session_and_prints_json() {
    const HOUR_MS: u64 = 3_600_000;
    let tmp = tempfile::tempdir().unwrap();
    let server = fake_daemon(
        tmp.path(),
        ServerMsg::Events {
            events: vec![event(1)],
            truncated: true,
        },
    );
    let before = epoch_ms();
    let out = damper(
        tmp.path(),
        &["log", "--json", "--since", "1h", "--session", "7-1"],
    );
    let after = epoch_ms();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let ClientMsg::Log { since_ms, session } = server.join().unwrap() else {
        panic!("expected Log");
    };
    assert_eq!(session, Some(SessionId("7-1".into())));
    let since = since_ms.unwrap();
    assert!(
        (before - HOUR_MS..=after - HOUR_MS).contains(&since),
        "{since} not within an hour before now ({before}..{after})"
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

fn inputs() -> InputsSummary {
    InputsSummary {
        pressure: Pressure::Normal,
        available_pct: 60,
        swap_used: 0,
        swap_growing: false,
        build_footprint: 1 << 30,
        reserve: 1 << 30,
    }
}

fn status_with(id: &str, cwd: &str) -> Status {
    Status {
        limit: 4,
        reason: LimitReason::Headroom,
        cores: 8,
        memory: 16 << 30,
        inputs: inputs(),
        holding: false,
        sessions: vec![SessionStatus {
            session: SessionId(id.into()),
            cwd: cwd.into(),
            subcommand: "build".into(),
            state: SessionState::Compiling,
            slots: 2,
            tokens_outstanding: 1,
            age_secs: 5,
        }],
        warnings: vec![],
    }
}

/// True when `text` has a control character (other than newline) or a
/// Unicode format/bidi character that could drive a terminal.
fn has_unsafe_char(text: &str) -> bool {
    text.chars().any(|c| {
        (c.is_control() && c != '\n')
            || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}'..='\u{200f}')
    })
}

#[test]
fn status_prints_human_and_json_from_the_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let status = status_with("5-1", "/work/demo");
    let server = fake_daemon(tmp.path(), ServerMsg::Status(status.clone()));
    let out = damper(tmp.path(), &["status"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(server.join().unwrap(), ClientMsg::Status);
    let human = text(&out.stdout);
    assert!(human.contains("5-1"), "{human}");
    assert!(human.contains("/work/demo"), "{human}");

    let server = fake_daemon_again(tmp.path(), ServerMsg::Status(status.clone()));
    let out = damper(tmp.path(), &["status", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    server.join().unwrap();
    let parsed: Status = serde_json::from_str(&text(&out.stdout)).unwrap();
    assert_eq!(parsed, status);
}

/// Like [`fake_daemon`], for a second daemon in the same state directory.
fn fake_daemon_again(state: &Path, reply: ServerMsg) -> JoinHandle<ClientMsg> {
    let paths = resolve_paths_from(Some(state)).unwrap();
    fs::remove_file(&paths.socket).unwrap();
    fake_daemon(state, reply)
}

#[test]
fn escapes_in_daemon_text_never_reach_the_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let server = fake_daemon(
        tmp.path(),
        ServerMsg::Error {
            message: "bad \u{1b}[31mred".into(),
        },
    );
    let out = damper(tmp.path(), &["explain", "1-1"]);
    server.join().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stderr), "damper: bad ?[31mred\n");

    let server = fake_daemon_again(
        tmp.path(),
        ServerMsg::Status(status_with("1-\u{1b}]0;x", "/tmp/\u{202e}gpj.exe")),
    );
    let out = damper(tmp.path(), &["status"]);
    server.join().unwrap();
    let human = text(&out.stdout);
    assert!(human.contains("gpj.exe"), "{human}");
    assert!(!has_unsafe_char(&human), "{human:?}");

    let server = fake_daemon_again(
        tmp.path(),
        ServerMsg::Events {
            events: vec![Event {
                ts_ms: 1,
                kind: EventKind::Error {
                    message: "x\u{202e}y".into(),
                },
            }],
            truncated: false,
        },
    );
    let out = damper(tmp.path(), &["explain", "1-\u{1b}[2J"]);
    server.join().unwrap();
    assert!(!has_unsafe_char(&text(&out.stdout)), "{:?}", out.stdout);
}

#[test]
fn escapes_in_config_never_reach_the_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("config.toml"), "\"bo\\u001b[31mgus\" = 1\n").unwrap();
    let out = damper(tmp.path(), &["config", "check"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("bogus") || stdout.contains("bo?[31mgus"),
        "{stdout:?}"
    );
    assert!(!has_unsafe_char(&stdout), "{stdout:?}");
    let out = damper(tmp.path(), &["doctor"]);
    assert!(!has_unsafe_char(&text(&out.stdout)), "{:?}", out.stdout);
}

#[test]
fn disable_does_not_follow_a_symlink_at_the_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    assert_eq!(damper(&state, &["enable"]).status.code(), Some(0));
    assert_eq!(damper(&state, &["disable"]).status.code(), Some(0));
    assert_eq!(damper(&state, &["enable"]).status.code(), Some(0));
    let victim = tmp.path().join("victim");
    fs::write(&victim, "keep").unwrap();
    std::os::unix::fs::symlink(&victim, state.join("disabled")).unwrap();
    let out = damper(&state, &["disable"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
}

#[test]
fn disable_with_a_loose_state_dir_says_how_to_fix_it() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    let out = damper(&state, &["disable"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(&format!("chmod 700 {}", state.display())),
        "{stderr}"
    );
}
