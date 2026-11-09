#![expect(clippy::unwrap_used, reason = "test")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use damper_proto::{
    EventKind, HoldReason, LimitReason, Pressure, ServerMsg, SessionId, SessionState,
};

use super::{Action, Broker, MemorySample, OpenError, OpenRequest, TickInput};
use crate::account::ProcInfo;
use crate::classify::Kind;
use crate::config::{Effective, Priority};

mod lifecycle;
mod self_fed;
mod shares;

const GIB: u64 = 1 << 30;
const START: u64 = 0x6730_0000;

fn cfg(cores: u32) -> Effective {
    Effective {
        cores,
        memory: 24 * GIB,
        max_hold: Duration::from_secs(600),
        enabled: true,
        priority: Priority::Utility,
        extra_build_subcommands: Vec::new(),
    }
}

fn broker(cores: u32, t0: Instant) -> Broker {
    Broker::new(cfg(cores), vec!["w".to_owned()], START, t0)
}

fn memory(pressure: Pressure) -> MemorySample {
    MemorySample {
        pressure,
        available_pct: 60,
        physical: 32 * GIB,
        swap_used: 0,
    }
}

fn req(shim_pid: u32) -> OpenRequest {
    OpenRequest {
        cwd: PathBuf::from("/w"),
        subcommand: "build".to_owned(),
        kind: Kind::Build,
        shim_pid,
        never_hold: false,
        nested: false,
    }
}

fn proc(pid: u32, parent: u32, name: &str) -> ProcInfo {
    ProcInfo {
        pid,
        ppid: parent,
        name: name.to_owned(),
        footprint: 1 << 20,
        peak: 1 << 20,
    }
}

/// A cargo at `cargo_pid` with a rustc child.
fn compiling(cargo_pid: u32) -> Vec<ProcInfo> {
    vec![
        proc(cargo_pid, 1, "cargo"),
        proc(cargo_pid + 1, cargo_pid, "rustc"),
    ]
}

fn input(
    now: Instant,
    pressure: Pressure,
    procs: Vec<ProcInfo>,
    fill: &[(&SessionId, u32)],
) -> TickInput {
    TickInput {
        now,
        memory: memory(pressure),
        procs,
        fifo_fill: fill.iter().map(|(s, n)| ((*s).clone(), *n)).collect(),
        sccache_servers: Vec::new(),
    }
}

fn secs(t0: Instant, s: u64) -> Instant {
    t0 + Duration::from_secs(s)
}

/// Opens a session, readies its FIFO and starts cargo at `cargo_pid`.
fn start(b: &mut Broker, now: Instant, request: OpenRequest, cargo_pid: u32) -> SessionId {
    let (s, _) = b.open(now, request).unwrap();
    b.fifo_ready(&s, PathBuf::from(format!("/run/{s}.fifo")), now);
    b.started(&s, cargo_pid);
    s
}

fn fed(actions: &[Action], s: &SessionId) -> u32 {
    actions
        .iter()
        .map(|a| match a {
            Action::Feed { session, tokens } if session == s => *tokens,
            _ => 0,
        })
        .sum()
}

fn drained(actions: &[Action], s: &SessionId) -> u32 {
    actions
        .iter()
        .map(|a| match a {
            Action::Drain { session, tokens } if session == s => *tokens,
            _ => 0,
        })
        .sum()
}

fn sent<'a>(actions: &'a [Action], s: &SessionId) -> Vec<&'a ServerMsg> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send { session, msg } if session == s => Some(msg),
            _ => None,
        })
        .collect()
}

fn logs(actions: &[Action]) -> Vec<&EventKind> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Log(event) => Some(event),
            _ => None,
        })
        .collect()
}

fn token_events(actions: &[Action], s: &SessionId) -> Vec<u32> {
    logs(actions)
        .into_iter()
        .filter_map(|e| match e {
            EventKind::SessionTokens { session, tokens } if session == s => Some(*tokens),
            _ => None,
        })
        .collect()
}

