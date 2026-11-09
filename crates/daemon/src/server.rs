//! One client connection: peer checks, line framing and request dispatch.
//!
//! Every connection runs in its own task. A session connection forwards
//! `Open`/`Started` to the broker task and writes what the broker sends it
//! through a bounded queue; the broker never waits on a client. Protocol
//! errors (a line over [`MAX_LINE`], bad JSON, a wrong version, a message out
//! of order) get one `Error` line, a `ClientRejected` event, and the
//! connection is closed. Nothing else is affected.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use damper_core::broker::OpenRequest;
use damper_core::classify::Kind;
use damper_proto::{
    ClientMsg, Event, EventKind, MAX_LINE, ServerMsg, SessionId, Status, decode, explain_events,
};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot};

use crate::eventlog::EventLog;
use crate::procs::{parent_pid, process_name};

mod checks;
mod wire;

pub use checks::peer_allowed;
use checks::{decode_error_reason, never_hold, started_allowed};
use wire::{LineError, next_line, write_msg};

/// How long a new connection has to send its first complete line.
pub const FIRST_LINE_TIMEOUT: Duration = Duration::from_secs(5);

/// Messages queued for one session's client; a client that lets this many
/// pile up is not reading and is disconnected.
pub(crate) const CLIENT_QUEUE: usize = 64;

/// Sends events to the event-log writer.
pub(crate) type LogTx = mpsc::UnboundedSender<EventKind>;

/// What a connection asks of the broker task.
#[derive(Debug)]
pub(crate) enum Request {
    /// Open a session; the broker's messages go to `tx`.
    Open {
        req: OpenRequest,
        tx: mpsc::Sender<ServerMsg>,
        reply: oneshot::Sender<Result<SessionId, String>>,
    },
    /// The session's cargo started.
    Started { session: SessionId, cargo_pid: u32 },
    /// The session's connection closed.
    Closed { session: SessionId },
    /// The current status.
    Status { reply: oneshot::Sender<Status> },
}

/// What every connection task shares.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) requests: mpsc::Sender<Request>,
    pub(crate) log: LogTx,
    pub(crate) events: Arc<Mutex<EventLog>>,
    pub(crate) own_uid: u32,
    /// The longest `Status` or `Events` reply line.
    pub(crate) max_reply: usize,
    /// How long a connection may stay silent before its first line.
    pub(crate) first_line_timeout: Duration,
}

impl Shared {
    pub(crate) fn log(&self, kind: EventKind) {
        // The writer is gone only during shutdown, when nothing more is logged.
        let _ = self.log.send(kind);
    }

    fn reject(&self, reason: String) {
        self.log(EventKind::ClientRejected { reason });
    }
}

/// Serves one accepted connection until it closes or breaks the protocol.
pub(crate) async fn serve(stream: UnixStream, shared: Arc<Shared>) {
    let cred = match stream.peer_cred() {
        Ok(cred) => cred,
        Err(e) => {
            shared.reject(format!("peer credentials unreadable: {e}"));
            return;
        }
    };
    if !peer_allowed(cred.uid(), shared.own_uid) {
        shared.reject(format!("connection from uid {}", cred.uid()));
        return;
    }
    let peer_pid = cred.pid().and_then(|p| u32::try_from(p).ok());
    let (r, w) = stream.into_split();
    let mut conn = Conn {
        reader: BufReader::new(r),
        writer: w,
        buf: Vec::new(),
        greeted: false,
        session: None,
        peer_pid,
        shared,
    };
    conn.run().await;
    if let Some((session, _)) = conn.session {
        // The broker task is gone only during shutdown.
        let _ = conn.shared.requests.send(Request::Closed { session }).await;
    }
}

struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: OwnedWriteHalf,
    buf: Vec<u8>,
    /// Whether the client has sent a complete line yet.
    greeted: bool,
    session: Option<(SessionId, mpsc::Receiver<ServerMsg>)>,
    peer_pid: Option<u32>,
    shared: Arc<Shared>,
}

/// What to do after handling a message.
enum Next {
    Continue,
    Close,
}

