//! Tokens the shim fed itself after losing the daemon (ruling R22).

use std::time::Instant;

use damper_proto::{EventKind, Pressure};

use super::{
    Broker, broker, compiling, drained, fed, grow_to_cores, input, logs, req, secs, start,
};

const CORES: u32 = 10;

fn recovered(actions: &[super::Action]) -> Option<u32> {
    logs(actions).into_iter().find_map(|e| match e {
        EventKind::SessionClosed {
            tokens_recovered, ..
        } => Some(*tokens_recovered),
        _ => None,
    })
}

#[test]
fn self_fed_tokens_are_drained_to_the_floor_under_critical() {
    let t0 = Instant::now();
    let mut b = broker(CORES, t0);
    let s = start(&mut b, t0, req(100), 500);
    let grown = grow_to_cores(&mut b, t0, &s, 500);
    let given = fed(&grown, &s);
    assert!(given > 0);

    // The daemon hung; the shim closed the connection and fed cores − 1.
    let hung = b.connection_closed(&s, secs(t0, 11));
    assert!(hung.is_empty(), "cargo still runs: {hung:?}");
    let actions = b.self_fed(&s, CORES - 1, CORES);
    assert!(
        logs(&actions)
            .iter()
            .any(|e| matches!(e, EventKind::ShimFallback { .. }))
    );
    // Cargo holds everything; nothing sits in the FIFO.
    let held = given + CORES - 1;
    assert_eq!(b.status(secs(t0, 11)).sessions[0].tokens_outstanding, held);

    // Critical: as cargo hands tokens back, every one is drained.
    let mut total = 0;
    for (t, fill) in [(12, 5), (13, 0), (14, 7)] {
        let actions = b.tick(&input(
            secs(t0, t),
            Pressure::Critical,
            compiling(500),
            &[(&s, fill)],
        ));
        assert_eq!(drained(&actions, &s), fill, "tick {t}");
        assert_eq!(fed(&actions, &s), 0, "tick {t}");
        total += fill;
    }
    // The next empty reading shows what cargo still holds.
    b.tick(&input(
        secs(t0, 15),
        Pressure::Critical,
        compiling(500),
        &[(&s, 0)],
    ));
    assert_eq!(
        b.status(secs(t0, 15)).sessions[0].tokens_outstanding,
        held - total
    );

    // Cargo exits holding the rest: all of it, self-fed included, recovered.
    let closed = b.pid_exited(500, secs(t0, 16));
    assert_eq!(recovered(&closed), Some(held - total));
}

#[test]
fn self_fed_tokens_do_not_make_the_session_hungry_for_more() {
    let t0 = Instant::now();
    let mut b = broker(CORES, t0);
    let s = start(&mut b, t0, req(100), 500);
    grow_to_cores(&mut b, t0, &s, 500);
    b.self_fed(&s, CORES - 1, CORES);
    let actions = b.tick(&input(
        secs(t0, 11),
        Pressure::Normal,
        compiling(500),
        &[(&s, 0)],
    ));
    assert_eq!(fed(&actions, &s), 0);
    // Spare tokens in the FIFO above the share are drained even when calm.
    let actions = b.tick(&input(
        secs(t0, 12),
        Pressure::Normal,
        compiling(500),
        &[(&s, 4)],
    ));
    assert_eq!(drained(&actions, &s), 4);
}

#[test]
fn self_fed_for_an_unknown_session_is_ignored() {
    let t0 = Instant::now();
    let mut b = broker(CORES, t0);
    let s = start(&mut b, t0, req(100), 500);
    let closed = b.connection_closed(&s, t0);
    assert_eq!(closed, []);
    b.pid_exited(500, t0);
    assert_eq!(b.self_fed(&s, 3, CORES), []);
}

#[test]
fn a_fill_above_given_is_adopted_and_drained() {
    let t0 = Instant::now();
    let mut b = broker(CORES, t0);
    let s = start(&mut b, t0, req(100), 500);
    let given = fed(&grow_to_cores(&mut b, t0, &s, 500), &s);
    // Foreign tokens: the FIFO holds more than the daemon ever gave.
    let fill = given + 6;
    let actions = b.tick(&input(
        secs(t0, 11),
        Pressure::Critical,
        compiling(500),
        &[(&s, fill)],
    ));
    assert_eq!(drained(&actions, &s), fill);
    let closed = b.connection_closed(&s, secs(t0, 12));
    assert_eq!(closed, []);
    let closed = b.pid_exited(500, secs(t0, 12));
    assert_eq!(recovered(&closed), Some(0));
}

/// The final review's worked case: a 2-core config on a 12-core Mac, so the
/// shim feeds 11 while the broker's share is 1 token.
const MACHINE: u32 = 12;
const SPARE: u32 = MACHINE - 1;

/// Which the engine handles first after a hang: the queued `SelfFed` or a
/// tick that already sees the self-fed tokens in the FIFO.
#[derive(Clone, Copy, Debug)]
enum Order {
    SelfFedFirst,
    TickFirst,
}

