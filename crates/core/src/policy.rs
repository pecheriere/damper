//! The pool sizing policy: how many build slots to allow right now.
//!
//! Each tick the policy looks at memory pressure, the swap trend, the measured
//! footprint of running builds and a fading reserve for the largest job seen
//! recently, and moves the global slot limit by at most one step (or halves it
//! under pressure). All arithmetic saturates, so no input can panic.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use damper_proto::{HoldReason, InputsSummary, LimitReason, Pressure};

use crate::config::Effective;
use crate::peak::DecayingPeak;

/// At most one slot is added per this interval.
pub const GROW_INTERVAL: Duration = Duration::from_secs(2);
/// Swap is "growing" if it rose by [`SWAP_GROWTH`] within this window.
pub const SWAP_WINDOW: Duration = Duration::from_secs(10);
/// The rise in swap use, in bytes, that counts as growing (64 MiB).
pub const SWAP_GROWTH: u64 = 64 << 20;
/// How long the reserve takes to halve once big jobs stop.
pub const RESERVE_HALF_LIFE: Duration = Duration::from_secs(180);
/// The most the start-up reserve can be (2 GiB); the budget's quarter if smaller.
pub const INITIAL_RESERVE_CAP: u64 = 2 << 30;
/// Memory kept free of the system's available memory (1 GiB).
pub const SAFETY_MARGIN: u64 = 1 << 30;
/// How long `used + 1 < limit` must hold before the limit drops to `used + 1`.
pub const IDLE_DECAY: Duration = Duration::from_secs(10);

/// What the policy sees each tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyInputs {
    /// The system's memory pressure level.
    pub pressure: Pressure,
    /// Available memory in bytes.
    pub available: u64,
    /// Available memory as a percentage of physical, for the summary.
    pub available_pct: u8,
    /// Swap in use, in bytes.
    pub swap_used: u64,
    /// Measured memory footprint of the running builds, in bytes.
    pub build_footprint: u64,
    /// Footprint of the largest single job currently running, in bytes.
    pub largest_job: u64,
    /// Slots currently in use.
    pub used_slots: u32,
    /// Whether some build is waiting for a slot.
    pub hungry: bool,
}

/// The outcome of one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The global slot limit, implicit slots included.
    pub limit: u32,
    /// Set while builds should wait for memory.
    pub hold: Option<HoldReason>,
    /// Why the limit changed this tick; `None` if it did not.
    pub changed: Option<LimitReason>,
    /// The inputs the decision was based on, for the event log.
    pub summary: InputsSummary,
}

/// The sizing state machine.
#[derive(Debug)]
pub struct Policy {
    cores: u32,
    budget: u64,
    limit: u32,
    reserve: DecayingPeak,
    swap_samples: VecDeque<(Instant, u64)>,
    last_growth: Option<Instant>,
    /// Since when `used + 1 < limit` has held on every tick.
    idle_since: Option<Instant>,
}

impl Policy {
    /// A policy at one slot, reserving `min(2 GiB, budget / 4)` to start.
    #[must_use]
    pub fn new(cfg: &Effective, now: Instant) -> Self {
        let initial_reserve = INITIAL_RESERVE_CAP.min(cfg.memory / 4);
        Self {
            cores: cfg.cores,
            budget: cfg.memory,
            limit: 1.min(cfg.cores),
            reserve: DecayingPeak::new(initial_reserve, RESERVE_HALF_LIFE, now),
            swap_samples: VecDeque::new(),
            last_growth: None,
            idle_since: None,
        }
    }

    /// The current slot limit.
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Applies a new configuration, clamping the limit to the new core count.
    pub fn reconfigure(&mut self, cfg: &Effective) {
        self.cores = cfg.cores;
        self.budget = cfg.memory;
        self.limit = self.limit.min(self.cores);
    }

