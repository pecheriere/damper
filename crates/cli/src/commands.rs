//! The commands of `damper`. Output goes to the writers passed in, so the
//! commands never print through the standard macros.

use std::fmt::Display;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use damper_core::config::{Priority, load};
use damper_proto::paths::{Paths, prepare_dirs, resolve_paths};
use damper_proto::{Client, ClientMsg, Event, MAX_REPLY, ProtoError, ServerMsg, SessionId, Status};

use crate::render::{fmt_bytes, fmt_secs, render_events, render_explain, render_status};
use crate::{Command, ConfigAction, doctor, machine};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Why a command did not succeed.
enum Failure {
    /// The daemon did not answer: exit 2.
    NotRunning(PathBuf),
    /// Anything else: exit 1.
    Message(String),
    /// The command ran and reported a problem itself (output already written).
    Reported,
}

impl<E: Display> From<E> for Failure {
    fn from(e: E) -> Self {
        Self::Message(e.to_string())
    }
}

/// Runs `command`, returning the process exit code.
pub fn run(command: &Command, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    match dispatch(command, out) {
        Ok(()) => 0,
        Err(Failure::Reported) => 1,
        Err(Failure::Message(message)) => {
            let _ = writeln!(err, "damper: {message}");
            1
        }
        Err(Failure::NotRunning(socket)) => {
            let _ = writeln!(
                err,
                "damper: damperd is not running (socket {}). Start it with 'brew services start damper'.",
                socket.display()
            );
            2
        }
    }
}

fn dispatch(command: &Command, out: &mut dyn Write) -> Result<(), Failure> {
    let paths = resolve_paths()?;
    match command {
        Command::Status { json } => status(&paths, *json, out),
        Command::Log {
            json,
            since,
            session,
        } => {
            let since_ms = since
                .map(|d| u64::try_from(d.as_millis()))
                .transpose()
                .map_err(|_| Failure::Message("--since is too large".to_owned()))?;
            let session = session.clone().map(SessionId);
            events(
                &paths,
                &ClientMsg::Log { since_ms, session },
                *json,
                None,
                out,
            )
        }
        Command::Explain { session, json } => {
            let id = SessionId(session.clone());
            events(
                &paths,
                &ClientMsg::Explain {
                    session: id.clone(),
                },
                *json,
                Some(&id),
                out,
            )
        }
        Command::Config {
            action: ConfigAction::Check,
        } => config_check(&paths, out),
        Command::Doctor => run_doctor(&paths, out),
        Command::Enable => {
            match fs::remove_file(&paths.disabled) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            writeln!(out, "damper is enabled")?;
            Ok(())
        }
        Command::Disable => {
            prepare_dirs(&paths)?;
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&paths.disabled)?;
            writeln!(
                out,
                "damper is disabled; run 'damper enable' to turn it back on"
            )?;
            Ok(())
        }
    }
}

/// Sends `msg` and returns the reply. A daemon that cannot be reached, or that
/// does not answer in time, counts as not running; an `Error` reply is a failure.
fn ask(paths: &Paths, msg: &ClientMsg) -> Result<ServerMsg, Failure> {
    let not_running = || Failure::NotRunning(paths.socket.clone());
    let mut client = Client::connect(&paths.socket, TIMEOUT).map_err(|_| not_running())?;
    client.send(msg).map_err(|_| not_running())?;
    let reply = client
        .recv_with_limit(Some(TIMEOUT), MAX_REPLY)
        .map_err(|e| match e {
            ProtoError::Timeout => not_running(),
            other => Failure::Message(format!("bad reply from damperd: {other}")),
        })?;
    match reply {
        ServerMsg::Error { message } => Err(Failure::Message(message)),
        other => Ok(other),
    }
}

fn unexpected() -> Failure {
    Failure::Message("unexpected reply from damperd".to_owned())
}

fn status(paths: &Paths, json: bool, out: &mut dyn Write) -> Result<(), Failure> {
    let ServerMsg::Status(status) = ask(paths, &ClientMsg::Status)? else {
        return Err(unexpected());
    };
    write_status(&status, json, out)
}

fn write_status(status: &Status, json: bool, out: &mut dyn Write) -> Result<(), Failure> {
    if json {
        writeln!(out, "{}", serde_json::to_string(status)?)?;
    } else {
        write!(out, "{}", render_status(status))?;
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// `log` and, with `explain` set, `explain`.
fn events(
    paths: &Paths,
    msg: &ClientMsg,
    json: bool,
    explain: Option<&SessionId>,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let ServerMsg::Events { events, truncated } = ask(paths, msg)? else {
        return Err(unexpected());
    };
    write_events(&events, truncated, json, explain, out)
}

fn write_events(
    events: &[Event],
    truncated: bool,
    json: bool,
    explain: Option<&SessionId>,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    if json {
        let doc = serde_json::json!({ "events": events, "truncated": truncated });
        writeln!(out, "{doc}")?;
        return Ok(());
    }
    let now = now_ms();
    let text = match explain {
        Some(id) => render_explain(id, events, now),
        None => render_events(events, now, truncated),
    };
    write!(out, "{text}")?;
    Ok(())
}

fn config_check(paths: &Paths, out: &mut dyn Write) -> Result<(), Failure> {
    let text = match fs::read_to_string(&paths.config) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(Failure::Message(format!("{}: {e}", paths.config.display()))),
    };
    let loaded = load(text.as_deref(), machine::read()?);
    let e = &loaded.effective;
    let priority = match e.priority {
        Priority::Utility => "utility",
        Priority::Nice => "nice",
        Priority::None => "none",
    };
    writeln!(
        out,
        "config file: {}",
        if text.is_some() {
            paths.config.display().to_string()
        } else {
            format!("none at {} (defaults)", paths.config.display())
        }
    )?;
    writeln!(out, "cores: {}", e.cores)?;
    writeln!(out, "memory: {}", fmt_bytes(e.memory))?;
    writeln!(out, "max_hold: {}", fmt_secs(e.max_hold.as_secs()))?;
    writeln!(out, "enabled: {}", e.enabled)?;
    writeln!(out, "priority: {priority}")?;
    writeln!(
        out,
        "extra_build_subcommands: {}",
        if e.extra_build_subcommands.is_empty() {
            "none".to_owned()
        } else {
            e.extra_build_subcommands.join(", ")
        }
    )?;
    if loaded.warnings.is_empty() {
        return Ok(());
    }
    writeln!(out, "warnings:")?;
    for warning in &loaded.warnings {
        writeln!(out, "  - {warning}")?;
    }
    Err(Failure::Reported)
}

fn run_doctor(paths: &Paths, out: &mut dyn Write) -> Result<(), Failure> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let checks = doctor::doctor(paths, &path_var);
    for c in &checks {
        writeln!(
            out,
            "{} {}: {}",
            if c.ok { "ok  " } else { "FAIL" },
            c.name,
            c.detail
        )?;
    }
    if checks.iter().all(|c| c.ok) {
        Ok(())
    } else {
        Err(Failure::Reported)
    }
}
