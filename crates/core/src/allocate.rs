//! Fair division of the pool's slots among open sessions (spec §5.4).

/// What one session asks for this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Demand {
    /// Cargo is waiting for more tokens than it holds.
    pub hungry: bool,
    /// Tokens cargo holds now.
    pub outstanding: u32,
}

/// Splits `limit` slots among sessions, returned in the order of `demands`
/// (callers pass the oldest session first).
///
/// Every session gets 1, even when `limit` is smaller than the session
/// count. The rest is shared max-min fairly: a satisfied session is capped at
/// `outstanding + 1`, a hungry one is unbounded, and ties go to the earlier
/// session. Slots nobody wants stay unallocated.
#[must_use]
pub fn allocate(limit: u32, demands: &[Demand]) -> Vec<u32> {
    let mut slots = vec![1_u32; demands.len()];
    let sessions = u32::try_from(demands.len()).unwrap_or(u32::MAX);
    for _ in 0..limit.saturating_sub(sessions) {
        let next = demands
            .iter()
            .zip(&slots)
            .enumerate()
            .filter(|(_, (demand, slot))| demand.hungry || **slot <= demand.outstanding)
            .min_by_key(|(index, (_, slot))| (**slot, *index))
            .map(|(index, _)| index);
        match next {
            Some(index) => slots[index] = slots[index].saturating_add(1),
            None => break,
        }
    }
    slots
}

#[cfg(test)]
#[path = "allocate/tests.rs"]
mod tests;
