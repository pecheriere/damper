use std::time::{Duration, Instant};

use damper_proto::{HoldReason, LimitReason, Pressure};

use super::*;
use crate::config::Effective;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn cfg() -> Effective {
    Effective {
        cores: 10,
        memory: 32 * GIB,
        max_hold: Duration::from_secs(600),
        enabled: true,
        priority: crate::config::Priority::Utility,
        extra_build_subcommands: Vec::new(),
    }
}

fn calm(limit_used: u32) -> PolicyInputs {
    PolicyInputs {
        pressure: Pressure::Normal,
        available: 20 * GIB,
        available_pct: 60,
        swap_used: 0,
        build_footprint: 0,
        largest_job: 0,
        used_slots: limit_used,
        hungry: false,
    }
}

fn secs(t0: Instant, s: u64) -> Instant {
    t0 + Duration::from_secs(s)
}

/// Grows a fresh policy to `target` slots with hungry ticks, returning the next free second.
fn grown(policy: &mut Policy, t0: Instant, target: u32) -> u64 {
    let mut s = 0;
    while policy.limit() < target {
        let used = policy.limit();
        let inp = PolicyInputs {
            hungry: true,
            ..calm(used)
        };
        policy.tick(secs(t0, s), &inp);
        s += 1;
    }
    s
}

#[test]
fn starts_at_one_slot() {
    let policy = Policy::new(&cfg(), Instant::now());
    assert_eq!(policy.limit(), 1);
}

#[test]
fn grows_one_slot_per_interval_while_hungry() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let mut limits = Vec::new();
    for s in 0..5 {
        let inp = PolicyInputs {
            hungry: true,
            ..calm(policy.limit())
        };
        limits.push(policy.tick(secs(t0, s), &inp).limit);
    }
    assert_eq!(limits, [2, 2, 3, 3, 4]);
    for s in 5..60 {
        let inp = PolicyInputs {
            hungry: true,
            ..calm(policy.limit())
        };
        assert!(policy.tick(secs(t0, s), &inp).limit <= 10);
    }
    assert_eq!(policy.limit(), 10);
}

#[test]
fn growth_reports_its_reason_and_summary() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let inp = PolicyInputs {
        hungry: true,
        build_footprint: 3 * GIB,
        swap_used: 5 * MIB,
        available_pct: 55,
        ..calm(1)
    };
    let d = policy.tick(t0, &inp);
    assert_eq!(d.changed, Some(LimitReason::Growth));
    assert_eq!(d.hold, None);
    assert_eq!(d.summary.pressure, Pressure::Normal);
    assert_eq!(d.summary.available_pct, 55);
    assert_eq!(d.summary.swap_used, 5 * MIB);
    assert!(!d.summary.swap_growing);
    assert_eq!(d.summary.build_footprint, 3 * GIB);
    assert_eq!(d.summary.reserve, INITIAL_RESERVE_CAP);
}

#[test]
fn does_not_grow_when_not_hungry() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    for s in 0..10 {
        let d = policy.tick(secs(t0, s), &calm(1));
        assert_eq!(d.limit, 1);
        assert_eq!(d.changed, None);
    }
}

#[test]
fn does_not_grow_without_room_for_largest_job() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let s = grown(&mut policy, t0, 4);
    let tight = PolicyInputs {
        available: 2 * GIB + 512 * MIB,
        largest_job: 2 * GIB,
        hungry: true,
        ..calm(4)
    };
    // headroom = 2.5 GiB - 1 GiB margin = 1.5 GiB < 2 GiB reserve
    let d = policy.tick(secs(t0, s + 10), &tight);
    assert_eq!(d.limit, 3);
    assert_eq!(d.changed, Some(LimitReason::Headroom));
}

#[test]
fn headroom_shrink_never_goes_below_one() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let tight = PolicyInputs {
        available: GIB,
        largest_job: 2 * GIB,
        ..calm(1)
    };
    let d = policy.tick(t0, &tight);
    assert_eq!(d.limit, 1);
    assert_eq!(d.changed, None);
}

#[test]
fn critical_drops_to_zero_and_holds() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let s = grown(&mut policy, t0, 6);
    let inp = PolicyInputs {
        pressure: Pressure::Critical,
        ..calm(6)
    };
    let d = policy.tick(secs(t0, s), &inp);
    assert_eq!(d.limit, 0);
    assert_eq!(d.changed, Some(LimitReason::Critical));
    assert_eq!(d.hold, Some(HoldReason::PressureCritical));
    // Still critical: stays at zero, no new change reported.
    let d = policy.tick(secs(t0, s + 1), &inp);
    assert_eq!(d.limit, 0);
    assert_eq!(d.changed, None);
    assert_eq!(d.hold, Some(HoldReason::PressureCritical));
}

