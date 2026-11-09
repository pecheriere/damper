//! The daemon end to end over its socket, with fake system sources.

#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test")]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use damper_core::broker::MemorySample;
use damper_proto::paths::{Paths, resolve_paths_from};
use damper_proto::{
    ClientMsg, EventKind, MAX_LINE, MAX_REPLY, Pressure, ServerMsg, decode_with_limit, encode,
};
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
        (n > 0).then(|| decode_with_limit(&line, MAX_REPLY).unwrap())
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
        if let Some(ServerMsg::Events { events, .. }) = conn.recv().await
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

mod basics;
mod limits;
