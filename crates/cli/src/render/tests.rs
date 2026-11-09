use damper_proto::{
    Event, EventKind, HoldReason, InputsSummary, LimitReason, Pressure, SessionId, SessionState,
    SessionStatus, Status,
};

use super::{render_events, render_explain, render_status};

fn sid(s: &str) -> SessionId {
    SessionId(s.into())
}

fn inputs() -> InputsSummary {
    InputsSummary {
        pressure: Pressure::Warning,
        available_pct: 40,
        swap_used: 3 << 20,
        swap_growing: true,
        build_footprint: 3 << 30,
        reserve: 2 << 30,
    }
}

fn session(id: &str, cwd: &str, state: SessionState, slots: u32, age_secs: u64) -> SessionStatus {
    SessionStatus {
        session: sid(id),
        cwd: cwd.into(),
        subcommand: "build".into(),
        state,
        slots,
        tokens_outstanding: slots.saturating_sub(1),
        age_secs,
    }
}

fn ev(ts_ms: u64, kind: EventKind) -> Event {
    Event { ts_ms, kind }
}

const NOW: u64 = 10_000_000;

fn limit(from: u32, to: u32, reason: LimitReason) -> EventKind {
    EventKind::LimitChanged {
        from,
        to,
        reason,
        inputs: inputs(),
    }
}

fn opened(id: &str) -> EventKind {
    EventKind::SessionOpened {
        session: sid(id),
        cwd: "/work/demo".into(),
        subcommand: "build".into(),
        shim_pid: 4242,
        never_hold: false,
    }
}

#[test]
fn status_two_sessions_one_held() {
    let status = Status {
        limit: 6,
        reason: LimitReason::Headroom,
        cores: 10,
        memory: 32 << 30,
        inputs: inputs(),
        holding: true,
        sessions: vec![
            session(
                "6543-1",
                "/work/demo",
                SessionState::Held { waited_secs: 4 },
                0,
                4,
            ),
            session("6543-2", "/work/other", SessionState::Compiling, 3, 95),
        ],
        warnings: vec!["unknown key `foo` ignored".into()],
    };
    insta::assert_snapshot!(render_status(&status));
}

#[test]
fn status_idle_without_sessions() {
    let status = Status {
        limit: 2,
        reason: LimitReason::Startup,
        cores: 8,
        memory: 16 << 30,
        inputs: InputsSummary {
            pressure: Pressure::Normal,
            swap_growing: false,
            swap_used: 0,
            ..inputs()
        },
        holding: false,
        sessions: vec![],
        warnings: vec![],
    };
    insta::assert_snapshot!(render_status(&status));
}

fn mixed() -> Vec<Event> {
    vec![
        ev(NOW - 7_200_000, limit(2, 4, LimitReason::Growth)),
        ev(NOW - 3_700_000, opened("6543-1")),
        ev(
            NOW - 3_600_000,
            EventKind::SessionStarted {
                session: sid("6543-1"),
                cargo_pid: 777,
            },
        ),
        ev(
            NOW - 600_000,
            EventKind::SessionTokens {
                session: sid("6543-1"),
                tokens: 3,
            },
        ),
        ev(
            NOW - 120_000,
            EventKind::HoldStarted {
                session: sid("6543-2"),
                reason: HoldReason::PressureCritical,
            },
        ),
        ev(
            NOW - 90_000,
            EventKind::HoldReleased {
                session: sid("6543-2"),
                waited_ms: 30_000,
                timed_out: false,
            },
        ),
        ev(
            NOW - 60_000,
            EventKind::HoldReleased {
                session: sid("6543-3"),
                waited_ms: 600_000,
                timed_out: true,
            },
        ),
        ev(
            NOW - 30_000,
            EventKind::SessionClosed {
                session: sid("6543-1"),
                tokens_recovered: 2,
                duration_ms: 3_570_000,
            },
        ),
        ev(
            NOW - 20_000,
            EventKind::ShimFallback {
                reason: "daemon not running".into(),
                cwd: "/work/demo".into(),
            },
        ),
        ev(
            NOW - 15_000,
            EventKind::ConfigLoaded {
                warnings: vec!["unknown key `foo` ignored".into()],
            },
        ),
        ev(
            NOW - 10_000,
            EventKind::ClientRejected {
                reason: "unsupported protocol version 9".into(),
            },
        ),
        ev(
            NOW - 5_000,
            EventKind::Error {
                message: "sampling failed".into(),
            },
        ),
        ev(NOW, limit(4, 2, LimitReason::Warning)),
    ]
}

#[test]
fn events_mixed() {
    insta::assert_snapshot!(render_events(&mixed(), NOW, false));
}

#[test]
fn events_truncated_notice_and_empty() {
    insta::assert_snapshot!(render_events(&mixed()[..1], NOW, true));
    insta::assert_snapshot!(render_events(&[], NOW, false));
}

#[test]
fn explain_held_then_ran() {
    let id = sid("6543-2");
    let events = vec![
        ev(NOW - 130_000, opened("6543-2")),
        ev(NOW - 125_000, limit(4, 2, LimitReason::Critical)),
        ev(
            NOW - 120_000,
            EventKind::HoldStarted {
                session: id.clone(),
                reason: HoldReason::PressureCritical,
            },
        ),
        ev(
            NOW - 90_000,
            EventKind::HoldReleased {
                session: id.clone(),
                waited_ms: 30_000,
                timed_out: false,
            },
        ),
        ev(
            NOW - 89_000,
            EventKind::SessionStarted {
                session: id.clone(),
                cargo_pid: 901,
            },
        ),
        ev(
            NOW - 1_000,
            EventKind::SessionClosed {
                session: id.clone(),
                tokens_recovered: 1,
                duration_ms: 129_000,
            },
        ),
    ];
    insta::assert_snapshot!(render_explain(&id, &events, NOW));
}

#[test]
fn explain_unknown_session() {
    assert_eq!(
        render_explain(&sid("nope"), &[], NOW),
        "No events for session nope.\n"
    );
}

#[test]
fn clean_replaces_control_and_format_characters() {
    for c in [
        '\u{1b}',
        '\u{7}',
        '\u{202e}',
        '\u{2066}',
        '\u{200b}',
        '\u{feff}',
        '\u{e0041}',
        '\u{2028}',
    ] {
        assert_eq!(super::clean(&format!("a{c}b")), "a?b", "{c:?}");
    }
    assert_eq!(super::clean("plain é 日本"), "plain é 日本");
}
