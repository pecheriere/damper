//! Event-log types, written to `events.jsonl` and returned by `Log`/`Explain`.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::msg::{HoldReason, SessionId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pressure {
    Normal,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitReason {
    Startup,
    Critical,
    Warning,
    SwapGrowing,
    Headroom,
    Growth,
    IdleDecay,
    Config,
}

impl fmt::Display for LimitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Startup => "startup",
            Self::Critical => "critical",
            Self::Warning => "warning",
            Self::SwapGrowing => "swap growing",
            Self::Headroom => "headroom",
            Self::Growth => "growth",
            Self::IdleDecay => "idle decay",
            Self::Config => "config",
        })
    }
}

/// The policy inputs behind a limit decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputsSummary {
    pub pressure: Pressure,
    pub available_pct: u8,
    pub swap_used: u64,
    pub swap_growing: bool,
    pub build_footprint: u64,
    pub reserve: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub ts_ms: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    LimitChanged {
        from: u32,
        to: u32,
        reason: LimitReason,
        inputs: InputsSummary,
    },
    SessionOpened {
        session: SessionId,
        cwd: PathBuf,
        subcommand: String,
        shim_pid: u32,
        never_hold: bool,
    },
    SessionStarted {
        session: SessionId,
        cargo_pid: u32,
    },
    SessionTokens {
        session: SessionId,
        tokens: u32,
    },
    HoldStarted {
        session: SessionId,
        reason: HoldReason,
    },
    HoldReleased {
        session: SessionId,
        waited_ms: u64,
        timed_out: bool,
    },
    SessionClosed {
        session: SessionId,
        tokens_recovered: u32,
        duration_ms: u64,
    },
    ShimFallback {
        reason: String,
        cwd: PathBuf,
    },
    ConfigLoaded {
        warnings: Vec<String>,
    },
    ClientRejected {
        reason: String,
    },
    Error {
        message: String,
    },
}

impl EventKind {
    /// The session this event belongs to, if it is a per-session event.
    #[must_use]
    pub fn session(&self) -> Option<&SessionId> {
        match self {
            Self::SessionOpened { session, .. }
            | Self::SessionStarted { session, .. }
            | Self::SessionTokens { session, .. }
            | Self::HoldStarted { session, .. }
            | Self::HoldReleased { session, .. }
            | Self::SessionClosed { session, .. } => Some(session),
            Self::LimitChanged { .. }
            | Self::ShimFallback { .. }
            | Self::ConfigLoaded { .. }
            | Self::ClientRejected { .. }
            | Self::Error { .. } => None,
        }
    }
}
