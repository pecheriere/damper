//! Opening a session with damperd and waiting for its first slot
//! (spec §5.3 steps 1–2, §6.3).

use crate::hold::{hold_line, timeout_line};
use damper_core::classify::{Invocation, Kind};
use damper_proto::paths::Paths;
use damper_proto::{Client, ClientMsg, HoldReason, ProtoError, ServerMsg, SessionId};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// From connecting to `Opened`; past it the shim falls back.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
/// Silence after which the daemon counts as dead (it sends a heartbeat
/// every 2 s).
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
/// Time between hold lines.
pub const HOLD_NOTICE: Duration = Duration::from_secs(30);

/// The result of trying to open a session.
pub enum Opened {
    /// The session has its first slot: run cargo.
    Granted {
        /// The daemon's session id.
        session: SessionId,
        /// The session's jobserver FIFO.
        fifo: PathBuf,
        /// Test threads for this run (spec §5.6).
        threads: u32,
        /// The connection, kept open for the whole build.
        client: Client,
    },
    /// No session: the shim falls back. The reason never holds arguments or
    /// environment.
    Unavailable(String),
}

/// Opens a session for `inv` and waits for its first slot, printing hold
/// lines to `out` while the daemon holds it.
pub fn open_session(paths: &Paths, inv: &Invocation, cwd: &Path, out: &mut dyn Write) -> Opened {
    match handshake(paths, inv, cwd) {
        Ok((client, session, fifo)) => wait_for_grant(client, session, fifo, out),
        Err(reason) => Opened::Unavailable(reason),
    }
}

/// Connects, sends `Open` and reads `Opened`, all within `CONNECT_TIMEOUT`.
fn handshake(
    paths: &Paths,
    inv: &Invocation,
    cwd: &Path,
) -> Result<(Client, SessionId, PathBuf), String> {
    let start = Instant::now();
    let mut client =
        Client::connect(&paths.socket, CONNECT_TIMEOUT).map_err(|e| describe("connecting", &e))?;
    let open = ClientMsg::Open {
        cwd: cwd.to_path_buf(),
        subcommand: inv.subcommand.clone().unwrap_or_default(),
        shim_pid: std::process::id(),
        parent_pid: std::os::unix::process::parent_id(),
        // A nested invocation never reaches a session (see `decide`).
        nested: false,
    };
    client
        .send(&open)
        .map_err(|e| describe("sending open", &e))?;
    loop {
        let left = CONNECT_TIMEOUT.saturating_sub(start.elapsed());
        if left.is_zero() {
            return Err(describe("waiting for opened", &ProtoError::Timeout));
        }
        match client.recv(Some(left)) {
            Ok(ServerMsg::Opened { session, fifo }) => return Ok((client, session, fifo)),
            Ok(ServerMsg::Error { message }) => return Err(refused(&message)),
            Ok(_) => {}
            Err(e) => return Err(describe("waiting for opened", &e)),
        }
    }
}

/// Waits for `Granted`, printing a hold line when a hold starts and every
/// `HOLD_NOTICE` after. A daemon that closes or goes silent for
/// `HEARTBEAT_TIMEOUT` makes the session unavailable.
fn wait_for_grant(
    mut client: Client,
    session: SessionId,
    fifo: PathBuf,
    out: &mut dyn Write,
) -> Opened {
    let mut last_message = Instant::now();
    let mut hold: Option<Hold> = None;
    loop {
        if let Some(h) = &mut hold {
            h.notice_if_due(out, &session);
        }
        let silent_until = last_message + HEARTBEAT_TIMEOUT;
        let deadline = hold
            .as_ref()
            .map_or(silent_until, |h| h.next_notice.min(silent_until));
        let msg = match client.recv(Some(deadline.saturating_duration_since(Instant::now()))) {
            Ok(msg) => msg,
            Err(ProtoError::Timeout) if Instant::now() < silent_until => continue,
            Err(e) => return Opened::Unavailable(describe("waiting for a slot", &e)),
        };
        last_message = Instant::now();
        match msg {
            ServerMsg::Held { reason } => {
                if let Some(h) = &mut hold {
                    h.reason = reason;
                } else {
                    hold = Some(Hold::start(reason, out, &session));
                }
            }
            ServerMsg::Granted {
                threads,
                after_timeout,
            } => {
                if after_timeout {
                    let waited = hold.map_or(Duration::ZERO, |h| h.since.elapsed());
                    say(out, &timeout_line(waited, &session));
                }
                return Opened::Granted {
                    session,
                    fifo,
                    threads,
                    client,
                };
            }
            ServerMsg::Error { message } => return Opened::Unavailable(refused(&message)),
            _ => {}
        }
    }
}

/// A hold in progress.
struct Hold {
    /// The latest reason the daemon gave.
    reason: HoldReason,
    since: Instant,
    next_notice: Instant,
}

impl Hold {
    /// Starts a hold, printing its first line at once.
    fn start(reason: HoldReason, out: &mut dyn Write, session: &SessionId) -> Self {
        say(out, &hold_line(reason, Duration::ZERO, session));
        let since = Instant::now();
        Self {
            reason,
            since,
            next_notice: since + HOLD_NOTICE,
        }
    }

    fn notice_if_due(&mut self, out: &mut dyn Write, session: &SessionId) {
        let now = Instant::now();
        if now >= self.next_notice {
            say(out, &hold_line(self.reason, now - self.since, session));
            self.next_notice = now + HOLD_NOTICE;
        }
    }
}

fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn refused(message: &str) -> String {
    format!("daemon refused the session: {message}")
}

/// A fallback reason for `error` during `step`, never including arguments
/// or environment.
fn describe(step: &str, error: &ProtoError) -> String {
    match error {
        ProtoError::Io(e) => format!("{step}: socket error: {}", e.kind()),
        other => format!("{step}: {other}"),
    }
}

/// The environment added to cargo's: the session FIFO as the jobserver,
/// the session id, and the test thread count unless the user set it.
#[must_use]
pub fn session_env(
    fifo: &Path,
    session: &SessionId,
    inv: &Invocation,
    threads: u32,
) -> Vec<(OsString, OsString)> {
    let mut flags = OsString::from("-j --jobserver-auth=fifo:");
    flags.push(fifo);
    let mut env = vec![
        (OsString::from("CARGO_MAKEFLAGS"), flags.clone()),
        // Replaces a parent make's MAKEFLAGS: damper's pool is machine-wide.
        (OsString::from("MAKEFLAGS"), flags),
        (OsString::from("DAMPER_SESSION"), OsString::from(&session.0)),
    ];
    let thread_var = match inv.kind {
        _ if inv.user_threads => None,
        Kind::Nextest => Some("NEXTEST_TEST_THREADS"),
        Kind::Test => Some("RUST_TEST_THREADS"),
        Kind::Build | Kind::Run | Kind::PassThrough => None,
    };
    if let Some(name) = thread_var {
        env.push((name.into(), threads.max(1).to_string().into()));
    }
    env
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test")]
mod tests {
    use super::{Hold, session_env};
    use damper_core::classify::{Invocation, Kind};
    use damper_proto::{HoldReason, SessionId};
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn env_for(kind: Kind, user_threads: bool) -> Vec<(String, String)> {
        let inv = Invocation {
            kind,
            subcommand: Some("x".to_owned()),
            user_threads,
        };
        session_env(
            Path::new("/s/run/a.fifo"),
            &SessionId("s1".to_owned()),
            &inv,
            3,
        )
        .into_iter()
        .map(|(k, v): (OsString, OsString)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
    }

    fn base() -> Vec<(String, String)> {
        let flags = "-j --jobserver-auth=fifo:/s/run/a.fifo".to_owned();
        vec![
            ("CARGO_MAKEFLAGS".to_owned(), flags.clone()),
            ("MAKEFLAGS".to_owned(), flags),
            ("DAMPER_SESSION".to_owned(), "s1".to_owned()),
        ]
    }

    fn with(name: &str) -> Vec<(String, String)> {
        let mut env = base();
        env.push((name.to_owned(), "3".to_owned()));
        env
    }

    #[test]
    fn session_env_sets_threads_only_when_user_did_not() {
        assert_eq!(env_for(Kind::Nextest, false), with("NEXTEST_TEST_THREADS"));
        assert_eq!(env_for(Kind::Nextest, true), base());
        assert_eq!(env_for(Kind::Test, false), with("RUST_TEST_THREADS"));
        assert_eq!(env_for(Kind::Test, true), base());
        assert_eq!(env_for(Kind::Build, false), base());
        assert_eq!(env_for(Kind::Run, false), base());
    }

    #[test]
    fn hold_notice_repeats_with_the_latest_reason() {
        let now = Instant::now();
        let mut hold = Hold {
            reason: HoldReason::MemoryNearlyFull,
            since: now.checked_sub(Duration::from_secs(31)).unwrap(),
            next_notice: now.checked_sub(Duration::from_secs(1)).unwrap(),
        };
        let mut out = Vec::new();
        hold.notice_if_due(&mut out, &SessionId("s1".to_owned()));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "damper: holding this build, memory nearly full (waited 31s). \
             Run 'damper status' for details. Session s1.\n"
        );
        assert!(hold.next_notice > now + Duration::from_secs(28));
        let mut again = Vec::new();
        hold.notice_if_due(&mut again, &SessionId("s1".to_owned()));
        assert!(again.is_empty(), "not due yet");
    }

    #[test]
    fn hold_notice_after_a_stop_does_not_burst() {
        // As after Ctrl-Z: the process was stopped for several notice periods.
        let now = Instant::now();
        let mut hold = Hold {
            reason: HoldReason::MemoryNearlyFull,
            since: now.checked_sub(Duration::from_secs(150)).unwrap(),
            next_notice: now.checked_sub(Duration::from_secs(120)).unwrap(),
        };
        let mut first = Vec::new();
        hold.notice_if_due(&mut first, &SessionId("s1".to_owned()));
        assert!(!first.is_empty(), "the first notice is due");
        let mut again = Vec::new();
        hold.notice_if_due(&mut again, &SessionId("s1".to_owned()));
        assert!(again.is_empty(), "one line, not one per missed period");
    }

    #[test]
    fn session_env_never_sets_zero_threads() {
        let inv = Invocation {
            kind: Kind::Test,
            subcommand: Some("test".to_owned()),
            user_threads: false,
        };
        let env = session_env(Path::new("/f"), &SessionId("s".to_owned()), &inv, 0);
        assert!(env.contains(&("RUST_TEST_THREADS".into(), "1".into())));
    }
}
