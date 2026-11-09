#![expect(clippy::unwrap_used, reason = "test")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use damper_proto::{Event, EventKind};

use super::EventLog;

fn err(n: u32) -> EventKind {
    EventKind::Error {
        message: format!("e{n}"),
    }
}

fn messages(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|e| match &e.kind {
            EventKind::Error { message } => message.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn eventlog_rotates_and_reads_in_order() {
    let dir = tempfile::tempdir().unwrap();
    // Each line is about 50 bytes: a 120-byte limit keeps two per file.
    let mut log = EventLog::open_with_limit(dir.path(), 120).unwrap();
    for n in 0..20 {
        log.append(err(n)).unwrap();
    }
    let rotated: Vec<PathBuf> = (1..=4)
        .map(|k| dir.path().join(format!("events.jsonl.{k}")))
        .collect();
    assert!(rotated.iter().all(|p| p.exists()));
    assert!(!dir.path().join("events.jsonl.5").exists());
    for p in rotated.iter().chain([&dir.path().join("events.jsonl")]) {
        assert!(fs::metadata(p).unwrap().len() <= 120, "{}", p.display());
    }

    let all = messages(&log.read(None).unwrap());
    // Five files of two lines: the ten newest survive, oldest first.
    let expected: Vec<String> = (10..20).map(|n| format!("e{n}")).collect();
    assert_eq!(all, expected);
}

#[test]
fn read_filters_by_time() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = EventLog::open(dir.path()).unwrap();
    log.append(err(1)).unwrap();
    let events = log.read(None).unwrap();
    let ts = events[0].ts_ms;
    assert_eq!(log.read(Some(ts)).unwrap().len(), 1);
    assert_eq!(log.read(Some(ts + 1)).unwrap(), Vec::<Event>::new());
}

#[test]
fn eventlog_skips_corrupt_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = EventLog::open(dir.path()).unwrap();
    log.append(err(1)).unwrap();
    append_raw(&dir.path().join("events.jsonl"), b"{not json\n");
    log.append(err(2)).unwrap();
    assert_eq!(messages(&log.read(None).unwrap()), ["e1", "e2"]);
}

#[test]
fn reopening_keeps_the_size_and_appends() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = EventLog::open_with_limit(dir.path(), 120).unwrap();
    log.append(err(1)).unwrap();
    drop(log);
    let mut log = EventLog::open_with_limit(dir.path(), 120).unwrap();
    log.append(err(2)).unwrap();
    log.append(err(3)).unwrap();
    assert!(dir.path().join("events.jsonl.1").exists());
    assert_eq!(messages(&log.read(None).unwrap()), ["e1", "e2", "e3"]);
}

fn append_raw(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[test]
fn ingest_fallbacks_moves_entries() {
    let dir = tempfile::tempdir().unwrap();
    let fallbacks = dir.path().join("fallbacks.jsonl");
    let shim_line = |ts: u64, reason: &str| {
        format!(
            "{{\"ts_ms\":{ts},\"type\":\"shim_fallback\",\"reason\":\"{reason}\",\"cwd\":\"/src/app\"}}\n"
        )
    };
    append_raw(&fallbacks, shim_line(5, "daemon unreachable").as_bytes());
    append_raw(&fallbacks, b"garbage\n");
    // Only fallbacks are taken from the shim's file.
    append_raw(
        &fallbacks,
        b"{\"ts_ms\":6,\"type\":\"error\",\"message\":\"forged\"}\n",
    );
    append_raw(&fallbacks, shim_line(7, "connect timeout").as_bytes());

    let mut log = EventLog::open(dir.path()).unwrap();
    assert_eq!(log.ingest_fallbacks(&fallbacks).unwrap(), 2);
    assert!(!fallbacks.exists());
    let events = log.read(None).unwrap();
    let reasons: Vec<(u64, String)> = events
        .iter()
        .map(|e| match &e.kind {
            EventKind::ShimFallback { reason, cwd } => {
                assert_eq!(cwd, Path::new("/src/app"));
                (e.ts_ms, reason.clone())
            }
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        reasons,
        [
            (5, "daemon unreachable".to_owned()),
            (7, "connect timeout".to_owned())
        ]
    );
    // Nothing left to ingest the second time.
    assert_eq!(log.ingest_fallbacks(&fallbacks).unwrap(), 0);
}

#[test]
fn log_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut log = EventLog::open(dir.path()).unwrap();
    log.append(err(1)).unwrap();
    let mode = fs::metadata(dir.path().join("events.jsonl"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_torn_last_line_does_not_swallow_the_next_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = EventLog::open(dir.path()).unwrap();
    log.append(err(1)).unwrap();
    drop(log);
    // A crash mid-write left half a line without its newline.
    append_raw(&dir.path().join("events.jsonl"), b"{\"ts_ms\":3,\"ty");
    let mut log = EventLog::open(dir.path()).unwrap();
    log.append(err(2)).unwrap();
    assert_eq!(messages(&log.read(None).unwrap()), ["e1", "e2"]);
}

fn shim_line(ts: u64) -> String {
    format!(
        "{{\"ts_ms\":{ts},\"type\":\"shim_fallback\",\"reason\":\"r{ts}\",\"cwd\":\"/src/app\"}}\n"
    )
}

#[test]
fn an_unreadable_leftover_is_set_aside_and_ingest_carries_on() {
    let dir = tempfile::tempdir().unwrap();
    let fallbacks = dir.path().join("fallbacks.jsonl");
    let leftover = dir.path().join("fallbacks.jsonl.ingest");
    // O_NOFOLLOW refuses a symlink with ELOOP.
    std::os::unix::fs::symlink(dir.path().join("elsewhere"), &leftover).unwrap();
    append_raw(&fallbacks, shim_line(5).as_bytes());

    let mut log = EventLog::open(dir.path()).unwrap();
    assert_eq!(log.ingest_fallbacks(&fallbacks).unwrap(), 1);
    assert!(
        dir.path()
            .join("fallbacks.jsonl.bad")
            .symlink_metadata()
            .is_ok()
    );
    assert!(leftover.symlink_metadata().is_err());
    let events = log.read(None).unwrap();
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::Error { message } if message.contains("fallbacks.jsonl.ingest")
    )));
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::ShimFallback { .. }) && e.ts_ms == 5)
    );
    // The next start is not stuck on it.
    assert_eq!(log.ingest_fallbacks(&fallbacks).unwrap(), 0);
}

#[test]
fn ingest_skips_lines_longer_than_max_line() {
    let dir = tempfile::tempdir().unwrap();
    let fallbacks = dir.path().join("fallbacks.jsonl");
    append_raw(&fallbacks, shim_line(1).as_bytes());
    let mut oversized = shim_line(2).into_bytes();
    oversized.pop();
    oversized.splice(1..1, vec![b' '; damper_proto::MAX_LINE]);
    oversized.push(b'\n');
    append_raw(&fallbacks, &oversized);
    append_raw(&fallbacks, shim_line(3).as_bytes());

    let mut log = EventLog::open(dir.path()).unwrap();
    assert_eq!(log.ingest_fallbacks(&fallbacks).unwrap(), 2);
    let stamps: Vec<u64> = log.read(None).unwrap().iter().map(|e| e.ts_ms).collect();
    assert_eq!(stamps, [1, 3]);
}
