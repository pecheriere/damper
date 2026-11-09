//! Per-session state and its token accounting.

use std::time::Instant;

use damper_proto::{HoldReason, SessionId, SessionState, SessionStatus};

use super::OpenRequest;
use crate::allocate::Demand;

/// Where a session is in its life (spec §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum State {
    /// Opened; its FIFO is being created.
    Pending,
    /// Waiting at the first-slot step since `since`.
    Held { since: Instant, reason: HoldReason },
    /// Holds its first slot.
    Granted,
}

/// The session's cargo process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cargo {
    /// The shim has not reported cargo's pid yet.
    NotStarted,
    /// Running as this pid.
    Running(u32),
    /// Cargo has exited.
    Exited,
}

/// One open session.
#[derive(Debug)]
pub(super) struct Session {
    pub(super) id: SessionId,
    pub(super) req: OpenRequest,
    pub(super) opened: Instant,
    pub(super) state: State,
    pub(super) cargo: Cargo,
    pub(super) connection_closed: bool,
    /// Tokens fed minus tokens drained.
    pub(super) given: u32,
    /// The token target last computed; `SessionTokens` is logged when it moves.
    pub(super) target: u32,
    /// Tokens in the FIFO at the last tick; `None` when it could not be read.
    pub(super) fill: Option<u32>,
    /// A compile process ran in the tree at the last tick.
    pub(super) compiling: bool,
    /// A compile process has run in the tree at some tick.
    pub(super) has_compiled: bool,
    /// Test binaries running at the last tick.
    pub(super) test_jobs: u32,
}

impl Session {
    pub(super) fn new(id: SessionId, req: OpenRequest, now: Instant) -> Self {
        Self {
            id,
            req,
            opened: now,
            state: State::Pending,
            cargo: Cargo::NotStarted,
            connection_closed: false,
            given: 0,
            target: 0,
            fill: None,
            compiling: false,
            has_compiled: false,
            test_jobs: 0,
        }
    }

    /// Tokens cargo holds: given minus those still in the FIFO. With the fill
    /// unknown, every given token counts as held, the cautious reading.
    pub(super) fn outstanding(&self) -> u32 {
        self.given.saturating_sub(self.fill.unwrap_or(0))
    }

    /// Compiling with an empty FIFO: cargo would take more tokens. Never true
    /// while the fill is unknown.
    pub(super) fn hungry(&self) -> bool {
        self.compiling && self.fill == Some(0)
    }

    /// Takes part in sharing the pool: granted and compiling.
    pub(super) fn sharing(&self) -> bool {
        self.state == State::Granted && self.compiling
    }

    /// Granted but not compiling yet, and not running tests (whose jobs come
    /// out of the pool already): it will want its share soon, so a session
    /// granted now must not be told the whole pool is free.
    pub(super) fn about_to_compile(&self) -> bool {
        self.state == State::Granted && !self.has_compiled && self.test_jobs == 0
    }

    /// This session's demand when sharing. A satisfied session is capped at
    /// every token it was given, those still queued in its FIFO included, so
    /// they are drained only when fairness or a lower limit needs them.
    pub(super) fn demand(&self) -> Demand {
        Demand {
            hungry: self.hungry(),
            outstanding: self.given,
        }
    }

    /// Whether this session's tree is accounted (cargo started, not exited).
    pub(super) fn running_pid(&self) -> Option<u32> {
        match self.cargo {
            Cargo::Running(pid) => Some(pid),
            Cargo::NotStarted | Cargo::Exited => None,
        }
    }

    pub(super) fn status(&self, now: Instant) -> SessionStatus {
        let state = match self.state {
            State::Held { since, .. } => SessionState::Held {
                waited_secs: now.saturating_duration_since(since).as_secs(),
            },
            State::Granted if self.compiling => SessionState::Compiling,
            State::Pending | State::Granted => SessionState::Idle,
        };
        let slots = if state == SessionState::Compiling {
            self.target.saturating_add(1)
        } else {
            0
        };
        SessionStatus {
            session: self.id.clone(),
            cwd: self.req.cwd.clone(),
            subcommand: self.req.subcommand.clone(),
            state,
            slots,
            tokens_outstanding: self.outstanding(),
            age_secs: now.saturating_duration_since(self.opened).as_secs(),
        }
    }
}
