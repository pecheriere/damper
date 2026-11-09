//! Spec §11's fail-open checks: whatever happens to damper, the build
//! finishes or fails exactly as cargo would.

use crate::harness::{Build, Env, FifoTap, Report, alive, signal, wait_until};
use damper_proto::EventKind;
use nix::sys::signal::Signal;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;

/// The jobs of [`DaemonLoss`]'s build: more than the two the daemon allows it.
const JOBS: &str = "6";

/// A build whose daemon goes away while it runs. With `cores = 2` the daemon
/// gives it at most one token, so at most two jobs run at once on what the
/// daemon fed. Its jobs wait on a barrier until the daemon is gone.
struct DaemonLoss {
    env: Env,
    build: Build,
    fifo: FifoTap,
    report: PathBuf,
    barrier: PathBuf,
}

impl DaemonLoss {
    fn start() -> Self {
        let mut env = Env::new();
        env.config("cores = 2\n");
        env.start_daemon();
        let report = env.file("r");
        let barrier = env.file("go");
        let build = env.build(&[
            "build",
            "jobs",
            JOBS,
            "2000",
            "--report",
            report.to_str().unwrap(),
            "--wait",
            barrier.to_str().unwrap(),
        ]);
        let session = env.session_of(build.pid());
        let fifo = env.tap_fifo();
        env.wait_compiling(&session);
        Self {
            env,
            build,
            fifo,
            report,
            barrier,
        }
    }

    /// Checks that the shim fed the FIFO itself once the daemon was gone,
    /// then lets the jobs run and checks the build used those tokens.
    ///
    /// No job has run yet, so the FIFO holds what the daemon fed, at most
    /// one token, plus what the shim fed: one per core beyond the first.
    /// Two or more tokens can only come from the shim (on a machine with at
    /// least three cores), and more than two jobs at once can only run on them.
    fn finish_on_self_feed(&mut self) {
        wait_until("the shim to feed the FIFO", || {
            (self.fifo.tokens() >= 2).then_some(())
        });
        std::fs::write(&self.barrier, b"").unwrap();
        assert_eq!(self.build.wait().code(), Some(0));
        assert_eq!(self.build.stderr(), "");
        let peak = Report::read(&self.report).peak;
        assert!(peak >= 3, "only {peak} jobs at once after the daemon went");
        // The shim unlinks a FIFO it fed itself once cargo is done.
        assert!(!self.fifo.path.exists(), "the FIFO outlived its build");
    }

    /// Waits until `damper log` shows the shim's record of feeding the FIFO
    /// itself (spec §7.1, §11).
    fn wait_self_feed_logged(&self) {
        wait_until("the self-feed in the log", || {
            self.env
                .events()
                .into_iter()
                .find(|e| is_shim_self_feed(&e.kind))
        });
    }
}

/// The shim's own record of a mid-build self-feed.
fn is_shim_self_feed(kind: &EventKind) -> bool {
    matches!(kind, EventKind::ShimFallback { reason, .. } if reason.contains("mid-build"))
}

#[test]
fn kill_damperd_mid_build() {
    let mut run = DaemonLoss::start();
    run.env.stop_daemon(Signal::SIGKILL);
    run.finish_on_self_feed();
    // The next daemon moves the shim's record into the log.
    run.env.start_daemon();
    run.wait_self_feed_logged();
}

/// A daemon that hangs rather than dies (ruling R22): the shim feeds the FIFO
/// after the heartbeat timeout and says so. Once the daemon recovers it
/// counts those tokens as given and drains them back, and the shim's record
/// reaches the log while the daemon runs.
#[test]
fn hung_damperd_takes_back_the_self_fed_tokens() {
    let mut run = DaemonLoss::start();
    let daemon = run.env.daemon_pid();
    signal(daemon, Signal::SIGSTOP);
    wait_until("the shim to feed the FIFO", || {
        (run.fifo.tokens() >= 2).then_some(())
    });
    signal(daemon, Signal::SIGCONT);
    wait_until("the daemon to count the self-feed", || {
        run.env.events().into_iter().find(|e| {
            matches!(&e.kind, EventKind::ShimFallback { reason, .. } if reason.contains("now counts them as given"))
        })
    });
    // With `cores = 2` the session's share is one token: the rest go back.
    wait_until("the daemon to drain the self-fed tokens", || {
        (run.fifo.tokens() <= 1).then_some(())
    });
    run.wait_self_feed_logged();
    std::fs::write(&run.barrier, b"").unwrap();
    assert_eq!(run.build.wait().code(), Some(0));
    assert_eq!(run.build.stderr(), "");
    // The share's token stayed: counting the self-feed twice would have
    // drained it too and left cargo only its implicit slot.
    let peak = Report::read(&run.report).peak;
    assert!(peak >= 2, "only {peak} job at once after the recovery");
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
    let mut run = DaemonLoss::start();
    run.env.stop_daemon(Signal::SIGTERM);
    run.env.start_daemon();
    // The new daemon swept the runtime dir at start: the live build's FIFO
    // must have survived it. The jobs wait on the barrier, so the build
    // can't have ended yet.
    assert!(run.build.running(), "the build ended before the check");
    assert!(run.fifo.path.exists(), "the new daemon deleted a live FIFO");
    run.finish_on_self_feed();
    run.wait_self_feed_logged();
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

// Review Focus 3, while cargo runs. A hold needs Critical memory pressure,
// or Warning or growing swap, which the real daemon reads from the real
// machine and no config can force, so signals while held are covered by the
// shim's tests against a scripted daemon.

#[test]
fn sigterm_while_cargo_runs_closes_session() {
    signal_while_cargo_runs(Signal::SIGTERM);
}

/// The e2e build isn't in its terminal's foreground group (it has none, or
/// runs in a group of its own), so the shim forwards `SIGINT` (R18).
#[test]
fn sigint_while_cargo_runs_closes_session() {
    signal_while_cargo_runs(Signal::SIGINT);
}

/// Sends `sig` to the shim while cargo runs two jobs, one on a token: the
/// shim forwards it, cargo dies holding that token, and the session closes
/// with every token it was given recovered.
fn signal_while_cargo_runs(sig: Signal) {
    let mut env = Env::new();
    env.start_daemon();
    let report = env.file("r");
    // Jobs far longer than the test needs, so the token job still runs when
    // the signal lands.
    let mut build = env.build(&[
        "build",
        "jobs",
        "2",
        "60000",
        "--report",
        report.to_str().unwrap(),
    ]);
    let session = env.session_of(build.pid());
    let fifo = env.tap_fifo();
    Report::wait_running(&report, 2);
    signal(build.pid(), sig);
    // The shim forwards the signal to cargo and dies the way cargo did.
    assert_eq!(build.wait().signal(), Some(sig as i32));
    let tokens_recovered = env.tokens_recovered(&session);
    assert!(!fifo.path.exists(), "the FIFO outlived its session");
    let open = env.status().sessions;
    assert!(open.iter().all(|s| s.session != session), "{open:?}");
    // Both jobs were claimed, so cargo held exactly one token when it died;
    // every other token the daemon still counted is in the FIFO.
    assert_eq!(tokens_recovered, fifo.tokens() + 1);
}
