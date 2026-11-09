//! The commands of `damper`. Output goes to the writers passed in, so the
//! commands never print through the standard macros.

use std::fmt::Display;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use damper_core::config::{Priority, load};
use damper_proto::paths::{PathError, Paths, prepare_dirs, resolve_paths};
use damper_proto::{Client, ClientMsg, Event, MAX_REPLY, ProtoError, ServerMsg, SessionId, Status};
use nix::fcntl::OFlag;

use crate::render::{clean, fmt_bytes, fmt_secs, render_events, render_explain, render_status};
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
            let _ = writeln!(err, "damper: {}", clean(&message));
            1
        }
        Err(Failure::NotRunning(socket)) => {
            let _ = writeln!(
                err,
                "damper: damperd is not running (socket {}). Start it with 'brew services start damper'.",
                clean(&socket.to_string_lossy())
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
            let since_ms = since.map(|d| since_cutoff(now_ms(), d));
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
            prepare_dirs(&paths).map_err(|e| match e {
                PathError::BadMode { path, .. } => Failure::Message(format!(
                    "{} is accessible by group or others; run: chmod 700 {}",
                    path.display(),
                    path.display()
                )),
                other => other.into(),
            })?;
            set_disabled_flag(&paths.disabled)?;
            writeln!(
                out,
                "damper is disabled; run 'damper enable' to turn it back on"
            )?;
            Ok(())
        }
    }
}

/// The absolute time (epoch ms) `since` before `now_ms`; zero when `since`
/// reaches back past the epoch.
fn since_cutoff(now_ms: u64, since: Duration) -> u64 {
    let since_ms = u64::try_from(since.as_millis()).unwrap_or(u64::MAX);
    now_ms.saturating_sub(since_ms)
}

/// Creates the disabled flag without following a symlink or truncating
/// anything. An existing regular file already means "disabled"; anything else
/// at that path is refused.
fn set_disabled_flag(flag: &std::path::Path) -> Result<(), Failure> {
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(OFlag::O_NOFOLLOW.bits())
        .open(flag);
    match created {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let kind = fs::symlink_metadata(flag)?.file_type();
            if kind.is_file() {
                Ok(())
            } else {
                let what = if kind.is_symlink() {
                    "a symlink"
                } else if kind.is_dir() {
                    "a directory"
                } else if kind.is_socket() {
                    "a socket"
                } else {
                    "not a regular file"
                };
                Err(Failure::Message(format!(
                    "{} is {what}; remove it and run 'damper disable' again",
                    flag.display()
                )))
            }
        }
        Err(e) => Err(e.into()),
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
            clean(&paths.config.to_string_lossy())
        } else {
            format!(
                "none at {} (defaults)",
                clean(&paths.config.to_string_lossy())
            )
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
            clean(&e.extra_build_subcommands.join(", "))
        }
    )?;
    if loaded.warnings.is_empty() {
        return Ok(());
    }
    writeln!(out, "warnings:")?;
    for warning in &loaded.warnings {
        writeln!(out, "  - {}", clean(warning))?;
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
            clean(&c.detail)
        )?;
    }
    if checks.iter().all(|c| c.ok) {
        Ok(())
    } else {
        Err(Failure::Reported)
    }
}

#[cfg(test)]
mod tests;