/// A hung session where the shim fed `SPARE` tokens on top of the `g0` the
/// broker gave, and cargo took `taken` of them before the next tick. Checks
/// that the broker counts exactly the real tokens in either order.
fn recovers_the_real_count(order: Order, taken: u32) {
    let t0 = Instant::now();
    let mut b = broker(2, t0);
    let s = start(&mut b, t0, req(100), 500);
    let g0 = fed(&grow_to_cores(&mut b, t0, &s, 500), &s);
    assert_eq!(g0, 1);
    assert_eq!(b.connection_closed(&s, secs(t0, 11)), []);

    // The FIFO holds the broker's token and the shim's, less what cargo took.
    let mut real = g0 + SPARE;
    let fill = real - taken;
    let tick = |b: &mut Broker| {
        b.tick(&input(
            secs(t0, 11),
            Pressure::Normal,
            compiling(500),
            &[(&s, fill)],
        ))
    };
    let actions = match order {
        Order::SelfFedFirst => {
            b.self_fed(&s, SPARE, MACHINE);
            tick(&mut b)
        }
        Order::TickFirst => {
            let actions = tick(&mut b);
            b.self_fed(&s, SPARE, MACHINE);
            actions
        }
    };
    // Spare tokens above the floor of 1 are drained, as far as the FIFO holds
    // them. A tick that runs first knows only the fill, not what cargo took.
    let known = match order {
        Order::SelfFedFirst => real,
        Order::TickFirst => g0.max(fill),
    };
    let drained_now = drained(&actions, &s);
    assert_eq!(drained_now, fill.min(known - 1), "{order:?}, taken {taken}");
    real -= drained_now;

    // Cargo takes what is left in the FIFO: the broker counts it all as held.
    b.tick(&input(
        secs(t0, 12),
        Pressure::Normal,
        compiling(500),
        &[(&s, 0)],
    ));
    let outstanding = b.status(secs(t0, 12)).sessions[0].tokens_outstanding;
    assert_eq!(outstanding, real, "{order:?}, taken {taken}");

    // Cargo hands every token back: drains stop at the floor of 1.
    let actions = b.tick(&input(
        secs(t0, 13),
        Pressure::Normal,
        compiling(500),
        &[(&s, real)],
    ));
    assert_eq!(drained(&actions, &s), real - 1, "{order:?}, taken {taken}");
    real = 1;

    let closed = b.pid_exited(500, secs(t0, 14));
    assert_eq!(recovered(&closed), Some(real), "{order:?}, taken {taken}");
}

#[test]
fn self_fed_reported_before_the_tick_counts_the_real_tokens() {
    recovers_the_real_count(Order::SelfFedFirst, 0);
}

#[test]
fn self_fed_seen_by_a_tick_first_is_not_counted_twice() {
    recovers_the_real_count(Order::TickFirst, 0);
}

#[test]
fn self_fed_partly_taken_by_cargo_is_counted_once_in_either_order() {
    for order in [Order::SelfFedFirst, Order::TickFirst] {
        for taken in [1, 4, SPARE] {
            recovers_the_real_count(order, taken);
        }
    }
}

#[test]
fn self_fed_seen_by_a_tick_first_keeps_the_session_fed() {
    // A phantom would keep given above the share for the rest of the build,
    // so the session would never be fed again.
    let t0 = Instant::now();
    let mut b = broker(2, t0);
    let s = start(&mut b, t0, req(100), 500);
    let g0 = fed(&grow_to_cores(&mut b, t0, &s, 500), &s);
    assert_eq!(b.connection_closed(&s, secs(t0, 11)), []);
    // Cargo holds g0; the shim's tokens sit in the FIFO. Critical takes them.
    let actions = b.tick(&input(
        secs(t0, 11),
        Pressure::Critical,
        compiling(500),
        &[(&s, SPARE)],
    ));
    assert_eq!(drained(&actions, &s), SPARE);
    b.self_fed(&s, SPARE, MACHINE);
    // Cargo releases what it held; Critical takes that back too.
    let actions = b.tick(&input(
        secs(t0, 12),
        Pressure::Critical,
        compiling(500),
        &[(&s, g0)],
    ));
    assert_eq!(drained(&actions, &s), g0);
    // Calm again: the session is fed its share.
    let mut fed_again = 0;
    for t in 13..=25 {
        let actions = b.tick(&input(
            secs(t0, t),
            Pressure::Normal,
            compiling(500),
            &[(&s, 0)],
        ));
        fed_again += fed(&actions, &s);
    }
    assert_eq!(fed_again, 1, "the session was never fed again");
}

#[test]
fn only_the_first_self_fed_counts() {
    let t0 = Instant::now();
    let mut b = broker(CORES, t0);
    let s = start(&mut b, t0, req(100), 500);
    let given = fed(&grow_to_cores(&mut b, t0, &s, 500), &s);
    assert_eq!(b.connection_closed(&s, secs(t0, 11)), []);
    let first = b.self_fed(&s, 3, CORES);
    assert!(
        logs(&first)
            .iter()
            .any(|e| matches!(e, EventKind::ShimFallback { .. }))
    );
    assert_eq!(b.self_fed(&s, 3, CORES), []);
    assert_eq!(
        b.status(secs(t0, 11)).sessions[0].tokens_outstanding,
        given + 3
    );
}

#[test]
fn self_fed_is_clamped_to_the_machine_cores() {
    let t0 = Instant::now();
    let mut b = broker(CORES, t0);
    let s = start(&mut b, t0, req(100), 500);
    let given = fed(&grow_to_cores(&mut b, t0, &s, 500), &s);
    assert_eq!(b.connection_closed(&s, secs(t0, 11)), []);
    b.self_fed(&s, u32::MAX, MACHINE);
    assert_eq!(
        b.status(secs(t0, 11)).sessions[0].tokens_outstanding,
        given + MACHINE
    );
}
