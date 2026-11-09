#![expect(clippy::unwrap_used, reason = "test")]

use damper_proto::{Event, EventKind, MAX_LINE, ServerMsg, decode};
use tokio::io::BufReader;

use super::{LineError, fit_events, next_line, peer_allowed};

#[test]
fn only_the_same_uid_is_allowed() {
    assert!(peer_allowed(501, 501));
    assert!(!peer_allowed(502, 501));
    assert!(!peer_allowed(0, 501));
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

#[test]
fn long_histories_keep_the_newest_events() {
    let events: Vec<Event> = (0..2000).map(event).collect();
    let line = fit_events(&events).unwrap();
    assert!(line.len() <= MAX_LINE);
    let ServerMsg::Events { events: kept } = decode(&line).unwrap() else {
        panic!("not events");
    };
    assert!(kept.len() > 100 && kept.len() < 2000);
    assert_eq!(kept.last().unwrap().ts_ms, 1999);
    let first = kept[0].ts_ms;
    assert!(kept.iter().map(|e| e.ts_ms).eq(first..2000));
}

#[test]
fn short_histories_are_sent_whole() {
    let events: Vec<Event> = (0..3).map(event).collect();
    let ServerMsg::Events { events: kept } = decode(&fit_events(&events).unwrap()).unwrap() else {
        panic!("not events");
    };
    assert_eq!(kept, events);
}
