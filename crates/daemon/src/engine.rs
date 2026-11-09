//! The broker task: owns the [`Broker`], the session FIFOs and each session's
//! outgoing queue; ticks once a second and carries out the broker's actions.
//!
//! Nothing here waits on a client: messages go out with `try_send`, and a
//! client whose queue is full loses its connection. A failing action is
//! logged as `Error` and never stops the daemon.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::bail;
use damper_core::broker::{Action, Broker, MemorySample, TickInput};
use damper_core::config::{Loaded, Machine, load};
use damper_proto::{EventKind, ServerMsg, SessionId};
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::engine::fallbacks::Fallbacks;
use crate::fifo::SessionFifo;
use crate::pidwatch::PidWatcher;
use crate::server::{LogTx, Request};
use crate::sources::{MemorySource, ProcessSource, Snapshot};

pub(crate) mod fallbacks;
mod liveness;
#[cfg(test)]
mod tests;

/// The broker's sampling and policy interval.
pub const TICK: Duration = Duration::from_secs(1);

/// The config file as last seen: its mtime (`None` while it is missing).
#[derive(Debug, Clone)]
pub(crate) struct ConfigWatch {
    pub(crate) path: PathBuf,
    pub(crate) machine: Machine,
    pub(crate) mtime: Option<SystemTime>,
}

impl ConfigWatch {
    /// Loads the config now and remembers its mtime. Blocking.
    pub(crate) fn load_now(path: PathBuf, machine: Machine) -> (Self, Loaded) {
        let mtime = mtime(&path);
        let loaded = load_config(read_config(&path), machine);
        (
            Self {
                path,
                machine,
                mtime,
            },
            loaded,
        )
    }
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn read_config(path: &std::path::Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Resolves the config text; an unreadable file means defaults and a warning.
fn load_config(text: io::Result<Option<String>>, machine: Machine) -> Loaded {
    match text {
        Ok(text) => load(text.as_deref(), machine),
        Err(e) => {
            let mut loaded = load(None, machine);
            loaded
                .warnings
                .push(format!("config file unreadable ({e}); using defaults"));
            loaded
        }
    }
}

/// One tick's blocking reads.
struct Samples {
    memory: io::Result<MemorySample>,
    snapshot: io::Result<Snapshot>,
    /// The config's new mtime and text, when the mtime changed.
    config: Option<(Option<SystemTime>, io::Result<Option<String>>)>,
    /// How many shim fallbacks were moved into the log, when due this tick.
    ingested: Option<io::Result<u32>>,
}

/// The broker task's state.
pub(crate) struct Engine {
    pub(crate) broker: Broker,
    pub(crate) fifos: HashMap<SessionId, SessionFifo>,
    pub(crate) clients: HashMap<SessionId, mpsc::Sender<ServerMsg>>,
    pub(crate) watcher: PidWatcher,
    pub(crate) log: LogTx,
    pub(crate) run_dir: PathBuf,
    pub(crate) memory: Arc<dyn MemorySource>,
    pub(crate) procs: Arc<dyn ProcessSource>,
    pub(crate) config: ConfigWatch,
    /// Whether the last tick's memory or process read failed, so a lasting
    /// failure is logged once rather than every second.
    pub(crate) memory_failing: bool,
    pub(crate) procs_failing: bool,
    /// Sessions whose FIFO could not be read last tick, so a lasting failure
    /// is logged once.
    pub(crate) fill_failing: HashSet<SessionId>,
    /// Pids the watcher refused, checked for exit every tick instead.
    pub(crate) polled: HashSet<u32>,
    /// The shims' fallback file, moved into the log every few ticks.
    pub(crate) fallbacks: Fallbacks,
}

impl Engine {
    fn log(&self, kind: EventKind) {
        // The writer is gone only during shutdown, when nothing more is logged.
        let _ = self.log.send(kind);
    }

    fn error(&self, message: String) {
        self.log(EventKind::Error { message });
    }

    /// Serves requests, ticks and pid exits until the request channel closes.
    ///
    /// # Errors
    /// The pid watcher stopped: sessions could no longer close, so the daemon
    /// must exit (shims then feed their own FIFOs).
    pub(crate) async fn run(
        mut self,
        mut requests: mpsc::Receiver<Request>,
        mut exits: mpsc::Receiver<u32>,
    ) -> anyhow::Result<()> {
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => self.tick().await,
                request = requests.recv() => match request {
                    Some(request) => self.handle(request),
                    None => return Ok(()),
                },
                exit = exits.recv() => {
                    let Some(pid) = exit else {
                        self.error("the pid watcher stopped; shutting down".to_owned());
                        bail!("the pid watcher stopped");
                    };
                    let actions = self.broker.pid_exited(pid, Instant::now());
                    self.apply(actions);
                }
            }
        }
    }

