//! Daemon start-up, the accept loop and shutdown.
//!
//! Start-up order: prepare the directories; take the lock (a second daemon
//! exits with "damperd is already running"); sweep FIFOs of dead shims; open
//! the event log and ingest the shims' fallback records; load the config;
//! remove a leftover socket (safe under the lock); bind it with mode 0600;
//! spawn the pid watcher; tick the broker once, so its start-up events come
//! first; then serve.
//!
//! Shutdown stops the broker first, so no closing connection can remove a
//! FIFO, then drops every connection (shims feed their own FIFOs), removes
//! the socket and leaves the FIFOs in place for the shims still using them.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use damper_core::broker::{Broker, MAX_SESSIONS};
use damper_core::config::Machine;
use damper_proto::paths::{Paths, prepare_dirs};
use damper_proto::{EventKind, MAX_REPLY};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::libc;
use nix::unistd::getuid;
use tokio::net::UnixListener;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::engine::fallbacks::Fallbacks;
use crate::engine::{ConfigWatch, Engine};
use crate::eventlog::EventLog;
use crate::fifo::sweep_stale_fifos;
use crate::pidwatch::PidWatcher;
use crate::server::{FIRST_LINE_TIMEOUT, Shared, serve};
use crate::sources::{MemorySource, ProcessSource};

/// Connections served at once; beyond that a connection is closed at once
/// and logged. Room for every session plus CLI clients.
const MAX_CONNECTIONS: usize = MAX_SESSIONS * 2;
/// The pause after a failed `accept`.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// Requests queued for the broker task.
const REQUEST_QUEUE: usize = 256;
/// The shims' fallback file, in the log directory.
const FALLBACKS: &str = "fallbacks.jsonl";

/// Settings for [`run_until`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// The session cap.
    pub max_sessions: usize,
    /// The longest `Status` or `Events` reply line.
    pub max_reply: usize,
    /// How long a new connection may take to send its first line.
    pub first_line_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_sessions: MAX_SESSIONS,
            max_reply: MAX_REPLY,
            first_line_timeout: FIRST_LINE_TIMEOUT,
        }
    }
}

/// Runs the daemon until SIGTERM or SIGINT.
///
/// # Errors
/// See [`run_until`]; also failure to install the signal handlers.
pub async fn run(
    paths: Paths,
    mem: Arc<dyn MemorySource>,
    procs: Arc<dyn ProcessSource>,
) -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
    let mut int = signal(SignalKind::interrupt()).context("installing the SIGINT handler")?;
    let shutdown = async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    };
    run_until(paths, mem, procs, Options::default(), shutdown).await
}

/// Writes a line to stderr, for what cannot go to the event log.
fn report(message: &str) {
    // stderr is the last resort: if it fails too, there is nowhere left.
    let _ = writeln!(io::stderr().lock(), "damperd: {message}");
}

fn lock(path: &Path) -> anyhow::Result<Flock<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening the lock file {}", path.display()))?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(lock),
        Err((_, Errno::EWOULDBLOCK)) => bail!("damperd is already running"),
        Err((_, e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

/// Removes a socket left by a daemon that did not shut down cleanly.
/// Anything else at that path is left alone and stops the start.
fn remove_stale_socket(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => fs::remove_file(path)
            .with_context(|| format!("removing the stale socket {}", path.display())),
        Ok(_) => bail!("{} exists and is not a socket", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("checking {}", path.display())),
    }
}

/// Logical cores, as `std::thread::available_parallelism` reports them
/// (`hw.logicalcpu` on macOS): the config's `cores` counts logical cores.
fn machine_cores() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX))
}

/// Starts the daemon and serves until `shutdown` completes.
///
/// # Errors
/// Any start-up step failing (another daemon holds the lock, an unsafe state
/// directory, a socket that cannot be bound), or the pid watcher stopping.
pub async fn run_until(
    paths: Paths,
    mem: Arc<dyn MemorySource>,
    procs: Arc<dyn ProcessSource>,
    opts: Options,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    prepare_dirs(&paths).context("preparing the state directory")?;
    let _lock = lock(&paths.lock)?;
    let sweep = sweep_stale_fifos(&paths.run).context("sweeping stale FIFOs")?;

    let mut eventlog = EventLog::open(&paths.log).context("opening the event log")?;
    let (log, log_rx) = mpsc::unbounded_channel();
    for (path, e) in sweep.failed {
        let message = format!("removing stale FIFO {}: {e}", path.display());
        log.send(EventKind::Error { message }).ok();
    }
    if let Err(e) = eventlog.ingest_fallbacks(&paths.log.join(FALLBACKS)) {
        let message = format!("ingesting shim fallbacks: {e}");
        log.send(EventKind::Error { message }).ok();
    }
    let events = Arc::new(Mutex::new(eventlog));
    let writer = tokio::task::spawn_blocking({
        let events = Arc::clone(&events);
        move || write_events(&events, log_rx)
    });

    let served = serve_all(&paths, mem, procs, opts, shutdown, log, &events).await;
    // Every log sender is gone now: the writer drains and stops.
    if let Err(e) = writer.await {
        report(&format!("event log writer failed: {e}"));
    }
    served
}

