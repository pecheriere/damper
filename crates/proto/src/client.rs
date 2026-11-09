//! Blocking client over the daemon's Unix socket, used by the shim and CLI.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::event::{Event, EventKind};
use crate::framing::{MAX_LINE, ProtoError, decode_with_limit, encode};
use crate::msg::{ClientMsg, ServerMsg, SessionId};

/// The shortest read/write timeout the OS accepts; zero would mean "block".
const MIN_TIMEOUT: Duration = Duration::from_millis(1);
/// The longest timeout handed to the OS; larger values (and `Duration::MAX`)
/// can be rejected as out of range.
const MAX_TIMEOUT: Duration = Duration::from_hours(24);

/// A blocking connection to the daemon.
///
/// After [`ProtoError::LineTooLong`], [`ProtoError::Io`] or a
/// [`ProtoError::Timeout`] during [`Client::send`], the stream position is
/// unknown (a half-written or half-skipped line), so the connection is
/// poisoned: every later call returns [`ProtoError::Io`] and the caller must
/// drop the client. A [`ProtoError::Timeout`] from [`Client::recv`] is
/// retryable and does not poison.
pub struct Client {
    reader: BufReader<UnixStream>,
    /// Bytes of a line read so far; survives a timeout so no data is lost.
    pending: Vec<u8>,
    /// The read timeout currently set on the socket.
    read_timeout: Option<Duration>,
    /// Set once the stream can no longer be used.
    poisoned: bool,
}

impl Client {
    /// Connects to the daemon socket.
    ///
    /// std offers no safe way to bound `connect` itself; connecting to a
    /// local Unix socket does not block meaningfully. `timeout` therefore
    /// sets the stream's write timeout, and the initial read timeout used
    /// until [`Client::recv`] sets its own.
    ///
    /// # Errors
    /// [`ProtoError::Io`] if the connection or socket setup fails.
    pub fn connect(socket: &Path, timeout: Duration) -> Result<Self, ProtoError> {
        let stream = UnixStream::connect(socket)?;
        let timeout = timeout.clamp(MIN_TIMEOUT, MAX_TIMEOUT);
        stream.set_write_timeout(Some(timeout))?;
        stream.set_read_timeout(Some(timeout))?;
        Ok(Self {
            reader: BufReader::new(stream),
            pending: Vec::new(),
            read_timeout: Some(timeout),
            poisoned: false,
        })
    }

    /// Sends one message.
    ///
    /// # Errors
    /// Encoding errors, or [`ProtoError::Timeout`] / [`ProtoError::Io`] on
    /// write. Both poison the connection, since part of the line may have
    /// been written; so does any call on a poisoned connection.
    pub fn send(&mut self, msg: &ClientMsg) -> Result<(), ProtoError> {
        self.check_usable()?;
        let line = encode(msg)?;
        let mut stream = self.reader.get_ref();
        let written = stream.write_all(&line).and_then(|()| stream.flush());
        written.map_err(|err| self.poison(map_io(err)))
    }

    /// Receives one message. `None` blocks until a line arrives.
    ///
    /// `Some(d)` is a deadline for the whole call, not for each read: a peer
    /// trickling bytes cannot hold the call past about `d`. A timed-out call
    /// can be retried: a partially received line is kept.
    ///
    /// # Errors
    /// [`ProtoError::Timeout`], [`ProtoError::Closed`] on EOF,
    /// [`ProtoError::LineTooLong`] if the peer exceeds [`MAX_LINE`] without
    /// a newline (poisons the connection), [`ProtoError::Io`] (poisons it),
    /// plus decoding errors.
    pub fn recv(&mut self, timeout: Option<Duration>) -> Result<ServerMsg, ProtoError> {
        self.recv_with_limit(timeout, MAX_LINE)
    }

    /// Like [`Client::recv`], accepting a line of up to `max` bytes: the CLI
    /// reads `Status` and `Events` replies with [`crate::MAX_REPLY`].
    ///
    /// # Errors
    /// As [`Client::recv`], with `max` in place of [`MAX_LINE`].
    pub fn recv_with_limit(
        &mut self,
        timeout: Option<Duration>,
        max: usize,
    ) -> Result<ServerMsg, ProtoError> {
        self.check_usable()?;
        let deadline = timeout.and_then(|d| Instant::now().checked_add(d));
        let result = self.read_line(deadline, max);
        if let Err(ProtoError::LineTooLong | ProtoError::Io(_)) = result {
            self.pending.clear();
            self.poisoned = true;
        }
        result
    }

