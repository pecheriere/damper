//! Token accounting and shares: short feeds and drains, satisfied sessions,
//! and threads for sessions granted close together.

use std::path::PathBuf;
use std::time::Instant;

use damper_proto::{Pressure, ServerMsg, SessionId};

use super::{broker, compiling, drained, fed, grow_to_cores, input, proc, req, secs, sent, start};
use crate::broker::{ABOUT_TO_COMPILE_TICKS, Action, Broker, OpenRequest};
use crate::classify::Kind;

fn granted_threads(actions: &[Action], s: &SessionId) -> Option<u32> {
    sent(actions, s).into_iter().find_map(|m| match m {
        ServerMsg::Granted { threads, .. } => Some(*threads),
        _ => None,
    })
}

/// Grows one hungry session at cargo pid 500 until the limit reaches
/// `cores`, then leaves it idle (cargo alive, no compiler) for one tick, so
/// it no longer takes part in sharing. Returns the next free second.
fn grow_then_idle(b: &mut Broker, t0: Instant, cores: u32) -> u64 {
    let s = start(b, t0, req(100), 500);
    let mut t = 0;
    while b.status(secs(t0, t)).limit < cores {
        b.tick(&input(
            secs(t0, t),
            Pressure::Normal,
            compiling(500),
            &[(&s, 0)],
        ));
        t += 1;
    }
    b.tick(&input(
        secs(t0, t),
        Pressure::Normal,
        vec![proc(500, 1, "cargo")],
        &[(&s, 0)],
    ));
    assert_eq!(b.status(secs(t0, t)).limit, cores);
    t + 1
}

#[test]
fn feed_shortfall_is_fed_again() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    let actions = b.tick(&input(t0, Pressure::Normal, compiling(500), &[(&s, 0)]));
    assert_eq!(fed(&actions, &s), 1);
    // The FIFO was full: none of the token went in.
    b.feed_result(&s, 1, 0);
    let actions = b.tick(&input(
        secs(t0, 1),
        Pressure::Normal,
        compiling(500),
        &[(&s, 0)],
    ));
    assert_eq!(fed(&actions, &s), 1);
    assert_eq!(b.status(secs(t0, 1)).sessions[0].tokens_outstanding, 1);
}

#[test]
fn drain_shortfall_stays_given() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    grow_to_cores(&mut b, t0, &s, 500);
    let actions = b.tick(&input(
        secs(t0, 11),
        Pressure::Critical,
        compiling(500),
        &[(&s, 2)],
    ));
    assert_eq!(drained(&actions, &s), 2);
    // Cargo took one of the two tokens before the read.
    b.drain_result(&s, 2, 1);
    // Cargo returns both tokens it holds: both are drained.
    let actions = b.tick(&input(
        secs(t0, 12),
        Pressure::Critical,
        compiling(500),
        &[(&s, 2)],
    ));
    assert_eq!(drained(&actions, &s), 2);
    assert_eq!(b.status(secs(t0, 12)).sessions[0].tokens_outstanding, 0);
}

#[test]
fn token_results_saturate_and_ignore_unknown_sessions() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    b.tick(&input(t0, Pressure::Normal, compiling(500), &[(&s, 0)]));
    let before = b.status(t0);
    // More reported than asked is no shortfall; an oversized shortfall stops
    // at zero; an unknown session changes nothing.
    b.feed_result(&s, 1, 3);
    b.drain_result(&s, 0, 4);
    b.feed_result(&SessionId("gone".to_owned()), 5, 0);
    b.drain_result(&SessionId("gone".to_owned()), 5, 0);
    assert_eq!(b.status(t0), before);
    b.feed_result(&s, u32::MAX, 0);
    assert_eq!(b.status(t0).sessions[0].tokens_outstanding, 0);
    b.drain_result(&s, u32::MAX, 0);
    b.drain_result(&s, u32::MAX, 0);
    assert_eq!(b.status(t0).sessions[0].tokens_outstanding, u32::MAX);
}

#[test]
fn satisfied_session_keeps_queued_tokens_until_someone_needs_them() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s1 = start(&mut b, t0, req(100), 500);
    grow_to_cores(&mut b, t0, &s1, 500);
    // One of its three tokens sits in the FIFO; the limit did not drop and
    // nobody else wants it, so it stays.
    let actions = b.tick(&input(
        secs(t0, 11),
        Pressure::Normal,
        compiling(500),
        &[(&s1, 1)],
    ));
    assert_eq!((drained(&actions, &s1), fed(&actions, &s1)), (0, 0));

    // A hungry second session: the fair split is 2 and 2, so s1 gives one back.
    let s2 = start(&mut b, secs(t0, 12), req(200), 600);
    let mut procs = compiling(500);
    procs.extend(compiling(600));
    let actions = b.tick(&input(
        secs(t0, 12),
        Pressure::Normal,
        procs,
        &[(&s1, 1), (&s2, 0)],
    ));
    assert_eq!(drained(&actions, &s1), 1);
    assert_eq!(fed(&actions, &s2), 1);
}

