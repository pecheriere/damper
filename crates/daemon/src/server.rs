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
    ClientMsg, Event, EventKind, MAX_LINE, ProtoError, ServerMsg, SessionId, Status, decode,
    encode, explain_events,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot};

use crate::eventlog::EventLog;
use crate::procs::process_name;

/// The longest a write to a client may take before the connection is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Messages queued for one session's client; a client that lets this many
/// pile up is not reading and is disconnected.
pub(crate) const CLIENT_QUEUE: usize = 64;

/// The name of the parent process whose builds are never held (spec §5.4).
const NEVER_HOLD_PARENT: &str = "rust-analyzer";

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

/// Only the daemon's own user may connect.
#[must_use]
pub fn peer_allowed(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid
}

/// Why a line could not be read.
#[derive(Debug)]
enum LineError {
    TooLong,
    Io,
}

/// Reads the next line into `buf` (cleared first if it held a whole line).
///
/// Cancel-safe: bytes are moved into `buf` only after `fill_buf` returns, so
/// a cancelled call loses nothing and the next call carries on. `Ok(false)`
/// means the peer closed the connection (a partial last line is dropped).
async fn next_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<bool, LineError> {
    if buf.last() == Some(&b'\n') {
        buf.clear();
    }
    loop {
        let available = reader.fill_buf().await.map_err(|_| LineError::Io)?;
        if available.is_empty() {
            return Ok(false);
        }
        let (take, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        let room = (MAX_LINE + 1).saturating_sub(buf.len());
        buf.extend_from_slice(&available[..take.min(room)]);
        reader.consume(take);
        if buf.len() > MAX_LINE {
            return Err(LineError::TooLong);
        }
        if done {
            return Ok(true);
        }
    }
}

async fn write_msg(w: &mut OwnedWriteHalf, msg: &ServerMsg) -> Result<(), ()> {
    let line = match msg {
        ServerMsg::Events { events } => fit_events(events),
        other => encode(other),
    }
    .map_err(|_| ())?;
    match tokio::time::timeout(WRITE_TIMEOUT, w.write_all(&line)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

/// Room left in a line for the envelope around the events:
/// `{"v":1,"type":"events","events":[` … `]}` and the newline.
const EVENTS_ENVELOPE: usize = 64;

/// Encodes `Events` with the newest events that fit in [`MAX_LINE`]: a long
/// history is cut from its oldest end, so a reply always carries the latest.
fn fit_events(events: &[Event]) -> Result<Vec<u8>, ProtoError> {
    let mut budget = MAX_LINE - EVENTS_ENVELOPE;
    let mut start = events.len();
    while let Some(index) = start.checked_sub(1) {
        // One more byte for the separating comma.
        let len = serde_json::to_vec(&events[index])?.len() + 1;
        if len > budget {
            break;
        }
        budget -= len;
        start = index;
    }
    encode(&ServerMsg::Events {
        events: events[start..].to_vec(),
    })
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
                        if write_msg(&mut self.writer, &msg).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    line = next_line(&mut self.reader, &mut self.buf) => line,
                }
            } else {
                next_line(&mut self.reader, &mut self.buf).await
            };
            match line {
                Ok(true) => {}
                Ok(false) | Err(LineError::Io) => return,
                Err(LineError::TooLong) => {
                    return self
                        .reject(format!("protocol line longer than {MAX_LINE} bytes"))
                        .await;
                }
            }
            let msg = match decode::<ClientMsg>(&self.buf) {
                Ok(msg) => msg,
                Err(e) => return self.reject(e.to_string()).await,
            };
            if matches!(self.handle(msg).await, Next::Close) {
                return;
            }
        }
    }

    /// Sends `Error`, logs `ClientRejected`; the caller then closes.
    async fn reject(&mut self, reason: String) {
        // The connection closes next whether or not the client got the line.
        let _ = write_msg(
            &mut self.writer,
            &ServerMsg::Error {
                message: reason.clone(),
            },
        )
        .await;
        self.shared.reject(reason);
    }

    async fn reply(&mut self, msg: &ServerMsg) -> Next {
        match write_msg(&mut self.writer, msg).await {
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
                    Ok(status) => self.reply(&ServerMsg::Status(status)).await,
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
                parent_pid,
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
                    never_hold: process_name(parent_pid).as_deref() == Some(NEVER_HOLD_PARENT),
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
                let _ = write_msg(&mut self.writer, &ServerMsg::Error { message }).await;
                Next::Close
            }
            Err(_) => Next::Close,
        }
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
            Ok(events) => self.reply(&ServerMsg::Events { events }).await,
            Err(message) => self.reply(&ServerMsg::Error { message }).await,
        }
    }
}

#[cfg(test)]
mod tests;
