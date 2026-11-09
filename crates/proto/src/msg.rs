//! Messages exchanged over the daemon socket.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::status::Status;

/// Daemon-assigned session identifier: `<daemon start unix secs, hex>-<counter>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why the daemon is holding a session back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    PressureCritical,
    MemoryNearlyFull,
}

impl fmt::Display for HoldReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PressureCritical => "memory pressure critical",
            Self::MemoryNearlyFull => "memory nearly full",
        })
    }
}

/// A message from a client (shim or CLI) to the daemon.
///
/// Carries only the classified subcommand name, never argument lists or
/// environment variables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Open {
        cwd: PathBuf,
        subcommand: String,
        shim_pid: u32,
        parent_pid: u32,
        nested: bool,
    },
    Started {
        cargo_pid: u32,
    },
    Status,
    Log {
        since_ms: Option<u64>,
        session: Option<SessionId>,
    },
    Explain {
        session: SessionId,
    },
}

/// A message from the daemon to a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Opened {
        session: SessionId,
        fifo: PathBuf,
    },
    Held {
        reason: HoldReason,
    },
    Granted {
        threads: u32,
        after_timeout: bool,
    },
    Heartbeat,
    Status(Status),
    /// A struct variant rather than a newtype around the list: serde cannot
    /// serialise a sequence inside an internally tagged newtype variant.
    ///
    /// `truncated` is set when the oldest events were left out so that the
    /// reply fits in [`crate::MAX_REPLY`].
    Events {
        events: Vec<Event>,
        truncated: bool,
    },
    Error {
        message: String,
    },
}