#[test]
fn sessions_released_in_one_tick_split_the_pool() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    let t = grow_then_idle(&mut b, t0, 10);
    let mut tight = input(secs(t0, t), Pressure::Warning, Vec::new(), &[]);
    tight.memory.available_pct = 5;
    b.tick(&tight);
    assert!(b.status(secs(t0, t)).holding);
    let held: Vec<SessionId> = (0..3)
        .map(|n| {
            let (s, _) = b.open(secs(t0, t), req(200 + n)).unwrap();
            b.fifo_ready(&s, PathBuf::from("/run/h.fifo"), secs(t0, t));
            s
        })
        .collect();

    let actions = b.tick(&input(secs(t0, t + 1), Pressure::Normal, Vec::new(), &[]));
    let status = b.status(secs(t0, t + 1));
    assert!(!status.holding);
    let threads: Vec<u32> = held
        .iter()
        .map(|s| granted_threads(&actions, s).unwrap())
        .collect();
    assert!(threads.iter().all(|&n| n >= 1), "{threads:?}");
    assert!(
        threads.iter().sum::<u32>() <= status.limit.max(3),
        "{threads:?} for limit {}",
        status.limit
    );
}

#[test]
fn granted_session_not_yet_compiling_counts_for_the_next_grant() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    let t = grow_then_idle(&mut b, t0, 10);
    let (a, _) = b.open(secs(t0, t), req(200)).unwrap();
    let first = b.fifo_ready(&a, PathBuf::from("/run/a.fifo"), secs(t0, t));
    assert_eq!(granted_threads(&first, &a), Some(10));

    // A tick passes with `a` granted but not compiling yet.
    b.tick(&input(
        secs(t0, t + 1),
        Pressure::Normal,
        Vec::new(),
        &[(&a, 0)],
    ));
    let (c, _) = b.open(secs(t0, t + 1), req(300)).unwrap();
    let second = b.fifo_ready(&c, PathBuf::from("/run/c.fifo"), secs(t0, t + 1));
    // `a` already has the whole pool and can't give it back; the newcomer
    // gets its fair share alongside `a`, not the whole pool again.
    assert_eq!(granted_threads(&second, &c), Some(5));
}

/// An idle build session holds the limit at `cores`; then a `cargo run`
/// session (cargo pid 600) is granted and its cargo, up to date, never
/// compiles. Returns the next free second.
fn grant_run_that_never_compiles(b: &mut Broker, t0: Instant, cores: u32) -> (SessionId, u64) {
    let t = grow_then_idle(b, t0, cores);
    let run = OpenRequest {
        kind: Kind::Run,
        ..req(200)
    };
    let r = start(b, secs(t0, t), run, 600);
    (r, t)
}

/// One tick with both cargos alive and neither compiling.
fn idle_tick(b: &mut Broker, t0: Instant, at: u64, r: &SessionId) {
    b.tick(&input(
        secs(t0, at),
        Pressure::Normal,
        vec![proc(500, 1, "cargo"), proc(600, 1, "cargo")],
        &[(r, 0)],
    ));
}

#[test]
fn started_session_that_never_compiles_counts_for_a_few_ticks() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    let (r, t) = grant_run_that_never_compiles(&mut b, t0, 10);
    for i in 0..ABOUT_TO_COMPILE_TICKS - 1 {
        idle_tick(&mut b, t0, t + u64::from(i), &r);
    }
    let at = t + u64::from(ABOUT_TO_COMPILE_TICKS);
    let (c, _) = b.open(secs(t0, at), req(300)).unwrap();
    let granted = b.fifo_ready(&c, PathBuf::from("/run/c.fifo"), secs(t0, at));
    assert_eq!(granted_threads(&granted, &c), Some(5));
}

#[test]
fn started_session_that_never_compiles_stops_counting() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    let (r, t) = grant_run_that_never_compiles(&mut b, t0, 10);
    for i in 0..ABOUT_TO_COMPILE_TICKS {
        idle_tick(&mut b, t0, t + u64::from(i), &r);
    }
    let at = t + u64::from(ABOUT_TO_COMPILE_TICKS);
    let (c, _) = b.open(secs(t0, at), req(300)).unwrap();
    let granted = b.fifo_ready(&c, PathBuf::from("/run/c.fifo"), secs(t0, at));
    assert_eq!(granted_threads(&granted, &c), Some(10));
}

#[test]
fn session_whose_cargo_exited_does_not_count() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    let (_r, t) = grant_run_that_never_compiles(&mut b, t0, 10);
    // Cargo exits; the shim's connection stays open.
    b.pid_exited(600, secs(t0, t));
    let (c, _) = b.open(secs(t0, t), req(300)).unwrap();
    let granted = b.fifo_ready(&c, PathBuf::from("/run/c.fifo"), secs(t0, t));
    assert_eq!(granted_threads(&granted, &c), Some(10));
}
