//! Session lifecycle edge cases: the session cap, close orderings, held
//! sessions closing, heartbeats after close, and early reconfiguration.

use std::path::PathBuf;
use std::time::Instant;

use damper_proto::{EventKind, LimitReason, Pressure, ServerMsg, SessionId};

use super::{broker, cfg, compiling, input, logs, req, secs, sent, start};
use crate::broker::{Action, OpenError};

fn closed(s: &SessionId, tokens_recovered: u32, duration_ms: u64) -> Vec<Action> {
    vec![
        Action::RemoveFifo { session: s.clone() },
        Action::Log(EventKind::SessionClosed {
            session: s.clone(),
            tokens_recovered,
            duration_ms,
        }),
    ]
}

#[test]
fn closing_frees_a_place_under_the_cap() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    b.tick(&input(t0, Pressure::Critical, Vec::new(), &[]));
    let (held, _) = b.open(t0, req(1)).unwrap();
    b.fifo_ready(&held, PathBuf::from("/run/h.fifo"), t0);
    let running = start(&mut b, t0, req(2), 500);
    assert_eq!(b.connection_closed(&running, t0), Vec::<Action>::new());
    for pid in 3..=256 {
        b.open(t0, req(pid)).unwrap();
    }
    // The held session and the closed one whose cargo still runs both count.
    assert_eq!(b.open(t0, req(999)), Err(OpenError::TooManySessions));

    assert_eq!(b.pid_exited(500, t0), closed(&running, 0, 0));
    b.open(t0, req(999)).unwrap();
    assert_eq!(b.open(t0, req(1000)), Err(OpenError::TooManySessions));

    assert_eq!(b.connection_closed(&held, t0), closed(&held, 0, 0));
    b.open(t0, req(1000)).unwrap();
}

#[test]
fn cargo_exit_before_close_closes_on_the_close() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    assert_eq!(b.pid_exited(500, secs(t0, 1)), Vec::<Action>::new());
    assert_eq!(b.status(secs(t0, 1)).sessions.len(), 1);
    assert_eq!(b.connection_closed(&s, secs(t0, 2)), closed(&s, 0, 2000));
    assert_eq!(b.status(secs(t0, 2)).sessions, Vec::new());
}

#[test]
fn held_session_that_closes_is_never_granted() {
    let t0 = Instant::now();
    let mut b = broker(10, t0);
    b.tick(&input(t0, Pressure::Critical, Vec::new(), &[]));
    let (s, _) = b.open(t0, req(100)).unwrap();
    b.fifo_ready(&s, PathBuf::from("/run/s.fifo"), t0);
    assert_eq!(b.connection_closed(&s, secs(t0, 3)), closed(&s, 0, 3000));
    let actions = b.tick(&input(secs(t0, 4), Pressure::Normal, Vec::new(), &[]));
    assert_eq!(sent(&actions, &s), Vec::<&ServerMsg>::new());
    assert!(
        !logs(&actions)
            .iter()
            .any(|e| matches!(e, EventKind::HoldReleased { .. }))
    );
}

#[test]
fn no_heartbeat_after_the_connection_closed() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    let s = start(&mut b, t0, req(100), 500);
    assert_eq!(b.connection_closed(&s, t0), Vec::<Action>::new());
    for t in 1..=4 {
        let actions = b.tick(&input(
            secs(t0, t),
            Pressure::Normal,
            compiling(500),
            &[(&s, 0)],
        ));
        assert_eq!(sent(&actions, &s), Vec::<&ServerMsg>::new());
    }
}

#[test]
fn reconfigure_before_the_first_tick_replaces_the_startup_config() {
    let t0 = Instant::now();
    let mut b = broker(4, t0);
    assert_eq!(
        b.reconfigure(cfg(2), vec!["x".to_owned()]),
        vec![Action::Log(EventKind::ConfigLoaded {
            warnings: vec!["x".to_owned()]
        })]
    );
    let actions = b.tick(&input(t0, Pressure::Normal, Vec::new(), &[]));
    let events = logs(&actions);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, EventKind::ConfigLoaded { .. })),
        "{events:?}"
    );
    assert!(matches!(
        events[0],
        EventKind::LimitChanged {
            from: 0,
            reason: LimitReason::Startup,
            ..
        }
    ));
    let status = b.status(t0);
    assert_eq!(status.reason, LimitReason::Startup);
    assert_eq!(status.warnings, vec!["x".to_owned()]);
}

#[test]
fn a_lowered_cap_refuses_the_next_open() {
    let t0 = Instant::now();
    let mut b = broker(4, t0).with_max_sessions(2);
    b.open(t0, req(1)).unwrap();
    b.open(t0, req(2)).unwrap();
    assert_eq!(b.open(t0, req(3)), Err(OpenError::TooManySessions));
}