#[test]
fn warning_halves_and_holds_only_without_headroom() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let s = grown(&mut policy, t0, 8);
    let roomy = PolicyInputs {
        pressure: Pressure::Warning,
        ..calm(8)
    };
    let d = policy.tick(secs(t0, s), &roomy);
    assert_eq!(d.limit, 4);
    assert_eq!(d.changed, Some(LimitReason::Warning));
    assert_eq!(d.hold, None);
    // R4: halves again on every tick while the warning lasts.
    let d = policy.tick(secs(t0, s + 1), &roomy);
    assert_eq!(d.limit, 2);

    let cramped = PolicyInputs {
        available: 1500 * MIB,
        ..roomy
    };
    let d = policy.tick(secs(t0, s + 2), &cramped);
    assert_eq!(d.limit, 1);
    assert_eq!(d.hold, Some(HoldReason::MemoryNearlyFull));
}

#[test]
fn swap_growth_halves_and_blocks_growth() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let s = grown(&mut policy, t0, 8);
    let base = secs(t0, s);
    let at = |swap: u64| PolicyInputs {
        swap_used: swap,
        hungry: true,
        ..calm(8)
    };
    let d = policy.tick(
        base,
        &PolicyInputs {
            swap_used: 0,
            ..calm(8)
        },
    );
    assert_eq!(d.changed, None);
    assert!(!d.summary.swap_growing);
    let d = policy.tick(base + Duration::from_secs(5), &at(100 * MIB));
    assert_eq!(d.limit, 4);
    assert_eq!(d.changed, Some(LimitReason::SwapGrowing));
    assert!(d.summary.swap_growing);
    // Still rising: no growth, halves again.
    let d = policy.tick(base + Duration::from_secs(6), &at(150 * MIB));
    assert_eq!(d.limit, 2);
    let d = policy.tick(base + Duration::from_secs(7), &at(200 * MIB));
    assert_eq!(d.limit, 1);
    assert_ne!(d.changed, Some(LimitReason::Growth));
}

#[test]
fn swap_that_stops_growing_lets_the_pool_grow_again() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let hungry = |swap: u64| PolicyInputs {
        swap_used: swap,
        hungry: true,
        ..calm(1)
    };
    policy.tick(t0, &hungry(0));
    let d = policy.tick(secs(t0, 4), &hungry(200 * MIB));
    assert!(d.summary.swap_growing);
    // Flat at 200 MiB; the rise ages out of the 10 s window.
    let d = policy.tick(secs(t0, 20), &hungry(200 * MIB));
    assert!(!d.summary.swap_growing);
    assert_eq!(d.changed, Some(LimitReason::Growth));
}

#[test]
fn swap_growth_counts_a_dip_then_a_rise() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    policy.tick(
        t0,
        &PolicyInputs {
            swap_used: 500 * MIB,
            ..calm(1)
        },
    );
    policy.tick(
        secs(t0, 2),
        &PolicyInputs {
            swap_used: 100 * MIB,
            ..calm(1)
        },
    );
    let d = policy.tick(
        secs(t0, 4),
        &PolicyInputs {
            swap_used: 200 * MIB,
            ..calm(1)
        },
    );
    assert!(
        d.summary.swap_growing,
        "latest minus window minimum is 100 MiB"
    );
}

#[test]
fn swap_history_stays_bounded() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    for s in 0..10_000 {
        policy.tick(secs(t0, s), &calm(1));
    }
    assert!(policy.swap_samples() <= 12);
}

#[test]
fn idle_limit_decays_to_usage() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let s = grown(&mut policy, t0, 8);
    let base = s + 100;
    for i in 0..10 {
        let d = policy.tick(secs(t0, base + i), &calm(2));
        assert_eq!(d.limit, 8, "tick {i}");
        assert_eq!(d.changed, None);
    }
    let d = policy.tick(secs(t0, base + 10), &calm(2));
    assert_eq!(d.limit, 3);
    assert_eq!(d.changed, Some(LimitReason::IdleDecay));
}

#[test]
fn a_busy_tick_resets_the_idle_timer() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let s = grown(&mut policy, t0, 8);
    let base = s + 100;
    for i in 0..9 {
        policy.tick(secs(t0, base + i), &calm(2));
    }
    // Usage catches up for one tick.
    policy.tick(secs(t0, base + 9), &calm(7));
    for i in 10..20 {
        let d = policy.tick(secs(t0, base + i), &calm(2));
        assert_eq!(d.limit, 8, "tick {i}");
    }
    let d = policy.tick(secs(t0, base + 20), &calm(2));
    assert_eq!(d.limit, 3);
}

