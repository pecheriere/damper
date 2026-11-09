//! Spec §11's fail-open checks: whatever happens to damper, the build
//! finishes or fails exactly as cargo would.

use crate::harness::{Env, Report, alive, signal};
use damper_proto::EventKind;
use nix::sys::signal::Signal;
use std::os::unix::process::ExitStatusExt;
use std::time::Duration;

/// `jobs 4 3000` takes at most 12 s on its implicit slot alone.
const SERIAL: Duration = Duration::from_secs(12);
const SLACK: Duration = Duration::from_secs(2);

#[test]
fn kill_damperd_mid_build() {
    let mut env = Env::new();
    env.start_daemon();
    let mut build = env.build(&["build", "jobs", "4", "3000"]);
    let session = env.session_of(build.pid());
    env.wait_compiling(&session);
    env.stop_daemon(Signal::SIGKILL);
    assert_eq!(build.wait().code(), Some(0));
    let took = build.started.elapsed();
    assert!(took <= SERIAL + SLACK, "the build took {took:?}");
    assert_eq!(build.stderr(), "");
}

#[test]
fn sigkill_shim_frees_session_after_cargo_exits() {
    let mut env = Env::new();
    env.start_daemon();
    let report = env.file("r");
    let mut build = env.build(&[
        "build",
        "jobs",
        "2",
        "3000",
        "--report",
        report.to_str().unwrap(),
    ]);
    let session = env.session_of(build.pid());
    let cargo = env.cargo_pid(&session);
    env.wait_compiling(&session);
    signal(build.pid(), Signal::SIGKILL);
    assert_eq!(build.wait().signal(), Some(Signal::SIGKILL as i32));
    // The shim is gone and its connection with it; cargo still runs, so the
    // session must still be there.
    let open = env.status().sessions.iter().any(|s| s.session == session);
    assert!(alive(cargo), "cargo finished before the check");
    assert!(open, "the session closed while cargo still ran");
    let finished = Report::read(&report).end;
    let closed = env.wait_closed(&session);
    assert!(
        closed.ts_ms >= finished,
        "closed at {} before cargo finished at {finished}",
        closed.ts_ms
    );
}

#[test]
fn sigkill_cargo_closes_session() {
    let mut env = Env::new();
    env.start_daemon();
    let mut build = env.build(&["build", "jobs", "2", "3000"]);
    let session = env.session_of(build.pid());
    let cargo = env.cargo_pid(&session);
    env.wait_compiling(&session);
    signal(cargo, Signal::SIGKILL);
    // The shim dies the way cargo did.
    assert_eq!(build.wait().signal(), Some(Signal::SIGKILL as i32));
    env.wait_closed(&session);
    let open = env.status().sessions;
    assert!(open.iter().all(|s| s.session != session), "{open:?}");
}

#[test]
fn broken_config_uses_defaults() {
    let mut env = Env::new();
    std::fs::write(env.state.join("config.toml"), "cores = = [[not toml\n").unwrap();
    env.start_daemon();
    let mut build = env.build(&["build", "jobs", "2", "300"]);
    let session = env.session_of(build.pid());
    assert_eq!(build.wait().code(), Some(0));
    assert_eq!(build.stderr(), "");
    env.wait_closed(&session);
    let warnings = env.status().warnings;
    assert!(!warnings.is_empty(), "no warning in status");
    let logged = env
        .events()
        .into_iter()
        .any(|e| matches!(e.kind, EventKind::ConfigLoaded { warnings } if !warnings.is_empty()));
    assert!(logged, "no warning in the log");
}

#[test]
fn damperd_restart_keeps_running_build() {
    let mut env = Env::new();
    env.start_daemon();
    let mut build = env.build(&["build", "jobs", "4", "3000"]);
    let session = env.session_of(build.pid());
    env.wait_compiling(&session);
    let fifos = env.fifos();
    assert_eq!(fifos.len(), 1, "{fifos:?}");
    env.stop_daemon(Signal::SIGTERM);
    env.start_daemon();
    // The new daemon swept the runtime dir at start: the live build's FIFO
    // must have survived it.
    assert!(build.running(), "the build ended before the check");
    assert!(fifos[0].exists(), "the new daemon deleted a live FIFO");
    assert_eq!(build.wait().code(), Some(0));
    let took = build.started.elapsed();
    assert!(took <= SERIAL + SLACK, "the build took {took:?}");
    assert!(!fifos[0].exists(), "the FIFO outlived its build");
    assert_eq!(build.stderr(), "");
}

#[test]
fn no_daemon_runs_cargo_silently() {
    let mut env = Env::new();
    let mut failing = env.build(&["build", "exit", "7"]);
    assert_eq!(failing.wait().code(), Some(7));
    assert_eq!(failing.stderr(), "");
    let mut jobs = env.build(&["build", "jobs", "2", "100"]);
    assert_eq!(jobs.wait().code(), Some(0));
    assert_eq!(jobs.stderr(), "");
}

// Review Focus 3, while cargo runs. A hold needs real memory pressure here
// (the real daemon reads the real machine), so SIGINT while held is covered
// by the shim's tests against a scripted daemon.

#[test]
fn sigterm_while_cargo_runs_closes_session() {
    let mut env = Env::new();
    env.start_daemon();
    let mut build = env.build(&["build", "jobs", "2", "3000"]);
    let session = env.session_of(build.pid());
    env.wait_compiling(&session);
    signal(build.pid(), Signal::SIGTERM);
    // The shim forwards SIGTERM to cargo and dies the way cargo did.
    assert_eq!(build.wait().signal(), Some(Signal::SIGTERM as i32));
    env.wait_closed(&session);
    let open = env.status().sessions;
    assert!(open.iter().all(|s| s.session != session), "{open:?}");
}
