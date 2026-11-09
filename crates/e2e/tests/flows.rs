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
    let fifo = env.tap_fifo();
    assert_eq!(build.wait().code(), Some(0));
    let tokens_recovered = env.tokens_recovered(&session);
    // Every token in circulation was written to the FIFO by the daemon, and
    // the daemon counts it as given until it drains it back out; cargo has
    // returned all it took. So whatever targets the daemon set and however
    // often it fed and drained, the tokens left in the FIFO are exactly the
    // ones it still counted as given when the session closed.
    assert!(!fifo.path.exists(), "the FIFO outlived its session");
    assert_eq!(tokens_recovered, fifo.tokens());
    let targets = env.targets(&session);
    let most = targets.iter().copied().max().unwrap_or(0);
    // The daemon never gives more than a target it set; cargo can't hold
    // more than it was given.
    assert!(tokens_recovered <= most, "{tokens_recovered} > {targets:?}");
    let peak = Report::read(&report).peak;
    assert!(peak >= 2, "the build never ran a job on a token");
    assert!(peak - 1 <= most, "ran {peak} jobs with targets {targets:?}");
}

#[test]
fn two_builds_share_fairly() {
    let mut env = Env::new();
    // The pool never outgrows the machine's cores, so cap it at four: then
    // every machine with four cores or more settles on the same split, and a
    // smaller one (GitHub's macOS runner has three) on the most it can give.
    let cores = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(4);
    env.config(&format!("cores = {cores}\n"));
    let cores = u32::try_from(cores).unwrap();
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
    // compiling, sharing every core between them, their slots at most one
    // apart. With four cores that is a token each on top of each implicit
    // slot; with three, one build gets the only token. (Status's
    // `tokens_outstanding` counts only tokens cargo took, none before the
    // barrier.)
    wait_until("both sessions to compile, sharing every core", || {
        let status = env.status();
        let slots: Vec<u32> = sessions
            .iter()
            .filter_map(|id| {
                status
                    .sessions
                    .iter()
                    .find(|s| &s.session == id && s.state == damper_proto::SessionState::Compiling)
                    .map(|s| s.slots)
            })
            .collect();
        let settled = slots.len() == 2
            && slots.iter().sum::<u32>() >= cores
            && slots[0].abs_diff(slots[1]) <= 1;
        settled.then_some(())
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
    // The larger share, a token on top of the implicit slot once there are
    // three cores or more.
    assert!(
        pa.max(pb) >= cores.div_ceil(2),
        "no build ran a job on a token"
    );
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