    /// Samples the sources and the config, then runs one broker tick.
    pub(crate) async fn tick(&mut self) {
        let memory = Arc::clone(&self.memory);
        let procs = Arc::clone(&self.procs);
        let path = self.config.path.clone();
        let last = self.config.mtime;
        let ingest = self.fallbacks.job();
        let samples = tokio::task::spawn_blocking(move || {
            let now = mtime(&path);
            Samples {
                memory: memory.sample(),
                snapshot: procs.snapshot(),
                config: (now != last).then(|| (now, read_config(&path))),
                ingested: ingest.map(|ingest| ingest()),
            }
        })
        .await;
        let samples = match samples {
            Ok(samples) => samples,
            Err(e) => return self.error(format!("sampling task failed: {e}")),
        };
        if let Some(message) = samples.ingested.and_then(|r| self.fallbacks.outcome(r)) {
            self.error(message);
        }
        if let Some((mtime, text)) = samples.config {
            self.config.mtime = mtime;
            let loaded = load_config(text, self.config.machine);
            let actions = self.broker.reconfigure(loaded.effective, loaded.warnings);
            self.apply(actions);
        }
        for pid in liveness::take_exited(&mut self.polled) {
            let actions = self.broker.pid_exited(pid, Instant::now());
            self.apply(actions);
        }
        let snapshot = match samples.snapshot {
            Ok(snapshot) => {
                self.procs_failing = false;
                snapshot
            }
            Err(e) => {
                if !std::mem::replace(&mut self.procs_failing, true) {
                    self.error(format!("reading the process table: {e}"));
                }
                Snapshot::default()
            }
        };
        let memory = match samples.memory {
            Ok(memory) => {
                self.memory_failing = false;
                memory
            }
            Err(e) => {
                // Without memory there is no decision: skip the tick. Shims
                // miss their heartbeat and feed themselves.
                if !std::mem::replace(&mut self.memory_failing, true) {
                    self.error(format!("reading memory state: {e}"));
                }
                return;
            }
        };
        let fifo_fill = self.read_fills();
        let input = TickInput {
            now: Instant::now(),
            memory,
            procs: snapshot.procs,
            fifo_fill,
            sccache_servers: snapshot.sccache_servers,
        };
        let actions = self.broker.tick(&input);
        self.apply(actions);
    }

    /// Each FIFO's fill. A FIFO that can't be read is left out of this tick;
    /// the first failure of a run of them is logged.
    fn read_fills(&mut self) -> HashMap<SessionId, u32> {
        let mut fills = HashMap::new();
        let mut newly_failing = Vec::new();
        for (id, fifo) in &self.fifos {
            match fifo.fill() {
                Ok(n) => {
                    self.fill_failing.remove(id);
                    fills.insert(id.clone(), n);
                }
                Err(e) => {
                    if self.fill_failing.insert(id.clone()) {
                        newly_failing.push(format!("reading the fill of session {id}'s FIFO: {e}"));
                    }
                }
            }
        }
        for message in newly_failing {
            self.error(message);
        }
        fills
    }

    fn handle(&mut self, request: Request) {
        let now = Instant::now();
        match request {
            Request::Open { req, tx, reply } => match self.broker.open(now, req) {
                Ok((session, actions)) => {
                    self.clients.insert(session.clone(), tx);
                    self.apply(actions);
                    // A connection that went away is closed by its task.
                    let _ = reply.send(Ok(session));
                }
                Err(e) => {
                    let reason = e.to_string();
                    self.log(EventKind::ClientRejected {
                        reason: reason.clone(),
                    });
                    let _ = reply.send(Err(reason));
                }
            },
            Request::Started { session, cargo_pid } => {
                let actions = self.broker.started(&session, cargo_pid);
                self.apply(actions);
            }
            Request::SelfFed { session, tokens } => {
                let actions = self.broker.self_fed(&session, tokens);
                self.apply(actions);
            }
            Request::Closed { session } => {
                self.clients.remove(&session);
                let actions = self.broker.connection_closed(&session, now);
                self.apply(actions);
            }
            Request::Status { reply } => {
                let _ = reply.send(self.broker.status(now));
            }
        }
    }

