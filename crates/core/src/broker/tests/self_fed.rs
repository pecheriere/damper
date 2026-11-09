//! Tokens the shim fed itself after losing the daemon (ruling R22).

use std::time::Instant;

use damper_proto::{EventKind, Pressure};

use super::{broker, compiling, drained, fed, grow_to_cores, input, logs, req, secs, start};

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
    let actions = b.self_fed(&s, CORES - 1);
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
    b.self_fed(&s, CORES - 1);
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
    assert_eq!(b.self_fed(&s, 3), []);
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
