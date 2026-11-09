//! The sans-IO broker state machine (spec §5.3–§5.6, §6.3).
//!
//! The daemon feeds the broker what happened (a session opened, its FIFO is
//! ready, cargo started or exited, a tick's samples) and carries out the
//! [`Action`]s it returns. The broker does no I/O and reads no clock: every
//! method takes the time it should act at.
//!
//! Start-up: [`Broker::new`] cannot return actions, so the first [`Broker::tick`]
//! begins with `ConfigLoaded` and `LimitChanged { from: 0, reason: Startup }`.
//! The daemon ticks once before it accepts connections, so these come first in
//! the log.

mod lifecycle;
mod session;
mod tick;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use damper_proto::{
    EventKind, HoldReason, InputsSummary, LimitReason, Pressure, ServerMsg, SessionId, Status,
};

use crate::account::ProcInfo;
use crate::classify::Kind;
use crate::config::Effective;
use crate::policy::Policy;
use session::Session;

/// The most sessions open at once; further opens fail and the shim falls back.
pub const MAX_SESSIONS: usize = 256;

/// A shim asking for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// The build's working directory.
    pub cwd: PathBuf,
    /// The classified cargo subcommand name.
    pub subcommand: String,
    /// What the invocation does.
    pub kind: Kind,
    /// The shim's pid, used to name the FIFO.
    pub shim_pid: u32,
    /// The build may be slowed but never held (rust-analyzer).
    pub never_hold: bool,
    /// A cargo inside another session's build; never held.
    pub nested: bool,
}

/// One memory reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySample {
    /// The system's memory pressure level.
    pub pressure: Pressure,
    /// Memory available, as a percentage of physical.
    pub available_pct: u8,
    /// Physical memory in bytes.
    pub physical: u64,
    /// Swap in use, in bytes.
    pub swap_used: u64,
}

/// Everything sampled for one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickInput {
    /// The tick's time.
    pub now: Instant,
    /// The memory reading.
    pub memory: MemorySample,
    /// The user's processes.
    pub procs: Vec<ProcInfo>,
    /// Tokens sitting in each session's FIFO. A session missing here (its
    /// FIFO could not be read) is neither fed nor drained this tick.
    pub fifo_fill: HashMap<SessionId, u32>,
    /// Pids of the user's sccache servers.
    pub sccache_servers: Vec<u32>,
}

/// Something the daemon must do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Create the session's FIFO, then call [`Broker::fifo_ready`].
    CreateFifo {
        /// The session.
        session: SessionId,
        /// The shim's pid, part of the FIFO's name.
        shim_pid: u32,
    },
    /// Write this many tokens into the session's FIFO, then report how many
    /// went in through [`Broker::feed_result`]: a full FIFO takes fewer.
    Feed {
        /// The session.
        session: SessionId,
        /// Tokens to write.
        tokens: u32,
    },
    /// Read this many tokens back out of the session's FIFO, then report how
    /// many came out through [`Broker::drain_result`]: cargo may take some
    /// first.
    Drain {
        /// The session.
        session: SessionId,
        /// Tokens to read; never more than the FIFO held.
        tokens: u32,
    },
    /// Remove the session's FIFO.
    RemoveFifo {
        /// The session.
        session: SessionId,
    },
    /// Send a message on the session's connection.
    Send {
        /// The session.
        session: SessionId,
        /// The message.
        msg: ServerMsg,
    },
    /// Report this pid's exit through [`Broker::pid_exited`]. A pid that has
    /// already exited (kqueue answers `ESRCH`) must be reported the same way,
    /// at once, or its session never closes.
    WatchPid {
        /// The pid to watch.
        pid: u32,
    },
    /// Append to the event log.
    Log(EventKind),
}

/// Why [`Broker::open`] refused a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// [`MAX_SESSIONS`] are already open.
    #[error("too many open sessions (at most {MAX_SESSIONS})")]
    TooManySessions,
}