/// Removes the bound socket when dropped, however serving ends; the FIFOs
/// stay.
struct BoundSocket<'a>(&'a Path);

impl Drop for BoundSocket<'_> {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(self.0)
            && e.kind() != io::ErrorKind::NotFound
        {
            report(&format!("removing the socket: {e}"));
        }
    }
}

/// Appends every event sent until all senders are dropped.
fn write_events(events: &Mutex<EventLog>, mut rx: mpsc::UnboundedReceiver<EventKind>) {
    while let Some(kind) = rx.blocking_recv() {
        let result = events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .append(kind);
        if let Err(e) = result {
            report(&format!("writing the event log: {e}"));
        }
    }
}

/// Logs a connection task that panicked; one that returned needs nothing.
fn note_ended(shared: &Shared, joined: Result<(), tokio::task::JoinError>) {
    if let Err(e) = joined
        && e.is_panic()
    {
        shared.log(EventKind::Error {
            message: "a connection task panicked".to_owned(),
        });
    }
}

async fn serve_all(
    paths: &Paths,
    memory: Arc<dyn MemorySource>,
    procs: Arc<dyn ProcessSource>,
    opts: Options,
    shutdown: impl Future<Output = ()>,
    log: mpsc::UnboundedSender<EventKind>,
    events: &Arc<Mutex<EventLog>>,
) -> anyhow::Result<()> {
    let sampler = Arc::clone(&memory);
    let sample = tokio::task::spawn_blocking(move || sampler.sample())
        .await
        .context("sampling memory")?
        .context("reading the machine's memory")?;
    let machine = Machine {
        cores: machine_cores(),
        memory: sample.physical,
    };
    let config_path = paths.config.clone();
    let (config, loaded) =
        tokio::task::spawn_blocking(move || ConfigWatch::load_now(config_path, machine))
            .await
            .context("loading the config")?;

    remove_stale_socket(&paths.socket)?;
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("binding {}", paths.socket.display()))?;
    let _bound = BoundSocket(&paths.socket);
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))
        .context("setting the socket's mode")?;

    let (exit_tx, exits) = mpsc::channel(REQUEST_QUEUE);
    let watcher = PidWatcher::spawn(exit_tx).context("starting the pid watcher")?;
    let start_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let broker = Broker::new(
        loaded.effective,
        loaded.warnings,
        start_unix_secs,
        Instant::now(),
    )
    .with_max_sessions(opts.max_sessions);
    let mut engine = Engine {
        broker,
        fifos: HashMap::new(),
        clients: HashMap::new(),
        watcher,
        log: log.clone(),
        run_dir: paths.run.clone(),
        memory,
        procs,
        config,
        memory_failing: false,
        procs_failing: false,
        fill_failing: HashSet::new(),
        polled: HashSet::new(),
        fallbacks: Fallbacks::new(Arc::clone(events), paths.log.join(FALLBACKS)),
    };
    engine.tick().await;

    let (requests, requests_rx) = mpsc::channel(REQUEST_QUEUE);
    let shared = Arc::new(Shared {
        requests,
        log,
        events: Arc::clone(events),
        own_uid: getuid().as_raw(),
        max_reply: opts.max_reply,
        first_line_timeout: opts.first_line_timeout,
    });
    let mut broker_task = Box::pin(engine.run(requests_rx, exits));
    let mut shutdown = std::pin::pin!(shutdown);
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut conns = JoinSet::new();
    let result = loop {
        tokio::select! {
            () = &mut shutdown => break Ok(()),
            result = &mut broker_task => break result,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => match Arc::clone(&permits).try_acquire_owned() {
                    Ok(permit) => {
                        let shared = Arc::clone(&shared);
                        conns.spawn(async move {
                            serve(stream, shared).await;
                            drop(permit);
                        });
                    }
                    Err(_) => shared.log(EventKind::ClientRejected {
                        reason: format!("more than {MAX_CONNECTIONS} connections"),
                    }),
                },
                Err(e) => {
                    shared.log(EventKind::Error {
                        message: format!("accepting a connection: {e}"),
                    });
                    // Out of descriptors, say: retrying at once would spin.
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
            },
            Some(joined) = conns.join_next(), if !conns.is_empty() => note_ended(&shared, joined),
        }
    };
    // Broker first: from here no closed connection can remove a FIFO.
    drop(broker_task);
    conns.shutdown().await;
    drop(listener);
    result
}
