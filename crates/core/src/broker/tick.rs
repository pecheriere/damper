//! The broker's tick: account, decide, release holds, feed and drain.

use std::time::Instant;

use damper_proto::{EventKind, LimitReason, ServerMsg};

use super::session::{Session, State};
use super::{Action, Broker, TickInput, millis};
use crate::account::{Accounting, Tracked, account};
use crate::allocate::{Demand, allocate};
use crate::policy::PolicyInputs;

impl Broker {
    /// Runs one tick on `input`, sampled at `input.now`.
    ///
    /// In order: accounts each started session's process tree, runs the
    /// policy, releases holds that cleared or timed out, sets each session's
    /// token target and feeds or drains toward it, and every second tick sends
    /// a heartbeat to every session that has had `Opened` and is still
    /// connected.
    pub fn tick(&mut self, input: &TickInput) -> Vec<Action> {
        let now = input.now;
        let mut actions = std::mem::take(&mut self.startup);
        let first = self.ticks == 0;
        self.ticks = self.ticks.saturating_add(1);

        let accounting = self.observe(input);
        let inputs = self.policy_inputs(input, &accounting);
        let before = self.policy.limit();
        let decision = self.policy.tick(now, &inputs);
        self.inputs = decision.summary;
        self.hold = decision.hold;
        if first {
            actions.push(Action::Log(EventKind::LimitChanged {
                from: 0,
                to: before,
                reason: LimitReason::Startup,
                inputs: decision.summary,
            }));
        }
        if let Some(reason) = decision.changed {
            self.reason = reason;
            actions.push(Action::Log(EventKind::LimitChanged {
                from: before,
                to: decision.limit,
                reason,
                inputs: decision.summary,
            }));
        }

        self.release_holds(now, &mut actions);
        self.feed(&mut actions);
        if self.ticks.is_multiple_of(2) {
            self.heartbeat(&mut actions);
        }
        actions
    }

    /// Records each session's FIFO fill and what its tree is doing.
    fn observe(&mut self, input: &TickInput) -> Accounting {
        let mut tracked = Vec::new();
        let mut owners = Vec::new();
        for (index, s) in self.sessions.iter_mut().enumerate() {
            s.fill = input.fifo_fill.get(&s.id).copied();
            // In normal operation fill ≤ given. Anything above is foreign
            // tokens (a shim's self-feed the daemon never heard of), adopted
            // so drains and `tokens_recovered` cover them.
            if let Some(fill) = s.fill {
                s.given = s.given.max(fill);
            }
            s.compiling = false;
            s.test_jobs = 0;
            if let Some(root_pid) = s.running_pid() {
                tracked.push(Tracked {
                    root_pid,
                    kind: s.req.kind,
                });
                owners.push(index);
            }
        }
        let accounting = account(&input.procs, &tracked, &input.sccache_servers);
        for (index, usage) in owners.into_iter().zip(&accounting.sessions) {
            let s = &mut self.sessions[index];
            s.compiling = usage.compiling;
            s.has_compiled |= usage.compiling;
            if !s.has_compiled {
                s.idle_ticks = s.idle_ticks.saturating_add(1);
            }
            s.test_jobs = usage.test_jobs;
        }
        accounting
    }

    /// The policy's view: R2's available bytes, used slots (each compiling
    /// session's outstanding tokens plus its implicit slot, plus test jobs),
    /// and hunger.
    fn policy_inputs(&self, input: &TickInput, accounting: &Accounting) -> PolicyInputs {
        let memory = input.memory;
        let used_slots = self.sessions.iter().fold(0_u32, |sum, s| {
            let jobs = if s.compiling {
                s.outstanding().saturating_add(1)
            } else {
                0
            };
            sum.saturating_add(jobs).saturating_add(s.test_jobs)
        });
        PolicyInputs {
            pressure: memory.pressure,
            available: memory
                .physical
                .saturating_mul(u64::from(memory.available_pct))
                / 100,
            available_pct: memory.available_pct,
            swap_used: memory.swap_used,
            build_footprint: accounting.total_footprint,
            largest_job: accounting.largest_job,
            used_slots,
            hungry: self.sessions.iter().any(Session::hungry),
        }
    }

