//! `damper doctor`: checks that the install is wired up correctly.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use damper_core::config::load;
use damper_proto::paths::Paths;
use damper_proto::{Client, ClientMsg, MAX_REPLY, PROTOCOL_VERSION, ProtoError, ServerMsg};
use nix::unistd::Uid;

use crate::machine;

const TIMEOUT: Duration = Duration::from_secs(5);

/// One line of `doctor` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

fn check(name: &'static str, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        name,
        ok,
        detail: detail.into(),
    }
}

/// Runs every check. `path_var` is the `PATH` to search for `cargo`.
#[must_use]
pub fn doctor(paths: &Paths, path_var: &OsStr) -> Vec<Check> {
    doctor_with(paths, path_var, TIMEOUT)
}

/// [`doctor`] with the time to wait for the daemon's answer.
fn doctor_with(paths: &Paths, path_var: &OsStr, timeout: Duration) -> Vec<Check> {
    vec![
        cargo_on_path(path_var),
        daemon(paths, timeout),
        private_entry("run directory", &paths.run, 0o700, true),
        private_entry("socket", &paths.socket, 0o600, false),
        config(paths),
        disabled_flag(paths),
    ]
}

/// The first executable `cargo` on `path_var`, as the shim walks it.
fn first_cargo(path_var: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|entry| !entry.as_os_str().is_empty())
        .map(|entry| entry.join("cargo"))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

fn cargo_on_path(path_var: &OsStr) -> Check {
    const NAME: &str = "cargo on PATH";
    let Some(found) = first_cargo(path_var) else {
        return check(NAME, false, "no cargo found on PATH");
    };
    let target = found.canonicalize().unwrap_or_else(|_| found.clone());
    if target.file_name() == Some(OsStr::new("damper-shim")) {
        check(
            NAME,
            true,
            format!("{} is the damper shim", found.display()),
        )
    } else {
        check(
            NAME,
            false,
            format!(
                "the first cargo is {} (resolves to {}), not damper-shim; put the shim's directory first",
                found.display(),
                target.display(),
            ),
        )
    }
}

fn daemon(paths: &Paths, timeout: Duration) -> Check {
    const NAME: &str = "daemon";
    let answer = Client::connect(&paths.socket, timeout).and_then(|mut client| {
        client.send(&ClientMsg::Status)?;
        client.recv_with_limit(Some(timeout), MAX_REPLY)
    });
    match answer {
        Ok(ServerMsg::Status(_)) => check(
            NAME,
            true,
            format!("answers on protocol version {PROTOCOL_VERSION}"),
        ),
        Ok(_) => check(NAME, false, "unexpected reply to Status"),
        Err(ProtoError::Version { got }) => check(
            NAME,
            false,
            format!("speaks protocol version {got}, this damper speaks {PROTOCOL_VERSION}"),
        ),
        Err(e) => check(
            NAME,
            false,
            format!("no answer on {} ({e})", paths.socket.display()),
        ),
    }
}

/// Checks that `path` is not a symlink, is owned by the user and has exactly
/// `mode`; `dir` says whether it must be a directory (else a socket).
fn private_entry(name: &'static str, path: &Path, mode: u32, dir: bool) -> Check {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) => return check(name, false, format!("{}: {e}", path.display())),
    };
    let kind = meta.file_type();
    let right_kind = if dir { kind.is_dir() } else { kind.is_socket() };
    let actual = meta.mode() & 0o777;
    let mut problems = Vec::new();
    if !right_kind {
        problems.push(format!(
            "is not a real {}",
            if dir { "directory" } else { "socket" }
        ));
    }
    if meta.uid() != Uid::current().as_raw() {
        problems.push(format!("is owned by uid {}", meta.uid()));
    }
    if actual != mode {
        problems.push(format!("has mode {actual:04o}, expected {mode:04o}"));
    }
    if problems.is_empty() {
        check(name, true, format!("{} ({actual:04o})", path.display()))
    } else {
        check(
            name,
            false,
            format!("{} {}", path.display(), problems.join(" and ")),
        )
    }
}

fn config(paths: &Paths) -> Check {
    const NAME: &str = "config";
    let text = match fs::read_to_string(&paths.config) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return check(NAME, false, format!("{}: {e}", paths.config.display())),
    };
    let machine = match machine::read() {
        Ok(machine) => machine,
        Err(e) => return check(NAME, false, e.to_string()),
    };
    let loaded = load(text.as_deref(), machine);
    if loaded.warnings.is_empty() {
        let detail = if text.is_some() {
            format!("{} is valid", paths.config.display())
        } else {
            "no config file, using defaults".to_owned()
        };
        check(NAME, true, detail)
    } else {
        check(NAME, false, loaded.warnings.join("; "))
    }
}

/// Informational: a set flag is a choice, not a fault, so it never fails.
fn disabled_flag(paths: &Paths) -> Check {
    let detail = if paths.disabled.exists() {
        format!(
            "disabled: flag set ({}); damper is off, run 'damper enable'",
            paths.disabled.display()
        )
    } else {
        "not disabled (flag not set)".to_owned()
    };
    check("disabled flag (damper off when set)", true, detail)
}

#[cfg(test)]
mod tests;