/// The broker: the policy, the open sessions and their tokens.
#[derive(Debug)]
pub struct Broker {
    cfg: Effective,
    warnings: Vec<String>,
    policy: Policy,
    start_unix_secs: u64,
    next_id: u64,
    /// Open sessions, oldest first (allocation breaks ties by age).
    sessions: Vec<Session>,
    ticks: u64,
    /// The last decision's hold.
    hold: Option<HoldReason>,
    /// The reason behind the current limit.
    reason: LimitReason,
    /// The inputs behind the last decision.
    inputs: InputsSummary,
    /// Start-up events, logged by the first tick.
    startup: Vec<Action>,
}

impl Broker {
    /// A broker with no sessions and the policy's starting limit.
    #[must_use]
    pub fn new(cfg: Effective, warnings: Vec<String>, start_unix_secs: u64, now: Instant) -> Self {
        let policy = Policy::new(&cfg, now);
        let startup = vec![Action::Log(EventKind::ConfigLoaded {
            warnings: warnings.clone(),
        })];
        Self {
            cfg,
            warnings,
            policy,
            start_unix_secs,
            next_id: 1,
            sessions: Vec::new(),
            ticks: 0,
            hold: None,
            reason: LimitReason::Startup,
            inputs: InputsSummary {
                pressure: Pressure::Normal,
                available_pct: 0,
                swap_used: 0,
                swap_growing: false,
                build_footprint: 0,
                reserve: 0,
            },
            startup,
        }
    }

    /// Applies a new configuration: logs `ConfigLoaded`, and `LimitChanged`
    /// with reason `Config` when the limit moved.
    ///
    /// Before the first tick, the start-up `ConfigLoaded` is dropped as stale
    /// and no `LimitChanged` is logged: the first tick's `Startup` change
    /// reports the configured limit.
    pub fn reconfigure(&mut self, cfg: Effective, warnings: Vec<String>) -> Vec<Action> {
        let from = self.policy.limit();
        self.policy.reconfigure(&cfg);
        self.cfg = cfg;
        self.warnings.clone_from(&warnings);
        self.startup.clear();
        let mut actions = vec![Action::Log(EventKind::ConfigLoaded { warnings })];
        let to = self.policy.limit();
        if to != from && self.ticks > 0 {
            self.reason = LimitReason::Config;
            actions.push(Action::Log(EventKind::LimitChanged {
                from,
                to,
                reason: LimitReason::Config,
                inputs: self.inputs,
            }));
        }
        actions
    }

    /// The status snapshot as of `now`.
    #[must_use]
    pub fn status(&self, now: Instant) -> Status {
        Status {
            limit: self.policy.limit(),
            reason: self.reason,
            cores: self.cfg.cores,
            memory: self.cfg.memory,
            inputs: self.inputs,
            holding: self.hold.is_some(),
            sessions: self.sessions.iter().map(|s| s.status(now)).collect(),
            warnings: self.warnings.clone(),
        }
    }

    /// The daemon wrote `actual` of the `requested` tokens of a
    /// [`Action::Feed`]; the shortfall was never given. Unknown sessions are
    /// ignored.
    pub fn feed_result(&mut self, session: &SessionId, requested: u32, actual: u32) {
        if let Some(index) = self.index(session) {
            let s = &mut self.sessions[index];
            s.given = s.given.saturating_sub(requested.saturating_sub(actual));
        }
    }

    /// The daemon read `actual` of the `requested` tokens of a
    /// [`Action::Drain`]; the shortfall, taken by cargo first, is still
    /// given. Unknown sessions are ignored.
    pub fn drain_result(&mut self, session: &SessionId, requested: u32, actual: u32) {
        if let Some(index) = self.index(session) {
            let s = &mut self.sessions[index];
            s.given = s.given.saturating_add(requested.saturating_sub(actual));
        }
    }

    fn index(&self, session: &SessionId) -> Option<usize> {
        self.sessions.iter().position(|s| &s.id == session)
    }
}

/// Whole milliseconds, saturating.
fn millis(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
