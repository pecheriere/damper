//! Line framing: one JSON object per line, versioned.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The protocol version carried in every message as `v`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest accepted protocol line, in bytes, including the newline. Every
/// client request and every message the shim reads stays within it.
pub const MAX_LINE: usize = 64 * 1024;

/// Longest reply line the daemon sends to `Status`, `Log` and `Explain`, in
/// bytes, including the newline. Read such replies with
/// [`crate::Client::recv_with_limit`].
pub const MAX_REPLY: usize = 16 * 1024 * 1024;

/// A message with its protocol version, as it appears on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub v: u32,
    #[serde(flatten)]
    pub msg: T,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtoError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported protocol version {got}")]
    Version { got: u32 },
    #[error("protocol line too long")]
    LineTooLong,
    #[error("timed out")]
    Timeout,
    #[error("connection closed")]
    Closed,
}

/// Encodes `msg` as one envelope line ending in `\n`.
///
/// # Errors
/// Returns [`ProtoError::Json`] if serialisation fails, or
/// [`ProtoError::LineTooLong`] if the line would exceed [`MAX_LINE`].
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    encode_with_limit(msg, MAX_LINE)
}

/// Like [`encode`], with a line of up to `max` bytes allowed.
///
/// # Errors
/// Returns [`ProtoError::Json`] if serialisation fails, or
/// [`ProtoError::LineTooLong`] if the line would exceed `max`.
pub fn encode_with_limit<T: Serialize>(msg: &T, max: usize) -> Result<Vec<u8>, ProtoError> {
    let mut line = serde_json::to_vec(&Envelope {
        v: PROTOCOL_VERSION,
        msg,
    })?;
    line.push(b'\n');
    if line.len() > max {
        return Err(ProtoError::LineTooLong);
    }
    Ok(line)
}

/// Decodes one line (with or without its trailing newline).
///
/// The length is checked before parsing and the version before the message
/// is interpreted, so a future version's unknown message reports `Version`.
///
/// # Errors
/// [`ProtoError::LineTooLong`], [`ProtoError::Version`] or [`ProtoError::Json`].
pub fn decode<T: DeserializeOwned>(line: &[u8]) -> Result<T, ProtoError> {
    decode_with_limit(line, MAX_LINE)
}

/// Like [`decode`], with a line of up to `max` bytes allowed.
///
/// # Errors
/// [`ProtoError::LineTooLong`], [`ProtoError::Version`] or [`ProtoError::Json`].
pub fn decode_with_limit<T: DeserializeOwned>(line: &[u8], max: usize) -> Result<T, ProtoError> {
    if line.len() > max {
        return Err(ProtoError::LineTooLong);
    }
    let mut object: Map<String, Value> = serde_json::from_slice(line)?;
    let version = object
        .remove("v")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| <serde_json::Error as serde::de::Error>::missing_field("v"))?;
    if version != u64::from(PROTOCOL_VERSION) {
        return Err(ProtoError::Version {
            got: u32::try_from(version).unwrap_or(u32::MAX),
        });
    }
    Ok(serde_json::from_value(Value::Object(object))?)
}
