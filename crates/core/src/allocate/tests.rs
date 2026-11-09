#![expect(clippy::unwrap_used, reason = "test")]

use super::*;
use proptest::prelude::*;

const HUNGRY: Demand = Demand {
    hungry: true,
    outstanding: 0,
};

fn satisfied(outstanding: u32) -> Demand {
    Demand {
        hungry: false,
        outstanding,
    }
}

#[test]
fn allocate_gives_everyone_one() {
    assert_eq!(allocate(2, &[HUNGRY; 4]), [1, 1, 1, 1]);
}

#[test]
fn allocate_splits_fairly_among_hungry() {
    assert_eq!(allocate(10, &[HUNGRY; 3]), [4, 3, 3]);
}

#[test]
fn allocate_caps_satisfied_sessions() {
    assert_eq!(allocate(10, &[satisfied(1), HUNGRY, HUNGRY]), [2, 4, 4]);
}

#[test]
fn allocate_leaves_slots_unused_when_everyone_is_satisfied() {
    assert_eq!(allocate(10, &[satisfied(1), satisfied(0)]), [2, 1]);
}

#[test]
fn allocate_handles_no_sessions() {
    assert_eq!(allocate(8, &[]), Vec::<u32>::new());
}

fn demand() -> impl Strategy<Value = Demand> {
    (any::<bool>(), 0_u32..8).prop_map(|(hungry, outstanding)| Demand {
        hungry,
        outstanding,
    })
}

proptest! {
    #[test]
    fn allocate_invariants(limit in 0_u32..64, demands in proptest::collection::vec(demand(), 0..16)) {
        let slots = allocate(limit, &demands);
        let n = u32::try_from(demands.len()).unwrap();
        prop_assert_eq!(slots.len(), demands.len());
        prop_assert!(slots.iter().all(|&s| s >= 1));
        prop_assert!(slots.iter().sum::<u32>() <= limit.max(n));

        let over_cap = |d: &Demand, s: u32| !d.hungry && s > d.outstanding + 1;
        prop_assert!(demands.iter().zip(&slots).all(|(d, &s)| !over_cap(d, s)));

        let at_cap = |d: &Demand, s: u32| !d.hungry && s == d.outstanding + 1;
        let everyone_capped = demands.iter().zip(&slots).all(|(d, &s)| at_cap(d, s));
        if !everyone_capped && limit >= n {
            let hungry: Vec<u32> = demands
                .iter()
                .zip(&slots)
                .filter(|(d, _)| d.hungry)
                .map(|(_, &s)| s)
                .collect();
            if let (Some(min), Some(max)) = (hungry.iter().min(), hungry.iter().max()) {
                prop_assert!(max - min <= 1);
            }
        }
        if limit >= n && demands.iter().any(|d| d.hungry) {
            prop_assert_eq!(slots.iter().sum::<u32>(), limit);
        }
    }
}