/// Grows one hungry compiling session to `cores` slots; returns all actions.
fn grow_to_cores(b: &mut Broker, t0: Instant, s: &SessionId, cargo_pid: u32) -> Vec<Action> {
    let mut all = Vec::new();
    for t in 0..=10 {
        let actions = b.tick(&input(
            secs(t0, t),
            Pressure::Normal,
            compiling(cargo_pid),
            &[(s, 0)],
        ));
        all.extend(actions);
    }
    all
}

#[test]
fn open_creates_fifo_then_grants() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    let (s, actions) = b.open(t0, req(100)).unwrap();
    assert_eq!(s, SessionId(format!("{START:x}-1")));
    assert_eq!(
        actions,
        vec![
            Action::CreateFifo {
                session: s.clone(),
                shim_pid: 100
            },
            Action::Log(EventKind::SessionOpened {
                session: s.clone(),
                cwd: PathBuf::from("/w"),
                subcommand: "build".to_owned(),
                shim_pid: 100,
                never_hold: false,
            }),
        ]
    );
    let fifo = PathBuf::from("/run/s.fifo");
    let actions = b.fifo_ready(&s, fifo.clone(), t0);
    assert_eq!(actions.len(), 2);
    assert_eq!(
        actions[0],
        Action::Send {
            session: s.clone(),
            msg: ServerMsg::Opened {
                session: s.clone(),
                fifo
            }
        }
    );
    match &actions[1] {
        Action::Send {
            session,
            msg:
                ServerMsg::Granted {
                    threads,
                    after_timeout: false,
                },
        } => {
            assert_eq!(session, &s);
            assert!(*threads >= 1);
        }
        other => panic!("expected Granted, got {other:?}"),
    }
}

#[test]
fn first_tick_logs_startup_and_config() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    let actions = b.tick(&input(t0, Pressure::Normal, Vec::new(), &[]));
    let events = logs(&actions);
    assert_eq!(
        events[0],
        &EventKind::ConfigLoaded {
            warnings: vec!["w".to_owned()]
        }
    );
    assert!(matches!(
        events[1],
        EventKind::LimitChanged {
            from: 0,
            to: 1,
            reason: LimitReason::Startup,
            ..
        }
    ));
    let again = b.tick(&input(secs(t0, 1), Pressure::Normal, Vec::new(), &[]));
    assert_eq!(logs(&again), Vec::<&EventKind>::new());
}

#[test]
fn feeds_hungry_session_up_to_share() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    let mut total = 0;
    let mut all = Vec::new();
    for t in 0..=10 {
        let actions = b.tick(&input(
            secs(t0, t),
            Pressure::Normal,
            compiling(500),
            &[(&s, 0)],
        ));
        total += fed(&actions, &s);
        assert_eq!(drained(&actions, &s), 0);
        assert!(total < b.status(secs(t0, t)).limit.max(1));
        all.extend(actions);
    }
    assert_eq!(total, 3);
    assert_eq!(b.status(secs(t0, 10)).limit, 4);
    assert_eq!(token_events(&all, &s), vec![1, 2, 3]);
    let growth = logs(&all)
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                EventKind::LimitChanged {
                    reason: LimitReason::Growth,
                    ..
                }
            )
        })
        .count();
    assert_eq!(growth, 3);
}

#[test]
fn drains_unused_tokens_when_limit_drops() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    let grown = grow_to_cores(&mut b, t0, &s, 500);
    assert_eq!(fed(&grown, &s), 3);

    let actions = b.tick(&input(
        secs(t0, 11),
        Pressure::Critical,
        compiling(500),
        &[(&s, 2)],
    ));
    assert_eq!(drained(&actions, &s), 2);
    assert_eq!(fed(&actions, &s), 0);
    assert!(logs(&actions).iter().any(|e| matches!(
        e,
        EventKind::LimitChanged {
            from: 4,
            to: 0,
            reason: LimitReason::Critical,
            ..
        }
    )));

    let actions = b.tick(&input(
        secs(t0, 12),
        Pressure::Critical,
        compiling(500),
        &[(&s, 0)],
    ));
    assert_eq!(drained(&actions, &s), 0);
    assert_eq!(fed(&actions, &s), 0);

    let actions = b.tick(&input(
        secs(t0, 13),
        Pressure::Critical,
        compiling(500),
        &[(&s, 1)],
    ));
    assert_eq!(drained(&actions, &s), 1);
}