    fn read_line(
        &mut self,
        deadline: Option<Instant>,
        max: usize,
    ) -> Result<ServerMsg, ProtoError> {
        loop {
            let remaining = match deadline {
                Some(end) => {
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(ProtoError::Timeout);
                    }
                    Some(left.clamp(MIN_TIMEOUT, MAX_TIMEOUT))
                }
                None => None,
            };
            match self.set_read_timeout(remaining) {
                Ok(()) => {}
                // macOS rejects setsockopt once the peer has closed. Never
                // read with a stale timeout: drain what is already there
                // without blocking instead.
                Err(err) if err.kind() == ErrorKind::InvalidInput => {
                    return self.read_line_nonblocking(err, max);
                }
                Err(err) => return Err(err.into()),
            }
            if let Some(line) = self.read_chunk(max)? {
                return decode_with_limit(&line, max);
            }
        }
    }

    /// Reads without ever blocking, for a socket whose timeout cannot be
    /// changed. A closed peer yields its remaining data, then EOF. If the
    /// read would block, the peer is still open and the timeout error
    /// `cause` is the real failure.
    fn read_line_nonblocking(
        &mut self,
        cause: std::io::Error,
        max: usize,
    ) -> Result<ServerMsg, ProtoError> {
        self.reader.get_ref().set_nonblocking(true)?;
        let result = loop {
            match self.read_chunk(max) {
                Ok(Some(line)) => break decode_with_limit(&line, max),
                Ok(None) => {}
                Err(ProtoError::Timeout) => break Err(ProtoError::Io(cause)),
                Err(err) => break Err(err),
            }
        };
        self.reader.get_ref().set_nonblocking(false)?;
        result
    }

    /// One bounded read of at most one `fill_buf`, so the caller can check
    /// its deadline between reads. Returns a complete line, or `None` if
    /// more is needed. Never buffers more than `max + 1` bytes.
    fn read_chunk(&mut self, max: usize) -> Result<Option<Vec<u8>>, ProtoError> {
        let room = max.saturating_add(1).saturating_sub(self.pending.len());
        let available = self.reader.fill_buf().map_err(map_io)?;
        if available.is_empty() {
            return Err(ProtoError::Closed);
        }
        let window = &available[..available.len().min(room)];
        let (take, complete) = match window.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (window.len(), false),
        };
        self.pending.extend_from_slice(&window[..take]);
        self.reader.consume(take);
        if complete {
            return Ok(Some(std::mem::take(&mut self.pending)));
        }
        if self.pending.len() > max {
            return Err(ProtoError::LineTooLong);
        }
        Ok(None)
    }

    fn check_usable(&self) -> Result<(), ProtoError> {
        if self.poisoned {
            return Err(ProtoError::Io(std::io::Error::new(
                ErrorKind::BrokenPipe,
                "connection poisoned by an earlier error",
            )));
        }
        Ok(())
    }

    fn poison(&mut self, err: ProtoError) -> ProtoError {
        self.poisoned = true;
        err
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
        if self.read_timeout != timeout {
            self.reader.get_ref().set_read_timeout(timeout)?;
            self.read_timeout = timeout;
        }
        Ok(())
    }
}

fn map_io(err: std::io::Error) -> ProtoError {
    match err.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => ProtoError::Timeout,
        _ => ProtoError::Io(err),
    }
}

/// The events of `session`, plus every `LimitChanged` between its open and
/// close (to the end of `all` if it has not closed), in the original order.
#[must_use]
pub fn explain_events(all: &[Event], session: &SessionId) -> Vec<Event> {
    let opened = all.iter().find_map(|e| match &e.kind {
        EventKind::SessionOpened { session: s, .. } if s == session => Some(e.ts_ms),
        _ => None,
    });
    let closed = all.iter().find_map(|e| match &e.kind {
        EventKind::SessionClosed { session: s, .. } if s == session => Some(e.ts_ms),
        _ => None,
    });
    all.iter()
        .filter(|e| match &e.kind {
            EventKind::LimitChanged { .. } => {
                opened.is_some_and(|start| e.ts_ms >= start)
                    && closed.is_none_or(|end| e.ts_ms <= end)
            }
            kind => kind.session() == Some(session),
        })
        .cloned()
        .collect()
}
