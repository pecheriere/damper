//! Reply size caps, the first-line timeout and the `Started` pid check.

use std::process::{Child, Command};

use damper_proto::{Client, Event};

use super::*;

/// Writes `count` events of about 130 bytes each, stamped 0, 1, 2, …, to
/// the event log before the daemon starts.
fn prewrite_events(p: &Paths, count: u64) {
    damper_proto::paths::prepare_dirs(p).unwrap();
    let mut text = String::new();
    for ts in 0..count {
        let event = Event {
            ts_ms: ts,
            kind: EventKind::Error {
                message: "x".repeat(100),
            },
        };
        text.push_str(&serde_json::to_string(&event).unwrap());
        text.push('\n');
    }
    std::fs::write(p.log.join("events.jsonl"), text).unwrap();
}

/// Asks for the whole log with the blocking client the CLI uses.
async fn log_via_client(p: &Paths) -> ServerMsg {
    let socket = p.socket.clone();
    tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket, Duration::from_secs(5)).unwrap();
        client
            .send(&ClientMsg::Log {
                since_ms: None,
                session: None,
            })
            .unwrap();
        client
            .recv_with_limit(Some(Duration::from_secs(5)), MAX_REPLY)
            .unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_events_reply_longer_than_max_line_arrives_whole() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    // About 130 KiB of history: twice MAX_LINE.
    prewrite_events(&p, 1000);
    let d = start(&p).await;
    let ServerMsg::Events { events, truncated } = log_via_client(&p).await else {
        panic!("expected events");
    };
    assert!(!truncated);
    let stamps: Vec<u64> = events.iter().map(|e| e.ts_ms).take(1000).collect();
    assert!(stamps.into_iter().eq(0..1000));
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_events_reply_over_the_cap_keeps_the_newest_and_says_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    prewrite_events(&p, 1000);
    let max_reply = 32 * 1024;
    let d = start_with(
        &p,
        Options {
            max_reply,
            ..Options::default()
        },
    )
    .await;
    wait_for_event(&p, |k| matches!(k, EventKind::LimitChanged { .. })).await;
    let ServerMsg::Events { events, truncated } = log_via_client(&p).await else {
        panic!("expected events");
    };
    assert!(truncated);
    // The oldest are gone; the pre-written ones kept run up to the last.
    let old: Vec<u64> = events
        .iter()
        .filter(|e| e.ts_ms < 1000)
        .map(|e| e.ts_ms)
        .collect();
    assert!(old.len() > 10 && old.len() < 1000);
    assert!(old.iter().copied().eq(old[0]..1000));
    // The daemon's own start-up events, the newest, are there.
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::LimitChanged { .. }))
    );
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_status_over_the_cap_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let opts = Options {
        max_reply: 64,
        ..Options::default()
    };
    let d = start_with(&p, opts).await;
    let mut conn = Conn::new(&p.socket).await;
    conn.send(&ClientMsg::Status).await;
    assert!(matches!(conn.recv().await, Some(ServerMsg::Error { .. })));
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_connection_is_closed_after_the_first_line_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let opts = Options {
        first_line_timeout: Duration::from_millis(200),
        ..Options::default()
    };
    let d = start_with(&p, opts).await;
    let mut silent = Conn::new(&p.socket).await;
    // Half a line does not count.
    silent.raw(b"{\"v\":1,").await;
    let message = silent.expect_error_then_close().await;
    assert!(message.contains("no complete request"), "{message}");
    wait_for_event(&p, |k| {
        matches!(k, EventKind::ClientRejected { reason } if reason.contains("no complete request"))
    })
    .await;
    // A session that keeps quiet after its first line stays open.
    let mut conn = Conn::new(&p.socket).await;
    open_session(&mut conn).await;
    sleep(Duration::from_millis(400)).await;
    assert_eq!(Conn::new(&p.socket).await.status().await.sessions.len(), 1);
    d.stop().await.unwrap();
}

async fn wait_until_gone(path: &Path) {
    for _ in 0..300 {
        if !path.exists() {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("{} is still there", path.display());
}

#[tokio::test(flavor = "multi_thread")]
async fn started_for_a_pid_that_is_not_the_shims_child_closes_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    let fifo = open_session(&mut conn).await;
    conn.send(&ClientMsg::Started { cargo_pid: 1 }).await;
    let message = conn.expect_error_then_close().await;
    assert!(message.contains("not a child"), "{message}");
    wait_for_event(&p, is_rejected).await;
    wait_until_gone(&fifo).await;
    assert_eq!(Conn::new(&p.socket).await.status().await.sessions.len(), 0);
    d.stop().await.unwrap();
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn started_for_the_shims_child_is_watched_until_it_exits() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    let fifo = open_session(&mut conn).await;
    // This test process is the shim; its child plays cargo.
    let mut cargo = Killed(Command::new("/bin/sleep").arg("30").spawn().unwrap());
    conn.send(&ClientMsg::Started {
        cargo_pid: cargo.0.id(),
    })
    .await;
    // The session stays open while cargo runs, connection or not.
    drop(conn);
    sleep(Duration::from_millis(300)).await;
    assert!(fifo.exists());
    assert_eq!(Conn::new(&p.socket).await.status().await.sessions.len(), 1);
    cargo.0.kill().unwrap();
    cargo.0.wait().unwrap();
    wait_until_gone(&fifo).await;
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn started_for_a_cargo_that_already_exited_closes_the_session_normally() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    let fifo = open_session(&mut conn).await;
    // A short-lived cargo: finished and reaped before `Started` arrives.
    let mut cargo = Command::new("/usr/bin/true").spawn().unwrap();
    let pid = cargo.id();
    cargo.wait().unwrap();
    conn.send(&ClientMsg::Started { cargo_pid: pid }).await;
    drop(conn);
    wait_for_event(&p, |k| matches!(k, EventKind::SessionClosed { .. })).await;
    wait_until_gone(&fifo).await;
    let mut log = Conn::new(&p.socket).await;
    log.send(&ClientMsg::Log {
        since_ms: None,
        session: None,
    })
    .await;
    let Some(ServerMsg::Events { events, .. }) = log.recv().await else {
        panic!("expected events");
    };
    assert!(
        !events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Error { .. })
                || matches!(&e.kind, EventKind::ClientRejected { reason } if reason.contains("cargo_pid"))),
        "{events:?}"
    );
    d.stop().await.unwrap();
}