#[test]
fn unknown_fifo_fill_neither_feeds_nor_drains() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    grow_to_cores(&mut b, t0, &s, 500);
    let actions = b.tick(&input(
        secs(t0, 11),
        Pressure::Critical,
        compiling(500),
        &[],
    ));
    assert_eq!(drained(&actions, &s), 0);
    let actions = b.tick(&input(
        secs(t0, 12),
        Pressure::Critical,
        compiling(500),
        &[(&s, 2)],
    ));
    assert_eq!(drained(&actions, &s), 2);
}

#[test]
fn holds_new_session_in_critical_and_releases() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    b.tick(&input(t0, Pressure::Critical, Vec::new(), &[]));
    let (s, _) = b.open(secs(t0, 1), req(100)).unwrap();
    let actions = b.fifo_ready(&s, PathBuf::from("/run/s.fifo"), secs(t0, 1));
    assert_eq!(
        sent(&actions, &s)[1],
        &ServerMsg::Held {
            reason: HoldReason::PressureCritical
        }
    );
    assert_eq!(
        logs(&actions),
        vec![&EventKind::HoldStarted {
            session: s.clone(),
            reason: HoldReason::PressureCritical
        }]
    );

    let actions = b.tick(&input(secs(t0, 5), Pressure::Normal, Vec::new(), &[]));
    let msgs = sent(&actions, &s);
    assert!(
        msgs.iter().any(|m| matches!(
            m,
            ServerMsg::Granted {
                threads: 1..,
                after_timeout: false
            }
        )),
        "{msgs:?}"
    );
    assert!(logs(&actions).contains(&&EventKind::HoldReleased {
        session: s.clone(),
        waited_ms: 4000,
        timed_out: false,
    }));
    let later = b.tick(&input(secs(t0, 6), Pressure::Normal, Vec::new(), &[]));
    assert!(
        !sent(&later, &s)
            .iter()
            .any(|m| matches!(m, ServerMsg::Granted { .. }))
    );
}

#[test]
fn hold_times_out_after_max_hold() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    b.tick(&input(t0, Pressure::Critical, Vec::new(), &[]));
    let (s, _) = b.open(t0, req(100)).unwrap();
    b.fifo_ready(&s, PathBuf::from("/run/s.fifo"), t0);

    let early = b.tick(&input(secs(t0, 599), Pressure::Critical, Vec::new(), &[]));
    assert!(
        !sent(&early, &s)
            .iter()
            .any(|m| matches!(m, ServerMsg::Granted { .. }))
    );

    let actions = b.tick(&input(secs(t0, 600), Pressure::Critical, Vec::new(), &[]));
    assert!(sent(&actions, &s).contains(&&ServerMsg::Granted {
        threads: 1,
        after_timeout: true
    }));
    assert!(logs(&actions).contains(&&EventKind::HoldReleased {
        session: s.clone(),
        waited_ms: 600_000,
        timed_out: true,
    }));
}

#[test]
fn nested_and_never_hold_are_never_held() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    b.tick(&input(t0, Pressure::Critical, Vec::new(), &[]));
    let nested = OpenRequest {
        nested: true,
        ..req(100)
    };
    let never = OpenRequest {
        never_hold: true,
        ..req(101)
    };
    for request in [nested, never] {
        let (s, _) = b.open(t0, request).unwrap();
        let actions = b.fifo_ready(&s, PathBuf::from("/run/s.fifo"), t0);
        let msgs = sent(&actions, &s);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(
            msgs[1],
            ServerMsg::Granted {
                threads: 1..,
                after_timeout: false
            }
        ));
        assert_eq!(logs(&actions), Vec::<&EventKind>::new());
    }
}

