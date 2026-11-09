//! Line framing towards one client: reading capped request lines and
//! encoding replies.
//!
//! Requests stay within [`MAX_LINE`]. `Status` and `Events` replies may be up
//! to the reply cap ([`damper_proto::MAX_REPLY`] outside tests): `Events`
//! beyond it keep the newest events and say `truncated`, and a `Status`
//! beyond it becomes an `Error`. Every other message stays within
//! [`MAX_LINE`], so the shim can read it.

use std::time::Duration;

use damper_proto::{Event, MAX_LINE, ProtoError, ServerMsg, encode, encode_with_limit};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;

/// The longest a write to a client may take before the connection is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a line could not be read.
#[derive(Debug)]
pub(super) enum LineError {
    TooLong,
    Io,
}

/// Reads the next line into `buf` (cleared first if it held a whole line).
///
/// Cancel-safe: bytes are moved into `buf` only after `fill_buf` returns, so
/// a cancelled call loses nothing and the next call carries on. `Ok(false)`
/// means the peer closed the connection (a partial last line is dropped).
pub(super) async fn next_line<R: AsyncBufRead + Unpin>(
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

/// Writes `msg` as one line; `Err` means the connection must be dropped.
pub(super) async fn write_msg(
    w: &mut OwnedWriteHalf,
    msg: ServerMsg,
    max_reply: usize,
) -> Result<(), ()> {
    let line = encode_reply(msg, max_reply).map_err(|_| ())?;
    match tokio::time::timeout(WRITE_TIMEOUT, w.write_all(&line)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

/// Encodes `msg`, with `Status` and `Events` allowed up to `max_reply` bytes.
pub(super) fn encode_reply(msg: ServerMsg, max_reply: usize) -> Result<Vec<u8>, ProtoError> {
    match msg {
        ServerMsg::Events { events, .. } => fit_events(events, max_reply),
        status @ ServerMsg::Status(_) => match encode_with_limit(&status, max_reply) {
            Err(ProtoError::LineTooLong) => encode(&ServerMsg::Error {
                message: format!("the status reply is longer than {max_reply} bytes"),
            }),
            other => other,
        },
        other => encode(&other),
    }
}

/// Encodes `Events` with the newest events that fit in `max` bytes: a long
/// history is cut from its oldest end and marked `truncated`, so a reply
/// always carries the latest.
pub(super) fn fit_events(mut events: Vec<Event>, max: usize) -> Result<Vec<u8>, ProtoError> {
    // The envelope with no events; `false` is the longer of the two flags.
    let envelope = encode_with_limit(
        &ServerMsg::Events {
            events: Vec::new(),
            truncated: false,
        },
        usize::MAX,
    )?
    .len();
    let mut budget = max.saturating_sub(envelope);
    let mut start = events.len();
    while let Some(index) = start.checked_sub(1) {
        // Every event but the last one kept needs a separating comma.
        let comma = usize::from(start < events.len());
        let len = serde_json::to_vec(&events[index])?.len() + comma;
        if len > budget {
            break;
        }
        budget -= len;
        start = index;
    }
    events.drain(..start);
    encode_with_limit(
        &ServerMsg::Events {
            events,
            truncated: start > 0,
        },
        max,
    )
}
