//! Wire protocol between damper's shim and daemon.
//!
//! Messages are single-line JSON objects carrying a protocol version `v`,
//! terminated by `\n`. See [`framing`] for the encoding rules.

pub mod client;
pub mod event;
pub mod framing;
pub mod msg;
pub mod paths;
pub mod status;

#[cfg(test)]
mod tests;

pub use client::{Client, explain_events};
pub use event::{Event, EventKind, InputsSummary, LimitReason, Pressure};
pub use framing::{Envelope, MAX_LINE, PROTOCOL_VERSION, ProtoError, decode, encode};
pub use msg::{ClientMsg, HoldReason, ServerMsg, SessionId};
pub use status::{SessionState, SessionStatus, Status};