#[test]
fn fair_share_between_two_sessions() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    let s1 = start(&mut b, t0, req(100), 500);
    let s2 = start(&mut b, t0, req(200), 600);
    let mut all = Vec::new();
    for t in 0..=20 {
        let mut procs = compiling(500);
        procs.extend(compiling(600));
        all.extend(b.tick(&input(
            secs(t0, t),
            Pressure::Normal,
            procs,
            &[(&s1, 0), (&s2, 0)],
        )));
    }
    let status = b.status(secs(t0, 20));
    assert_eq!(status.limit, 10);
    assert_eq!(fed(&all, &s1), 4);
    assert_eq!(fed(&all, &s2), 4);
    assert_eq!(token_events(&all, &s1).last(), Some(&4));
    assert_eq!(token_events(&all, &s2).last(), Some(&4));
    let slots: Vec<u32> = status.sessions.iter().map(|s| s.slots).collect();
    assert_eq!(slots, vec![5, 5]);
}

#[test]
fn test_jobs_take_from_the_pool() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    grow_to_cores(&mut b, t0, &s, 500);
    let test = OpenRequest {
        kind: Kind::Test,
        subcommand: "test".to_owned(),
        ..req(200)
    };
    let t = start(&mut b, secs(t0, 11), test, 700);
    let mut procs = compiling(500);
    procs.push(proc(700, 1, "cargo"));
    procs.push(proc(710, 700, "a_test-89ab"));
    procs.push(proc(711, 700, "a_test-cdef"));
    let actions = b.tick(&input(
        secs(t0, 11),
        Pressure::Normal,
        procs,
        &[(&s, 1), (&t, 0)],
    ));
    // Two test jobs leave 2 of the 4 slots: the build session keeps 2, so its
    // target drops from 3 to 1 and the one token back in its FIFO is drained.
    assert_eq!(drained(&actions, &s), 1);
    assert_eq!(token_events(&actions, &s), vec![1]);
}

#[test]
fn idle_session_gets_no_tokens() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    for t in 0..=6 {
        let actions = b.tick(&input(
            secs(t0, t),
            Pressure::Normal,
            vec![proc(500, 1, "cargo")],
            &[(&s, 0)],
        ));
        assert_eq!(fed(&actions, &s), 0);
    }
    let status = b.status(secs(t0, 6));
    assert_eq!(status.sessions[0].state, SessionState::Idle);
    assert_eq!(status.sessions[0].slots, 0);
}

#[test]
fn close_waits_for_cargo_exit() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let (s, _) = b.open(t0, req(100)).unwrap();
    b.fifo_ready(&s, PathBuf::from("/run/s.fifo"), t0);
    assert_eq!(
        b.started(&s, 500),
        vec![
            Action::WatchPid { pid: 500 },
            Action::Log(EventKind::SessionStarted {
                session: s.clone(),
                cargo_pid: 500
            }),
        ]
    );
    let actions = b.tick(&input(t0, Pressure::Normal, compiling(500), &[(&s, 0)]));
    assert_eq!(fed(&actions, &s), 1);

    assert_eq!(b.connection_closed(&s, secs(t0, 3)), Vec::<Action>::new());
    assert_eq!(b.pid_exited(999, secs(t0, 4)), Vec::<Action>::new());
    assert_eq!(
        b.pid_exited(500, secs(t0, 5)),
        vec![
            Action::RemoveFifo { session: s.clone() },
            Action::Log(EventKind::SessionClosed {
                session: s.clone(),
                tokens_recovered: 1,
                duration_ms: 5000,
            }),
        ]
    );
    assert_eq!(b.status(secs(t0, 5)).sessions, Vec::new());
}

#[test]
fn close_without_start_is_immediate() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let (s, _) = b.open(t0, req(100)).unwrap();
    b.fifo_ready(&s, PathBuf::from("/run/s.fifo"), t0);
    assert_eq!(
        b.connection_closed(&s, secs(t0, 2)),
        vec![
            Action::RemoveFifo { session: s.clone() },
            Action::Log(EventKind::SessionClosed {
                session: s.clone(),
                tokens_recovered: 0,
                duration_ms: 2000,
            }),
        ]
    );
    assert_eq!(b.connection_closed(&s, secs(t0, 3)), Vec::<Action>::new());
}