    /// Grants held sessions once the hold clears, or with one thread after
    /// `max_hold`. Sessions released together split their share of the pool.
    fn release_holds(&mut self, now: Instant, actions: &mut Vec<Action>) {
        let cleared = self.hold.is_none();
        let mut shares = if cleared {
            self.shares_for(|s| matches!(s.state, State::Held { .. }))
        } else {
            Vec::new()
        }
        .into_iter();
        for s in &mut self.sessions {
            let State::Held { since, .. } = s.state else {
                continue;
            };
            let waited = now.saturating_duration_since(since);
            let timed_out = waited >= self.cfg.max_hold;
            if !cleared && !timed_out {
                continue;
            }
            let (threads, after_timeout) = if cleared {
                (shares.next().unwrap_or(1), false)
            } else {
                (1, true)
            };
            s.state = State::Granted;
            actions.push(Action::Send {
                session: s.id.clone(),
                msg: ServerMsg::Granted {
                    threads,
                    after_timeout,
                },
            });
            actions.push(Action::Log(EventKind::HoldReleased {
                session: s.id.clone(),
                waited_ms: millis(waited),
                timed_out: after_timeout,
            }));
        }
    }

    /// Sets each session's token target and feeds or drains toward it.
    ///
    /// Compiling, granted sessions share `limit − test_jobs` through
    /// [`allocate`]; each keeps `slots − 1` tokens, its implicit slot being
    /// the last. Everyone else, and every session while the policy holds
    /// (spec §6.3: running sessions drop to their floor), has target 0.
    /// Draining takes back only what sits in the FIFO; tokens cargo holds come
    /// back as it releases them. A session whose FIFO fill is unknown keeps
    /// its tokens as they are this tick.
    fn feed(&mut self, actions: &mut Vec<Action>) {
        let held = self.hold.is_some();
        let demands: Vec<_> = self
            .sessions
            .iter()
            .filter(|s| s.sharing())
            .map(Session::demand)
            .collect();
        let mut slots = allocate(self.session_pool(), &demands).into_iter();
        for s in &mut self.sessions {
            let share = if s.sharing() { slots.next() } else { None };
            let target = share
                .filter(|_| !held)
                .map_or(0, |slots| slots.saturating_sub(1));
            if target != s.target {
                s.target = target;
                actions.push(Action::Log(EventKind::SessionTokens {
                    session: s.id.clone(),
                    tokens: target,
                }));
            }
            let Some(fill) = s.fill else {
                continue;
            };
            if target > s.given {
                actions.push(Action::Feed {
                    session: s.id.clone(),
                    tokens: target - s.given,
                });
                s.given = target;
            } else {
                let tokens = fill.min(s.given - target);
                if tokens > 0 {
                    actions.push(Action::Drain {
                        session: s.id.clone(),
                        tokens,
                    });
                    s.given -= tokens;
                }
            }
        }
    }

    /// The slots available to sessions once running tests take theirs.
    fn session_pool(&self) -> u32 {
        let test_jobs = self
            .sessions
            .iter()
            .fold(0_u32, |sum, s| sum.saturating_add(s.test_jobs));
        self.policy.limit().saturating_sub(test_jobs)
    }

    /// Threads for the sessions `joining` picks, being granted now: in
    /// session order, the share each would get if hungry, alongside the
    /// sharing sessions and those granted but not compiling yet; at least 1.
    pub(super) fn shares_for(&self, joining: impl Fn(&Session) -> bool) -> Vec<u32> {
        let waiting = Demand {
            hungry: true,
            outstanding: 0,
        };
        let mut picked = Vec::new();
        let demands: Vec<Demand> = self
            .sessions
            .iter()
            .filter_map(|s| {
                let (joins, demand) = if joining(s) {
                    (true, waiting)
                } else if s.sharing() {
                    (false, s.demand())
                } else if s.about_to_compile() {
                    (false, waiting)
                } else {
                    return None;
                };
                picked.push(joins);
                Some(demand)
            })
            .collect();
        allocate(self.session_pool(), &demands)
            .into_iter()
            .zip(picked)
            .filter_map(|(slots, joins)| joins.then_some(slots.max(1)))
            .collect()
    }

    /// Sends `Heartbeat` to every session that has had `Opened` and whose
    /// connection is still open.
    fn heartbeat(&self, actions: &mut Vec<Action>) {
        let listening = |s: &&Session| s.state != State::Pending && !s.connection_closed;
        for s in self.sessions.iter().filter(listening) {
            actions.push(Action::Send {
                session: s.id.clone(),
                msg: ServerMsg::Heartbeat,
            });
        }
    }
}