    /// Records the swap reading and says whether swap rose by at least
    /// [`SWAP_GROWTH`] within [`SWAP_WINDOW`].
    ///
    /// The rise is measured from the lowest sample in the window, not the
    /// first, so a dip followed by a climb counts. Samples older than the
    /// window are dropped, which keeps the history bounded.
    fn swap_growing(&mut self, now: Instant, swap_used: u64) -> bool {
        while self
            .swap_samples
            .front()
            .is_some_and(|&(t, _)| now.saturating_duration_since(t) > SWAP_WINDOW)
        {
            self.swap_samples.pop_front();
        }
        if let Some(last) = self.swap_samples.back_mut().filter(|(t, _)| *t >= now) {
            last.1 = swap_used;
        } else {
            self.swap_samples.push_back((now, swap_used));
        }
        let lowest = self
            .swap_samples
            .iter()
            .map(|&(_, v)| v)
            .min()
            .unwrap_or(swap_used);
        swap_used.saturating_sub(lowest) >= SWAP_GROWTH
    }

    #[cfg(test)]
    fn swap_samples(&self) -> usize {
        self.swap_samples.len()
    }

    /// Runs one policy step.
    pub fn tick(&mut self, now: Instant, inp: &PolicyInputs) -> Decision {
        self.reserve.observe(inp.largest_job, now);
        let reserve = self.reserve.get(now);
        let swap_growing = self.swap_growing(now, inp.swap_used);
        let headroom = self
            .budget
            .saturating_sub(inp.build_footprint)
            .min(inp.available.saturating_sub(SAFETY_MARGIN));
        let before = self.limit;

        let (reason, hold) = if inp.pressure == Pressure::Critical {
            self.idle_since = None;
            self.limit = 0;
            (
                Some(LimitReason::Critical),
                Some(HoldReason::PressureCritical),
            )
        } else if inp.pressure == Pressure::Warning || swap_growing {
            self.idle_since = None;
            self.limit /= 2;
            let reason = if inp.pressure == Pressure::Warning {
                LimitReason::Warning
            } else {
                LimitReason::SwapGrowing
            };
            let hold = (headroom < reserve).then_some(HoldReason::MemoryNearlyFull);
            (Some(reason), hold)
        } else {
            (self.normal(now, inp, headroom, reserve), None)
        };

        self.limit = self.limit.min(self.cores);
        let changed = reason.filter(|_| self.limit != before);
        Decision {
            limit: self.limit,
            hold,
            changed,
            summary: InputsSummary {
                pressure: inp.pressure,
                available_pct: inp.available_pct,
                swap_used: inp.swap_used,
                swap_growing,
                build_footprint: inp.build_footprint,
                reserve,
            },
        }
    }

    /// Normal pressure, swap steady: shrink, grow or let an idle limit decay.
    ///
    /// Returns the reason when it acted. Any tick that does not satisfy
    /// `used + 1 < limit` (including ticks that moved the limit for another
    /// reason, and ticks under pressure) restarts the idle timer.
    fn normal(
        &mut self,
        now: Instant,
        inp: &PolicyInputs,
        headroom: u64,
        reserve: u64,
    ) -> Option<LimitReason> {
        if headroom < reserve {
            self.idle_since = None;
            if self.limit > 1 {
                self.limit -= 1;
                return Some(LimitReason::Headroom);
            }
            return None;
        }
        let may_grow = self
            .last_growth
            .is_none_or(|t| now.saturating_duration_since(t) >= GROW_INTERVAL);
        if inp.hungry && may_grow && self.limit < self.cores {
            self.limit += 1;
            self.last_growth = Some(now);
            self.idle_since = None;
            return Some(LimitReason::Growth);
        }
        let target = inp.used_slots.saturating_add(1);
        if target >= self.limit {
            self.idle_since = None;
            return None;
        }
        let since = *self.idle_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= IDLE_DECAY {
            self.limit = target;
            self.idle_since = None;
            return Some(LimitReason::IdleDecay);
        }
        None
    }
}

#[cfg(test)]
mod tests;
