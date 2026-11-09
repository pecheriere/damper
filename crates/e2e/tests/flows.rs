//! Sessions, tokens and sharing, as a build sees them.

use crate::harness::{Env, Report, wait_until};
use damper_proto::{EventKind, SessionId};

#[test]
fn build_gets_tokens_and_returns_them() {
    let mut env = Env::new();
    env.start_daemon();
    let report = env.file("r");
    let mut build = env.build(&[
        "build",
        "jobs",
        "4",
        "2000",
        "--report",
        report.to_str().unwrap(),
    ]);
    let session = env.session_of(build.pid());
    assert_eq!(build.wait().code(), Some(0));
    let closed = env.wait_closed(&session);
    let EventKind::SessionClosed {
        tokens_recovered, ..
    } = closed.kind
    else {
        unreachable!()
    };
    // The daemon logs each change of the session's token target and feeds
    // up to it; drains only follow a lower target. So the last target logged
    // is what the session held when it closed.
    let targets: Vec<u32> = env
        .events()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::SessionTokens { session: s, tokens } if s == session => Some(tokens),
            _ => None,
        })
        .collect();
    assert!(
        targets.iter().any(|t| *t > 0),
        "no token was fed: {targets:?}"
    );
    assert_eq!(Some(&tokens_recovered), targets.last(), "{targets:?}");
    assert!(
        Report::read(&report).peak >= 2,
        "the build never ran a job on a token"
    );
}

#[test]
fn two_builds_share_fairly() {
    let mut env = Env::new();
    env.start_daemon();
    let barrier = env.file("go");
    let reports = [env.file("r1"), env.file("r2")];
    let mut builds: Vec<_> = reports
        .iter()
        .map(|r| {
            env.build(&[
                "build",
                "jobs",
                "8",
                "2000",
                "--report",
                r.to_str().unwrap(),
                "--wait",
                barrier.to_str().unwrap(),
            ])
        })
        .collect();
    let sessions: Vec<SessionId> = builds.iter().map(|b| env.session_of(b.pid())).collect();
    // Start the jobs together, once the daemon has settled on a split: both
    // compiling, holding token counts at most one apart.
    wait_until("both sessions to compile with a settled split", || {
        let status = env.status();
        let given: Vec<u32> = sessions
            .iter()
            .filter_map(|id| {
                status
                    .sessions
                    .iter()
                    .find(|s| &s.session == id && s.state == damper_proto::SessionState::Compiling)
                    .map(|s| s.tokens_outstanding)
            })
            .collect();
        (given.len() == 2 && given[0].abs_diff(given[1]) <= 1).then_some(())
    });
    std::fs::write(&barrier, b"").unwrap();
    for b in &mut builds {
        assert_eq!(b.wait().code(), Some(0));
    }
    let [a, b] = reports.map(|r| Report::read(&r));
    // Both run from the later start until the first has claimed its last
    // job; after that one of them can't use more slots, so the other may
    // rightly get them.
    let from = a.start.max(b.start);
    let to = a.claimed.min(b.claimed);
    assert!(from < to, "the builds never ran together");
    let (pa, pb) = (a.peak_between(from, to), b.peak_between(from, to));
    assert!(pa.abs_diff(pb) <= 1, "unfair split: {pa} against {pb}");
    assert!(pa.max(pb) >= 2, "no build ran a job on a token");
}

#[test]
fn passthrough_never_opens_session() {
    let mut env = Env::new();
    env.start_daemon();
    let mut metadata = env.build(&["metadata"]);
    assert_eq!(metadata.wait().code(), Some(0));
    // A build after it gives the log a point where the metadata run, had it
    // opened a session, would already show.
    let mut build = env.build(&["build", "jobs", "1", "10"]);
    let session = env.session_of(build.pid());
    assert_eq!(build.wait().code(), Some(0));
    env.wait_closed(&session);
    let opened: Vec<_> = env
        .events()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::SessionOpened { session, .. } => Some(session),
            _ => None,
        })
        .collect();
    assert_eq!(opened, [session]);
}