impl Conn {
    async fn run(&mut self) {
        loop {
            let line = if let Some((_, rx)) = self.session.as_mut() {
                tokio::select! {
                    msg = rx.recv() => {
                        let Some(msg) = msg else { return };
                        if self.write(msg).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    line = next_line(&mut self.reader, &mut self.buf) => line,
                }
            } else if self.greeted {
                next_line(&mut self.reader, &mut self.buf).await
            } else {
                let timeout = self.shared.first_line_timeout;
                let first = next_line(&mut self.reader, &mut self.buf);
                let Ok(line) = tokio::time::timeout(timeout, first).await else {
                    return self
                        .reject(format!(
                            "no complete request within {} ms",
                            timeout.as_millis()
                        ))
                        .await;
                };
                line
            };
            match line {
                Ok(true) => self.greeted = true,
                Ok(false) | Err(LineError::Io) => return,
                Err(LineError::TooLong) => {
                    return self
                        .reject(format!("protocol line longer than {MAX_LINE} bytes"))
                        .await;
                }
            }
            let msg = match decode::<ClientMsg>(&self.buf) {
                Ok(msg) => msg,
                Err(e) => return self.reject(decode_error_reason(&e)).await,
            };
            if matches!(self.handle(msg).await, Next::Close) {
                return;
            }
        }
    }

    /// Sends `Error`, logs `ClientRejected`; the caller then closes.
    async fn reject(&mut self, reason: String) {
        // The connection closes next whether or not the client got the line.
        let _ = self
            .write(ServerMsg::Error {
                message: reason.clone(),
            })
            .await;
        self.shared.reject(reason);
    }

    async fn write(&mut self, msg: ServerMsg) -> Result<(), ()> {
        write_msg(&mut self.writer, msg, self.shared.max_reply).await
    }

    async fn reply(&mut self, msg: ServerMsg) -> Next {
        match self.write(msg).await {
            Ok(()) => Next::Continue,
            Err(()) => Next::Close,
        }
    }

    async fn handle(&mut self, msg: ClientMsg) -> Next {
        match msg {
            ClientMsg::Status => {
                let (reply, rx) = oneshot::channel();
                if self
                    .shared
                    .requests
                    .send(Request::Status { reply })
                    .await
                    .is_err()
                {
                    return Next::Close;
                }
                match rx.await {
                    Ok(status) => self.reply(ServerMsg::Status(status)).await,
                    Err(_) => Next::Close,
                }
            }
            ClientMsg::Log { since_ms, session } => {
                let events = self.read_events(since_ms).await.map(|all| match session {
                    Some(s) => all
                        .into_iter()
                        .filter(|e| e.kind.session() == Some(&s))
                        .collect(),
                    None => all,
                });
                self.reply_events(events).await
            }
            ClientMsg::Explain { session } => {
                let events = self
                    .read_events(None)
                    .await
                    .map(|all| explain_events(&all, &session));
                self.reply_events(events).await
            }
            ClientMsg::Open {
                cwd,
                subcommand,
                shim_pid,
                // The client's word is not trusted for `never_hold`: the
                // daemon looks up the connecting shim's real parent.
                parent_pid: _,
                nested,
            } => {
                if self.session.is_some() {
                    self.reject("a session is already open on this connection".to_owned())
                        .await;
                    return Next::Close;
                }
                if self.peer_pid != Some(shim_pid) {
                    self.reject("shim_pid is not the connecting process".to_owned())
                        .await;
                    return Next::Close;
                }
                let req = OpenRequest {
                    cwd,
                    kind: Kind::from_subcommand(&subcommand),
                    subcommand,
                    shim_pid,
                    never_hold: never_hold(self.shim_parent_name().as_deref()),
                    nested,
                };
                self.open(req).await
            }
            ClientMsg::Started { cargo_pid } => {
                let Some((session, _)) = &self.session else {
                    self.reject("started before open".to_owned()).await;
                    return Next::Close;
                };
                let session = session.clone();
                // An open session means the peer pid was read and checked.
                let shim_pid = self.peer_pid.unwrap_or_default();
                if !started_allowed(parent_pid(cargo_pid), shim_pid) {
                    self.reject("cargo_pid is not a child of the shim".to_owned())
                        .await;
                    return Next::Close;
                }
                match self
                    .shared
                    .requests
                    .send(Request::Started { session, cargo_pid })
                    .await
                {
                    Ok(()) => Next::Continue,
                    Err(_) => Next::Close,
                }
            }
        }
    }

    async fn open(&mut self, req: OpenRequest) -> Next {
        let (tx, rx) = mpsc::channel(CLIENT_QUEUE);
        let (reply, answer) = oneshot::channel();
        if self
            .shared
            .requests
            .send(Request::Open { req, tx, reply })
            .await
            .is_err()
        {
            return Next::Close;
        }
        match answer.await {
            Ok(Ok(session)) => {
                self.session = Some((session, rx));
                Next::Continue
            }
            Ok(Err(message)) => {
                // The broker already logged why; tell the shim to fall back.
                let _ = self.write(ServerMsg::Error { message }).await;
                Next::Close
            }
            Err(_) => Next::Close,
        }
    }

    /// The name of the connecting process's parent, from the process table.
    fn shim_parent_name(&self) -> Option<String> {
        self.peer_pid.and_then(parent_pid).and_then(process_name)
    }

    async fn read_events(&self, since_ms: Option<u64>) -> Result<Vec<Event>, String> {
        let events = Arc::clone(&self.shared.events);
        tokio::task::spawn_blocking(move || {
            events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .read(since_ms)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("reading the event log: {e}"))
    }

    async fn reply_events(&mut self, events: Result<Vec<Event>, String>) -> Next {
        match events {
            Ok(events) => {
                self.reply(ServerMsg::Events {
                    events,
                    truncated: false,
                })
                .await
            }
            Err(message) => self.reply(ServerMsg::Error { message }).await,
        }
    }
}

#[cfg(test)]
mod tests;
