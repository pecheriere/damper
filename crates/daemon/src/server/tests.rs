#![expect(clippy::unwrap_used, reason = "test")]

use damper_proto::{
    ClientMsg, Event, EventKind, MAX_LINE, MAX_REPLY, ServerMsg, decode, decode_with_limit,
};
use tokio::io::BufReader;

use super::checks::{decode_error_reason, never_hold, peer_allowed, started_allowed};
use super::wire::{LineError, fit_events, next_line};

#[test]
fn only_the_same_uid_is_allowed() {
    assert!(peer_allowed(501, 501));
    assert!(!peer_allowed(502, 501));
    assert!(!peer_allowed(0, 501));
}

#[test]
fn only_rust_analyzers_builds_are_never_held() {
    assert!(never_hold(Some("rust-analyzer")));
    assert!(!never_hold(Some("zsh")));
    assert!(!never_hold(Some("rust-analyzer-proc-macro-srv")));
    assert!(!never_hold(None));
}

#[test]
fn started_needs_cargo_to_be_the_shims_child() {
    assert!(started_allowed(Some(4242), 4242));
    assert!(!started_allowed(Some(1), 4242));
    assert!(!started_allowed(None, 4242));
}

#[test]
fn decode_errors_never_echo_the_client() {
    let secret = "hunter2-secret";
    for line in [
        format!("{{\"v\":1,\"type\":\"{secret}\"}}"),
        format!("{{\"v\":1,\"type\":\"log\",\"since_ms\":\"{secret}\"}}"),
    ] {
        let err = decode::<ClientMsg>(line.as_bytes()).unwrap_err();
        // serde's own text would have leaked it.
        assert!(err.to_string().contains(secret), "{err}");
        let reason = decode_error_reason(&err);
        assert!(!reason.contains(secret), "{reason}");
        assert!(reason.starts_with("malformed message: "), "{reason}");
    }
    let err = decode::<ClientMsg>(b"{\"v\":1,secret").unwrap_err();
    let reason = decode_error_reason(&err);
    assert_eq!(
        reason,
        "malformed message: syntax error at line 1, column 8"
    );
}

#[tokio::test]
async fn reads_lines_one_at_a_time() {
    let mut r = BufReader::new(&b"one\ntwo\npartial"[..]);
    let mut buf = Vec::new();
    assert!(next_line(&mut r, &mut buf).await.unwrap());
    assert_eq!(buf, b"one\n");
    assert!(next_line(&mut r, &mut buf).await.unwrap());
    assert_eq!(buf, b"two\n");
    // A last line without a newline is dropped at EOF.
    assert!(!next_line(&mut r, &mut buf).await.unwrap());
}

#[tokio::test]
async fn a_line_of_exactly_max_line_is_accepted() {
    let mut line = vec![b'x'; MAX_LINE - 1];
    line.push(b'\n');
    let mut r = BufReader::with_capacity(1000, &line[..]);
    let mut buf = Vec::new();
    assert!(next_line(&mut r, &mut buf).await.unwrap());
    assert_eq!(buf.len(), MAX_LINE);
}

#[tokio::test]
async fn a_longer_line_is_refused_without_buffering_it_all() {
    let line = vec![b'x'; MAX_LINE * 4];
    let mut r = BufReader::with_capacity(1000, &line[..]);
    let mut buf = Vec::new();
    assert!(matches!(
        next_line(&mut r, &mut buf).await,
        Err(LineError::TooLong)
    ));
    assert!(buf.len() <= MAX_LINE + 1);
}

fn event(n: u64) -> Event {
    Event {
        ts_ms: n,
        kind: EventKind::Error {
            message: "x".repeat(100),
        },
    }
}

fn kept(line: &[u8]) -> (Vec<Event>, bool) {
    match decode_with_limit(line, MAX_REPLY).unwrap() {
        ServerMsg::Events { events, truncated } => (events, truncated),
        other => panic!("not events: {other:?}"),
    }
}

#[test]
fn long_histories_keep_the_newest_events_and_say_so() {
    let events: Vec<Event> = (0..2000).map(event).collect();
    let line = fit_events(events, MAX_LINE).unwrap();
    assert!(line.len() <= MAX_LINE);
    let (kept, truncated) = kept(&line);
    assert!(truncated);
    assert!(kept.len() > 100 && kept.len() < 2000);
    let first = kept[0].ts_ms;
    assert!(kept.iter().map(|e| e.ts_ms).eq(first..2000));
}

#[test]
fn a_history_that_fits_exactly_is_sent_whole() {
    let events: Vec<Event> = (0..50).map(event).collect();
    let whole = fit_events(events.clone(), MAX_REPLY).unwrap();
    let (all, truncated) = kept(&whole);
    assert!(!truncated);
    assert_eq!(all, events);
    // The same history in a line of exactly its length is still whole.
    let (all, truncated) = kept(&fit_events(events.clone(), whole.len()).unwrap());
    assert!(!truncated);
    assert_eq!(all.len(), 50);
    let (cut, truncated) = kept(&fit_events(events, whole.len() - 1).unwrap());
    assert!(truncated);
    assert_eq!(cut.len(), 49);
}
