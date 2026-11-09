//! The daemon end to end over its socket, with fake system sources.

#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test")]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use damper_core::broker::MemorySample;
use damper_proto::paths::{Paths, resolve_paths_from};
use damper_proto::{ClientMsg, EventKind, MAX_LINE, Pressure, ServerMsg, decode, encode};
use damperd::runtime::{Options, run_until};
use damperd::sources::{MemorySource, ProcessSource, Snapshot};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

struct FakeMemory;

impl MemorySource for FakeMemory {
    fn sample(&self) -> io::Result<MemorySample> {
        Ok(MemorySample {
            pressure: Pressure::Normal,
            available_pct: 80,
            physical: 16 << 30,
            swap_used: 0,
        })
    }
}

struct FakeProcs;

impl ProcessSource for FakeProcs {
    fn snapshot(&self) -> io::Result<Snapshot> {
        Ok(Snapshot::default())
    }
}

struct Daemon {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Daemon {
    async fn stop(mut self) -> anyhow::Result<()> {
        self.stop.take().unwrap().send(()).unwrap();
        timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
    }
}

fn paths(dir: &TempDir) -> Paths {
    let mut p = resolve_paths_from(Some(&dir.path().join("state"))).unwrap();
    // Never read the developer's own config.
    p.config = dir.path().join("config.toml");
    p
}

fn spawn(p: &Paths, opts: Options) -> Daemon {
    let (stop, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(run_until(
        p.clone(),
        Arc::new(FakeMemory),
        Arc::new(FakeProcs),
        opts,
        async {
            let _ = rx.await;
        },
    ));
    Daemon {
        stop: Some(stop),
        task,
    }
}

async fn start(p: &Paths) -> Daemon {
    start_with(p, Options::default()).await
}

async fn start_with(p: &Paths, opts: Options) -> Daemon {
    let d = spawn(p, opts);
    for _ in 0..200 {
        if UnixStream::connect(&p.socket).await.is_ok() {
            return d;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon did not start listening");
}

struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Conn {
    async fn new(socket: &Path) -> Self {
        let (r, w) = UnixStream::connect(socket).await.unwrap().into_split();
        Self {
            reader: BufReader::new(r),
            writer: w,
        }
    }

    async fn raw(&mut self, bytes: &[u8]) {
        // The daemon may close the connection while a large line is in flight.
        let _ = self.writer.write_all(bytes).await;
    }

    async fn send(&mut self, msg: &ClientMsg) {
        self.raw(&encode(msg).unwrap()).await;
    }

    /// The next message, or `None` once the daemon closes the connection.
    async fn recv(&mut self) -> Option<ServerMsg> {
        let mut line = Vec::new();
        let n = timeout(
            Duration::from_secs(5),
            self.reader.read_until(b'\n', &mut line),
        )
        .await
        .expect("daemon answered in time")
        .unwrap_or(0);
        (n > 0).then(|| decode(&line).unwrap())
    }

    async fn status(&mut self) -> damper_proto::Status {
        self.send(&ClientMsg::Status).await;
        match self.recv().await {
            Some(ServerMsg::Status(s)) => s,
            other => panic!("expected status, got {other:?}"),
        }
    }

    async fn expect_error_then_close(&mut self) -> String {
        let message = match self.recv().await {
            Some(ServerMsg::Error { message }) => message,
            other => panic!("expected error, got {other:?}"),
        };
        assert_eq!(self.recv().await, None, "connection closed after the error");
        message
    }
}

fn open_msg(shim_pid: u32) -> ClientMsg {
    ClientMsg::Open {
        cwd: PathBuf::from("/src/app"),
        subcommand: "build".to_owned(),
        shim_pid,
        parent_pid: 1,
        nested: false,
    }
}

/// Opens a session as this process and returns the FIFO path.
async fn open_session(conn: &mut Conn) -> PathBuf {
    conn.send(&open_msg(std::process::id())).await;
    let fifo = match conn.recv().await {
        Some(ServerMsg::Opened { fifo, .. }) => fifo,
        other => panic!("expected opened, got {other:?}"),
    };
    match conn.recv().await {
        Some(ServerMsg::Granted { threads, .. }) => assert!(threads >= 1),
        other => panic!("expected granted, got {other:?}"),
    }
    fifo
}

/// Waits until the event log holds an event matching `pred`.
async fn wait_for_event(p: &Paths, pred: impl Fn(&EventKind) -> bool) {
    for _ in 0..200 {
        let mut conn = Conn::new(&p.socket).await;
        conn.send(&ClientMsg::Log {
            since_ms: None,
            session: None,
        })
        .await;
        if let Some(ServerMsg::Events { events }) = conn.recv().await
            && events.iter().any(|e| pred(&e.kind))
        {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("event never logged");
}

fn is_rejected(kind: &EventKind) -> bool {
    matches!(kind, EventKind::ClientRejected { .. })
}

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
    let d = start_with(&p, Options { max_sessions: 1 }).await;
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

    let d = start(&p).await;
    assert!(fifo.exists(), "a new daemon keeps a live shim's FIFO");
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
    let Some(ServerMsg::Events { events }) = cli.recv().await else {
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
