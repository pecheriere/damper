//! Blocking client over the daemon's Unix socket, used by the shim and CLI.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::event::{Event, EventKind};
use crate::framing::{MAX_LINE, ProtoError, decode, encode};
use crate::msg::{ClientMsg, ServerMsg, SessionId};

/// The shortest read/write timeout the OS accepts; zero would mean "block".
const MIN_TIMEOUT: Duration = Duration::from_millis(1);

pub struct Client {
    reader: BufReader<UnixStream>,
    /// Bytes of a line read so far; survives a timeout so no data is lost.
    pending: Vec<u8>,
    /// The read timeout currently set on the socket.
    read_timeout: Option<Duration>,
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
        let timeout = timeout.max(MIN_TIMEOUT);
        stream.set_write_timeout(Some(timeout))?;
        stream.set_read_timeout(Some(timeout))?;
        Ok(Self {
            reader: BufReader::new(stream),
            pending: Vec::new(),
            read_timeout: Some(timeout),
        })
    }

    /// Sends one message.
    ///
    /// # Errors
    /// Encoding errors, or [`ProtoError::Timeout`] / [`ProtoError::Io`] on write.
    pub fn send(&mut self, msg: &ClientMsg) -> Result<(), ProtoError> {
        let line = encode(msg)?;
        let mut stream = self.reader.get_ref();
        stream.write_all(&line).map_err(map_io)?;
        stream.flush().map_err(map_io)
    }

    /// Receives one message. `None` blocks until a line arrives.
    ///
    /// A timed-out call can be retried: a partially received line is kept.
    ///
    /// # Errors
    /// [`ProtoError::Timeout`], [`ProtoError::Closed`] on EOF,
    /// [`ProtoError::LineTooLong`] if the peer exceeds [`MAX_LINE`] without
    /// a newline, plus decoding errors.
    pub fn recv(&mut self, timeout: Option<Duration>) -> Result<ServerMsg, ProtoError> {
        self.set_read_timeout(timeout.map(|d| d.max(MIN_TIMEOUT)))?;
        loop {
            let room = (MAX_LINE + 1).saturating_sub(self.pending.len());
            let mut limited = (&mut self.reader).take(room as u64);
            let read = limited
                .read_until(b'\n', &mut self.pending)
                .map_err(map_io)?;
            if self.pending.last() == Some(&b'\n') {
                let line = std::mem::take(&mut self.pending);
                return decode(&line);
            }
            if self.pending.len() > MAX_LINE {
                self.pending.clear();
                return Err(ProtoError::LineTooLong);
            }
            if read == 0 {
                return Err(ProtoError::Closed);
            }
        }
    }
}

impl Client {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), ProtoError> {
        if self.read_timeout == timeout {
            return Ok(());
        }
        match self.reader.get_ref().set_read_timeout(timeout) {
            Ok(()) => self.read_timeout = timeout,
            // macOS rejects setsockopt once the peer has closed. The read
            // that follows then reports EOF at once, so it cannot block.
            Err(err) if err.kind() == ErrorKind::InvalidInput => {}
            Err(err) => return Err(err.into()),
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