    /// Carries out `actions` and every action they lead to, in order.
    pub(crate) fn apply(&mut self, actions: Vec<Action>) {
        let mut queue = VecDeque::from(actions);
        while let Some(action) = queue.pop_front() {
            queue.extend(self.apply_one(action));
        }
    }

    fn apply_one(&mut self, action: Action) -> Vec<Action> {
        match action {
            Action::CreateFifo { session, shim_pid } => self.create_fifo(&session, shim_pid),
            Action::Feed { session, tokens } => {
                let actual = self.transfer(&session, tokens, "feeding", SessionFifo::feed);
                self.broker.feed_result(&session, tokens, actual);
                Vec::new()
            }
            Action::Drain { session, tokens } => {
                let actual = self.transfer(&session, tokens, "draining", SessionFifo::drain);
                self.broker.drain_result(&session, tokens, actual);
                Vec::new()
            }
            Action::RemoveFifo { session } => {
                self.clients.remove(&session);
                self.fill_failing.remove(&session);
                if let Some(fifo) = self.fifos.remove(&session) {
                    match fifo.remove() {
                        Err(e) if e.kind() != io::ErrorKind::NotFound => {
                            self.error(format!("removing the FIFO of session {session}: {e}"));
                        }
                        _ => {}
                    }
                }
                Vec::new()
            }
            Action::Send { session, msg } => {
                self.send(&session, msg);
                Vec::new()
            }
            Action::WatchPid { pid } => {
                if let Err(e) = self.watcher.watch(pid) {
                    self.error(format!(
                        "watching pid {pid}: {e}; checking it every tick instead"
                    ));
                    self.polled.insert(pid);
                }
                Vec::new()
            }
            Action::Log(kind) => {
                self.log(kind);
                Vec::new()
            }
        }
    }

    /// Creates the FIFO and reports it ready; on failure the session closes
    /// at once and its shim is told to fall back. Both happen before any
    /// other request is served, so a FIFO cannot outlive its session.
    fn create_fifo(&mut self, session: &SessionId, shim_pid: u32) -> Vec<Action> {
        match SessionFifo::create(&self.run_dir, shim_pid, session) {
            Ok(fifo) => {
                let path = fifo.path().to_path_buf();
                self.fifos.insert(session.clone(), fifo);
                self.broker.fifo_ready(session, path, Instant::now())
            }
            Err(e) => {
                self.error(format!("creating the FIFO of session {session}: {e}"));
                if let Some(tx) = self.clients.remove(session) {
                    // Dropping `tx` closes the connection after this line.
                    let _ = tx.try_send(ServerMsg::Error {
                        message: "the daemon could not create the session FIFO".to_owned(),
                    });
                }
                self.broker.connection_closed(session, Instant::now())
            }
        }
    }

    /// Feeds or drains up to `tokens`; returns how many moved (0 on error).
    fn transfer(
        &self,
        session: &SessionId,
        tokens: u32,
        what: &str,
        op: fn(&SessionFifo, u32) -> io::Result<u32>,
    ) -> u32 {
        let Some(fifo) = self.fifos.get(session) else {
            return 0;
        };
        match op(fifo, tokens) {
            Ok(n) => n,
            Err(e) => {
                self.error(format!("{what} the FIFO of session {session}: {e}"));
                0
            }
        }
    }

    fn send(&mut self, session: &SessionId, msg: ServerMsg) {
        let Some(tx) = self.clients.get(session) else {
            return;
        };
        match tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.error(format!(
                    "session {session}'s client is not reading; closing its connection"
                ));
                self.clients.remove(session);
            }
            Err(TrySendError::Closed(_)) => {
                self.clients.remove(session);
            }
        }
    }
}
