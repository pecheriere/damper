//! A session's life: open, FIFO ready, cargo started, close.

use std::path::PathBuf;
use std::time::Instant;

use damper_proto::{EventKind, ServerMsg, SessionId};

use super::session::{Cargo, Session, State};
use super::{Action, Broker, MAX_SESSIONS, OpenError, OpenRequest, millis};

impl Broker {
    /// Registers a session and asks the daemon to create its FIFO.
    ///
    /// # Errors
    ///
    /// [`OpenError::TooManySessions`] when [`MAX_SESSIONS`] are open.
    pub fn open(
        &mut self,
        now: Instant,
        req: OpenRequest,
    ) -> Result<(SessionId, Vec<Action>), OpenError> {
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(OpenError::TooManySessions);
        }
        let id = SessionId(format!("{:x}-{}", self.start_unix_secs, self.next_id));
        self.next_id = self.next_id.saturating_add(1);
        let actions = vec![
            Action::CreateFifo {
                session: id.clone(),
                shim_pid: req.shim_pid,
            },
            Action::Log(EventKind::SessionOpened {
                session: id.clone(),
                cwd: req.cwd.clone(),
                subcommand: req.subcommand.clone(),
                shim_pid: req.shim_pid,
                never_hold: req.never_hold,
            }),
        ];
        self.sessions.push(Session::new(id.clone(), req, now));
        Ok((id, actions))
    }

    /// The session's FIFO exists: sends `Opened`, then `Granted` or `Held`.
    ///
    /// A session is held only while the last decision holds, and never when it
    /// is nested or `never_hold`.
    pub fn fifo_ready(&mut self, session: &SessionId, path: PathBuf, now: Instant) -> Vec<Action> {
        let hold = self.hold;
        let threads = self
            .shares_for(|s| &s.id == session)
            .first()
            .copied()
            .unwrap_or(1);
        let Some(index) = self.index(session) else {
            return Vec::new();
        };
        let s = &mut self.sessions[index];
        if s.state != State::Pending {
            return Vec::new();
        }
        let mut actions = vec![Action::Send {
            session: session.clone(),
            msg: ServerMsg::Opened {
                session: session.clone(),
                fifo: path,
            },
        }];
        if let Some(reason) = hold.filter(|_| !s.req.nested && !s.req.never_hold) {
            s.state = State::Held { since: now, reason };
            actions.push(Action::Send {
                session: session.clone(),
                msg: ServerMsg::Held { reason },
            });
            actions.push(Action::Log(EventKind::HoldStarted {
                session: session.clone(),
                reason,
            }));
        } else {
            s.state = State::Granted;
            actions.push(Action::Send {
                session: session.clone(),
                msg: ServerMsg::Granted {
                    threads,
                    after_timeout: false,
                },
            });
        }
        actions
    }

    /// Cargo is running as `cargo_pid`: watch it, so the session outlives
    /// its connection until cargo exits. Ignored after the first call.
    pub fn started(&mut self, session: &SessionId, cargo_pid: u32) -> Vec<Action> {
        let Some(index) = self.index(session) else {
            return Vec::new();
        };
        let s = &mut self.sessions[index];
        if s.cargo != Cargo::NotStarted {
            return Vec::new();
        }
        s.cargo = Cargo::Running(cargo_pid);
        vec![
            Action::WatchPid { pid: cargo_pid },
            Action::Log(EventKind::SessionStarted {
                session: session.clone(),
                cargo_pid,
            }),
        ]
    }

    /// The shim's connection closed. The session closes now unless cargo
    /// started and is still running.
    pub fn connection_closed(&mut self, session: &SessionId, now: Instant) -> Vec<Action> {
        let Some(index) = self.index(session) else {
            return Vec::new();
        };
        self.sessions[index].connection_closed = true;
        self.close_if_done(index, now)
    }

    /// A watched pid exited: closes every session whose cargo it was and whose
    /// connection is already closed.
    pub fn pid_exited(&mut self, pid: u32, now: Instant) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut index = 0;
        while index < self.sessions.len() {
            let s = &mut self.sessions[index];
            if s.cargo == Cargo::Running(pid) {
                s.cargo = Cargo::Exited;
                let closed = self.close_if_done(index, now);
                if !closed.is_empty() {
                    actions.extend(closed);
                    continue;
                }
            }
            index += 1;
        }
        actions
    }

    /// Closes session `index` if its connection is closed and its cargo, if
    /// any, has exited; returns the actions, empty if it stays open.
    fn close_if_done(&mut self, index: usize, now: Instant) -> Vec<Action> {
        let s = &self.sessions[index];
        if !s.connection_closed || s.running_pid().is_some() {
            return Vec::new();
        }
        let s = self.sessions.remove(index);
        let duration_ms = millis(now.saturating_duration_since(s.opened));
        vec![
            Action::RemoveFifo {
                session: s.id.clone(),
            },
            Action::Log(EventKind::SessionClosed {
                session: s.id,
                tokens_recovered: s.given,
                duration_ms,
            }),
        ]
    }
}
