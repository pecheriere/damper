use crate::{
    ClientMsg, Event, EventKind, HoldReason, InputsSummary, LimitReason, Pressure, ServerMsg,
    SessionId, SessionState, SessionStatus, Status,
};

pub fn sid(s: &str) -> SessionId {
    SessionId(s.into())
}

pub fn inputs() -> InputsSummary {
    InputsSummary {
        pressure: Pressure::Warning,
        available_pct: 40,
        swap_used: 1 << 20,
        swap_growing: true,
        build_footprint: 3 << 30,
        reserve: 2 << 30,
    }
}

pub fn status() -> Status {
    Status {
        limit: 6,
        reason: LimitReason::Headroom,
        cores: 10,
        memory: 32 << 30,
        inputs: inputs(),
        holding: false,
        sessions: vec![SessionStatus {
            session: sid("6543-1"),
            cwd: "/work/demo".into(),
            subcommand: "build".into(),
            state: SessionState::Held { waited_secs: 4 },
            slots: 2,
            tokens_outstanding: 1,
            age_secs: 12,
        }],
        warnings: vec!["w".into()],
    }
}

pub fn event(ts_ms: u64, kind: EventKind) -> Event {
    Event { ts_ms, kind }
}

pub fn limit_changed(ts_ms: u64) -> Event {
    event(
        ts_ms,
        EventKind::LimitChanged {
            from: 2,
            to: 3,
            reason: LimitReason::Growth,
            inputs: inputs(),
        },
    )
}

pub fn opened(ts_ms: u64, session: &SessionId) -> Event {
    event(
        ts_ms,
        EventKind::SessionOpened {
            session: session.clone(),
            cwd: "/w".into(),
            subcommand: "test".into(),
            shim_pid: 5,
            never_hold: false,
        },
    )
}

pub fn client_msgs() -> Vec<ClientMsg> {
    vec![
        ClientMsg::Open {
            cwd: "/w".into(),
            subcommand: "build".into(),
            shim_pid: 1,
            parent_pid: 2,
            nested: true,
        },
        ClientMsg::Started { cargo_pid: 9 },
        ClientMsg::Status,
        ClientMsg::Log {
            since_ms: Some(5),
            session: Some(sid("a-1")),
        },
        ClientMsg::Log {
            since_ms: None,
            session: None,
        },
        ClientMsg::Explain {
            session: sid("a-1"),
        },
    ]
}

fn event_kinds() -> Vec<EventKind> {
    let s = sid("a-1");
    vec![
        EventKind::LimitChanged {
            from: 1,
            to: 2,
            reason: LimitReason::Config,
            inputs: inputs(),
        },
        EventKind::SessionOpened {
            session: s.clone(),
            cwd: "/w".into(),
            subcommand: "check".into(),
            shim_pid: 3,
            never_hold: true,
        },
        EventKind::SessionStarted {
            session: s.clone(),
            cargo_pid: 4,
        },
        EventKind::SessionTokens {
            session: s.clone(),
            tokens: 2,
        },
        EventKind::HoldStarted {
            session: s.clone(),
            reason: HoldReason::MemoryNearlyFull,
        },
        EventKind::HoldReleased {
            session: s.clone(),
            waited_ms: 10,
            timed_out: true,
        },
        EventKind::SessionClosed {
            session: s,
            tokens_recovered: 1,
            duration_ms: 99,
        },
        EventKind::ShimFallback {
            reason: "r".into(),
            cwd: "/w".into(),
        },
        EventKind::ConfigLoaded {
            warnings: vec!["x".into()],
        },
        EventKind::ClientRejected { reason: "r".into() },
        EventKind::Error {
            message: "m".into(),
        },
    ]
}

pub fn server_msgs() -> Vec<ServerMsg> {
    let mut msgs = vec![
        ServerMsg::Opened {
            session: sid("a-1"),
            fifo: "/run/s.fifo".into(),
        },
        ServerMsg::Held {
            reason: HoldReason::PressureCritical,
        },
        ServerMsg::Granted {
            threads: 4,
            after_timeout: false,
        },
        ServerMsg::Heartbeat,
        ServerMsg::Status(status()),
        ServerMsg::Error {
            message: "e".into(),
        },
        ServerMsg::Events {
            events: Vec::new(),
            truncated: true,
        },
    ];
    msgs.push(ServerMsg::Events {
        events: event_kinds()
            .into_iter()
            .enumerate()
            .map(|(i, kind)| Event {
                ts_ms: i as u64,
                kind,
            })
            .collect(),
        truncated: false,
    });
    msgs
}