#[test]
fn too_many_sessions_is_an_error() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    for pid in 0..256 {
        b.open(t0, req(pid)).unwrap();
    }
    assert_eq!(b.open(t0, req(999)), Err(OpenError::TooManySessions));
}

#[test]
fn heartbeat_every_two_ticks() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let (s, _) = b.open(t0, req(100)).unwrap();
    b.fifo_ready(&s, PathBuf::from("/run/s.fifo"), t0);
    let beats: Vec<usize> = (1..=4)
        .map(|t| {
            let actions = b.tick(&input(secs(t0, t), Pressure::Normal, Vec::new(), &[]));
            sent(&actions, &s)
                .iter()
                .filter(|m| matches!(m, ServerMsg::Heartbeat))
                .count()
        })
        .collect();
    assert_eq!(beats, vec![0, 1, 0, 1]);
}

#[test]
fn reconfigure_logs_config_and_limit_change() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    grow_to_cores(&mut b, t0, &s, 500);
    let actions = b.reconfigure(cfg(2), vec!["x".to_owned()]);
    let events = logs(&actions);
    assert_eq!(
        events[0],
        &EventKind::ConfigLoaded {
            warnings: vec!["x".to_owned()]
        }
    );
    assert!(matches!(
        events[1],
        EventKind::LimitChanged {
            from: 4,
            to: 2,
            reason: LimitReason::Config,
            ..
        }
    ));
    let status = b.status(secs(t0, 10));
    assert_eq!((status.limit, status.cores), (2, 2));
    assert_eq!(status.reason, LimitReason::Config);
    assert_eq!(status.warnings, vec!["x".to_owned()]);
    let same = b.reconfigure(cfg(2), Vec::new());
    assert_eq!(logs(&same).len(), 1);
}

#[test]
fn status_reports_sessions_and_holds() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    let nested = OpenRequest {
        nested: true,
        ..req(100)
    };
    let s1 = start(&mut b, t0, nested, 500);
    b.tick(&input(t0, Pressure::Critical, compiling(500), &[(&s1, 0)]));
    let (s2, _) = b.open(t0, req(200)).unwrap();
    b.fifo_ready(&s2, PathBuf::from("/run/s2.fifo"), t0);

    let status = b.status(secs(t0, 3));
    assert_eq!(status.limit, 0);
    assert_eq!(status.reason, LimitReason::Critical);
    assert!(status.holding);
    assert_eq!((status.cores, status.memory), (10, 24 * GIB));
    assert_eq!(status.warnings, vec!["w".to_owned()]);
    assert_eq!(status.inputs.pressure, Pressure::Critical);
    let sessions: HashMap<&SessionId, &damper_proto::SessionStatus> =
        status.sessions.iter().map(|s| (&s.session, s)).collect();
    assert_eq!(status.sessions[0].session, s1);
    assert_eq!(sessions[&s1].state, SessionState::Compiling);
    assert_eq!(sessions[&s1].slots, 1);
    assert_eq!(sessions[&s1].age_secs, 3);
    assert_eq!(sessions[&s2].state, SessionState::Held { waited_secs: 3 });
    assert_eq!(sessions[&s2].subcommand, "build");
}

#[test]
fn memory_nearly_full_drops_running_sessions_to_floor() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    grow_to_cores(&mut b, t0, &s, 500);
    let mut tight = input(secs(t0, 11), Pressure::Warning, compiling(500), &[(&s, 1)]);
    tight.memory.available_pct = 5;
    let actions = b.tick(&tight);
    let status = b.status(secs(t0, 11));
    // The limit only halves, but the hold sends running sessions to their floor.
    assert_eq!(status.limit, 2);
    assert!(status.holding);
    assert_eq!(token_events(&actions, &s), vec![0]);
    assert_eq!(drained(&actions, &s), 1);
    // Cargo returns both tokens it held: both are drained, none kept for it.
    let mut later = input(secs(t0, 12), Pressure::Warning, compiling(500), &[(&s, 2)]);
    later.memory.available_pct = 5;
    let actions = b.tick(&later);
    assert_eq!(drained(&actions, &s), 2);
}