#[test]
fn a_limit_change_for_another_reason_resets_the_idle_timer() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let s = grown(&mut policy, t0, 8);
    let base = s + 100;
    for i in 0..8 {
        policy.tick(secs(t0, base + i), &calm(1));
    }
    let warn = PolicyInputs {
        pressure: Pressure::Warning,
        ..calm(1)
    };
    assert_eq!(policy.tick(secs(t0, base + 8), &warn).limit, 4);
    for i in 9..19 {
        let d = policy.tick(secs(t0, base + i), &calm(1));
        assert_eq!(d.limit, 4, "tick {i}");
    }
    let d = policy.tick(secs(t0, base + 19), &calm(1));
    assert_eq!(d.limit, 2);
    assert_eq!(d.changed, Some(LimitReason::IdleDecay));
}

#[test]
fn reserve_fades_after_big_link() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let small = PolicyInputs {
        largest_job: 40 * MIB,
        ..calm(1)
    };
    // Let the start-up reserve fade first.
    for m in 0..10 {
        policy.tick(secs(t0, m * 60), &small);
    }
    let link = secs(t0, 600);
    let one_gib_free = PolicyInputs {
        available: 2 * GIB,
        largest_job: 1331 * MIB,
        hungry: true,
        ..calm(1)
    };
    let d = policy.tick(link, &one_gib_free);
    assert_eq!(
        d.changed, None,
        "1 GiB headroom is below the 1.3 GiB reserve"
    );
    assert!(d.summary.reserve >= 1300 * MIB);

    let hungry_small = PolicyInputs {
        largest_job: 40 * MIB,
        ..one_gib_free
    };
    let mut last = d;
    for s in (1..=360).step_by(10) {
        last = policy.tick(link + Duration::from_secs(s), &hungry_small);
    }
    assert!(last.summary.reserve < 400 * MIB, "{}", last.summary.reserve);
    assert!(policy.limit() > 1, "growth resumed");
}

#[test]
fn reconfigure_clamps_limit() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    grown(&mut policy, t0, 8);
    let smaller = Effective { cores: 4, ..cfg() };
    policy.reconfigure(&smaller);
    assert_eq!(policy.limit(), 4);
    let d = policy.tick(
        secs(t0, 1000),
        &PolicyInputs {
            hungry: true,
            ..calm(4)
        },
    );
    assert_eq!(d.limit, 4, "never grows past the new cores");
}

#[test]
fn changed_is_none_when_value_unchanged() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let d = policy.tick(t0, &calm(1));
    assert_eq!(d.limit, 1);
    assert_eq!(d.changed, None);
}

#[test]
fn nothing_panics_on_extreme_inputs() {
    let t0 = Instant::now();
    let mut policy = Policy::new(&cfg(), t0);
    let extreme = PolicyInputs {
        pressure: Pressure::Warning,
        available: 0,
        available_pct: 255,
        swap_used: u64::MAX,
        build_footprint: u64::MAX,
        largest_job: u64::MAX,
        used_slots: u32::MAX,
        hungry: true,
    };
    policy.tick(t0, &extreme);
    policy.tick(
        t0,
        &PolicyInputs {
            swap_used: 0,
            ..extreme
        },
    );
}

mod props {
    use proptest::prelude::*;

    use super::*;

    fn pressure() -> impl Strategy<Value = Pressure> {
        prop_oneof![
            Just(Pressure::Normal),
            Just(Pressure::Warning),
            Just(Pressure::Critical)
        ]
    }

    prop_compose! {
        fn step()(
            dt in 0u64..5_000,
            pressure in pressure(),
            available in any::<u64>(),
            swap_used in any::<u64>(),
            build_footprint in any::<u64>(),
            largest_job in any::<u64>(),
            used_slots in any::<u32>(),
            hungry in any::<bool>(),
        ) -> (u64, PolicyInputs) {
            (dt, PolicyInputs {
                pressure,
                available,
                available_pct: 50,
                swap_used,
                build_footprint,
                largest_job,
                used_slots,
                hungry,
            })
        }
    }

    proptest! {
        #[test]
        fn limit_never_exceeds_cores(steps in proptest::collection::vec(step(), 1..200)) {
            let t0 = Instant::now();
            let mut policy = Policy::new(&cfg(), t0);
            let mut now = t0;
            for (dt, inp) in steps {
                now += Duration::from_millis(dt);
                let d = policy.tick(now, &inp);
                prop_assert!(d.limit <= 10);
                prop_assert_eq!(d.limit, policy.limit());
            }
        }
    }
}
