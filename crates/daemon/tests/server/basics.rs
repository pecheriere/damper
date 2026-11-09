//! Serving, protocol errors, start-up and shutdown.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn serves_status() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let status = Conn::new(&p.socket).await.status().await;
    assert!(status.limit >= 1);
    assert!(status.cores >= 1);
    assert_eq!(status.sessions.len(), 0);
    // The start-up events come first in the log.
    wait_for_event(&p, |k| matches!(k, EventKind::ConfigLoaded { .. })).await;
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn open_returns_fifo_and_grant() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    let fifo = open_session(&mut conn).await;
    assert_eq!(fifo.parent(), Some(p.run.as_path()));
    assert!(fifo.exists());
    assert_eq!(Conn::new(&p.socket).await.status().await.sessions.len(), 1);

    // Closing the connection closes the session and removes its FIFO.
    drop(conn);
    for _ in 0..200 {
        if !fifo.exists() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(!fifo.exists());
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_oversized_line_and_keeps_serving() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut bad = Conn::new(&p.socket).await;
    bad.raw(&vec![b'x'; MAX_LINE + 10]).await;
    bad.expect_error_then_close().await;
    let status = Conn::new(&p.socket).await.status().await;
    assert!(status.limit >= 1);
    wait_for_event(&p, is_rejected).await;
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_wrong_version() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    conn.raw(b"{\"v\":2,\"type\":\"status\"}\n").await;
    let message = conn.expect_error_then_close().await;
    assert!(message.contains("version"), "{message}");
    wait_for_event(&p, is_rejected).await;
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_garbage_json() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    conn.raw(b"{this is not json\n").await;
    conn.expect_error_then_close().await;
    wait_for_event(&p, is_rejected).await;
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_open_for_another_pid() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    conn.send(&open_msg(1)).await;
    conn.expect_error_then_close().await;
    wait_for_event(&p, is_rejected).await;
    assert_eq!(Conn::new(&p.socket).await.status().await.sessions.len(), 0);
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn second_daemon_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let second = spawn(&p, Options::default());
    let err = timeout(Duration::from_secs(5), second.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("damperd is already running"),
        "{err:#}"
    );
    // The first daemon is untouched.
    assert!(Conn::new(&p.socket).await.status().await.limit >= 1);
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn session_cap_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start_with(
        &p,
        Options {
            max_sessions: 1,
            ..Options::default()
        },
    )
    .await;
    let mut first = Conn::new(&p.socket).await;
    open_session(&mut first).await;
    let mut second = Conn::new(&p.socket).await;
    second.send(&open_msg(std::process::id())).await;
    let message = second.expect_error_then_close().await;
    assert!(message.contains("too many"), "{message}");
    // The first session is unaffected.
    assert_eq!(Conn::new(&p.socket).await.status().await.sessions.len(), 1);
    wait_for_event(&p, is_rejected).await;
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_keeps_live_fifos() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    // The session's shim is this test process, alive throughout.
    let fifo = open_session(&mut conn).await;
    d.stop().await.unwrap();
    assert!(fifo.exists(), "shutdown leaves FIFOs in place");
    // The old daemon's connection is gone: the shim would now self-feed.
    assert_eq!(conn.recv().await, None);

    // A FIFO left by a shim that has since exited.
    let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    child.wait().unwrap();
    let stale = p.run.join(format!("s-{}-0-1.fifo", child.id()));
    nix::unistd::mkfifo(
        &stale,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();

    let d = start(&p).await;
    assert!(fifo.exists(), "a new daemon keeps a live shim's FIFO");
    assert!(!stale.exists(), "a new daemon sweeps a dead shim's FIFO");
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_removes_socket() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    assert!(p.socket.exists());
    d.stop().await.unwrap();
    assert!(!p.socket.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn explain_returns_the_sessions_story() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    conn.send(&open_msg(std::process::id())).await;
    let Some(ServerMsg::Opened { session, .. }) = conn.recv().await else {
        panic!("expected opened");
    };
    wait_for_event(&p, |k| matches!(k, EventKind::SessionOpened { .. })).await;
    let mut cli = Conn::new(&p.socket).await;
    cli.send(&ClientMsg::Explain {
        session: session.clone(),
    })
    .await;
    let Some(ServerMsg::Events { events, .. }) = cli.recv().await else {
        panic!("expected events");
    };
    assert_ne!(events.len(), 0);
    assert!(
        events
            .iter()
            .all(|e| matches!(e.kind, EventKind::LimitChanged { .. })
                || e.kind.session() == Some(&session))
    );
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn fifo_failure_closes_the_session_and_keeps_serving() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    damper_proto::paths::prepare_dirs(&p).unwrap();
    // Occupy the FIFO paths the first session can get (its id starts with
    // the daemon's start time in hex), so creating the FIFO fails.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let blockers: Vec<PathBuf> = (0..5)
        .map(|k| {
            p.run
                .join(format!("s-{}-{:x}-1.fifo", std::process::id(), secs + k))
        })
        .collect();
    for b in &blockers {
        std::fs::write(b, b"").unwrap();
    }
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    conn.send(&open_msg(std::process::id())).await;
    conn.expect_error_then_close().await;
    wait_for_event(
        &p,
        |k| matches!(k, EventKind::Error { message } if message.contains("FIFO")),
    )
    .await;
    wait_for_event(&p, |k| matches!(k, EventKind::SessionClosed { .. })).await;
    assert_eq!(Conn::new(&p.socket).await.status().await.sessions.len(), 0);
    // Someone else's file at that path is never removed.
    assert!(blockers.iter().all(|b| b.exists()));
    d.stop().await.unwrap();
}
