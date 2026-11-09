//! State-directory layout shared by the daemon, the shim and the CLI.
//!
//! The state directory comes from the passwd entry of the current user, never
//! from `$HOME`, unless `DAMPER_STATE_DIR` overrides it (tests, development).

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::unistd::{Uid, User};

/// Longest socket path that fits `sockaddr_un.sun_path` on macOS (104 bytes
/// including the terminating NUL).
pub const MAX_SOCKET_PATH: usize = 103;

/// Environment variable that overrides the state directory.
pub const STATE_DIR_ENV: &str = "DAMPER_STATE_DIR";

/// Why the paths could not be resolved or prepared.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// The current user has no passwd entry with a home directory.
    #[error("cannot determine the home directory of the current user")]
    NoHome,
    /// The state directory is not an absolute path.
    #[error("state directory {0} is not an absolute path")]
    NotAbsolute(PathBuf),
    /// The state directory contains whitespace, which `MAKEFLAGS` cannot carry.
    #[error("state directory {0} contains whitespace, which MAKEFLAGS cannot carry")]
    Whitespace(PathBuf),
    /// The socket path exceeds the `sockaddr_un` limit.
    #[error("socket path {path} is {len} bytes, the limit is {MAX_SOCKET_PATH}")]
    SocketTooLong {
        /// The offending socket path.
        path: PathBuf,
        /// Its length in bytes.
        len: usize,
    },
    /// A path that must be a real directory is something else (or a symlink).
    #[error("{0} exists but is not a directory (symlinks are refused)")]
    NotDirectory(PathBuf),
    /// A directory belongs to another user.
    #[error("{path} is owned by uid {owner}, expected {expected}")]
    WrongOwner {
        /// The directory.
        path: PathBuf,
        /// Its owner.
        owner: u32,
        /// The current uid.
        expected: u32,
    },
    /// A directory is accessible by group or others.
    #[error("{path} has mode {mode:o}, group and other access must be clear")]
    BadMode {
        /// The directory.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// An ancestor of the state directory could be swapped by another user.
    #[error(
        "ancestor {path} (uid {owner}, mode {mode:o}) lets another user replace the state directory"
    )]
    UnsafeAncestor {
        /// The ancestor directory.
        path: PathBuf,
        /// Its owner.
        owner: u32,
        /// Its permission bits, including the sticky bit.
        mode: u32,
    },
    /// An I/O error on a path.
    #[error("{path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}

/// Every well-known path damper uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `<pw_dir>/.local/state/damper` (or the override).
    pub state: PathBuf,
    /// `<state>/run`, mode 0700.
    pub run: PathBuf,
    /// `<state>/run/damperd.sock`.
    pub socket: PathBuf,
    /// `<state>/run/damperd.lock`.
    pub lock: PathBuf,
    /// `<state>/log`.
    pub log: PathBuf,
    /// `<state>/disabled`.
    pub disabled: PathBuf,
    /// `<pw_dir>/.config/damper/config.toml`, or `<state>/config.toml` when the
    /// state directory is overridden (development and tests keep everything
    /// under one directory and never read the user's real config).
    pub config: PathBuf,
}

/// Resolve the paths from `DAMPER_STATE_DIR` or the passwd home directory.
///
/// # Errors
/// See [`resolve_paths_from`].
pub fn resolve_paths() -> Result<Paths, PathError> {
    resolve_paths_from(
        std::env::var_os(STATE_DIR_ENV)
            .map(PathBuf::from)
            .as_deref(),
    )
}

/// Resolve the paths with an optional state-directory override.
///
/// With an override, the config file is `<override>/config.toml`; without
/// one it is `<pw_dir>/.config/damper/config.toml`.
///
/// # Errors
/// [`PathError::NoHome`] without a passwd entry, [`PathError::NotAbsolute`],
/// [`PathError::Whitespace`] or [`PathError::SocketTooLong`] for an unusable
/// state directory.
pub fn resolve_paths_from(state_override: Option<&Path>) -> Result<Paths, PathError> {
    let home = User::from_uid(Uid::current())
        .ok()
        .flatten()
        .map(|user| user.dir)
        .ok_or(PathError::NoHome)?;
    let state = state_override.map_or_else(
        || home.join(".local").join("state").join("damper"),
        Path::to_path_buf,
    );
    let config = if state_override.is_some() {
        state.join("config.toml")
    } else {
        home.join(".config").join("damper").join("config.toml")
    };
    if !state.is_absolute() {
        return Err(PathError::NotAbsolute(state));
    }
    if state.to_string_lossy().chars().any(char::is_whitespace) {
        return Err(PathError::Whitespace(state));
    }
    let run = state.join("run");
    let socket = run.join("damperd.sock");
    let len = socket.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(PathError::SocketTooLong { path: socket, len });
    }
    Ok(Paths {
        lock: run.join("damperd.lock"),
        log: state.join("log"),
        disabled: state.join("disabled"),
        config,
        run,
        socket,
        state,
    })
}

/// Create the state, run and log directories (mode 0700) and verify them.
///
/// Existing directories must be real directories (a symlink is refused, never
/// followed), owned by the current user, with no group or other access. Every
/// ancestor of the state directory must be owned by the current user or root
/// and not writable by group or others unless it has the sticky bit (as
/// `/tmp` does); otherwise another user could rename the state directory away
/// and put their own in its place.
///
/// # Errors
/// Any [`PathError`] for a directory that cannot be created or fails the checks.
pub fn prepare_dirs(p: &Paths) -> Result<(), PathError> {
    if let Some(parent) = p.state.parent() {
        fs::create_dir_all(parent).map_err(|source| io_err(parent, source))?;
        check_ancestors(parent)?;
    }
    for dir in [&p.state, &p.run, &p.log] {
        ensure_private_dir(dir)?;
    }
    Ok(())
}

/// Verify every directory from `/` down to `parent` (symlinks resolved first,
/// so each component is a real directory) cannot be replaced by another user.
fn check_ancestors(parent: &Path) -> Result<(), PathError> {
    let resolved = fs::canonicalize(parent).map_err(|source| io_err(parent, source))?;
    let uid = Uid::current().as_raw();
    for dir in resolved.ancestors() {
        let meta = fs::symlink_metadata(dir).map_err(|source| io_err(dir, source))?;
        let mode = meta.permissions().mode() & 0o7777;
        let owned = meta.uid() == uid || meta.uid() == 0;
        let others_can_write = mode & 0o022 != 0;
        let sticky = mode & 0o1000 != 0;
        if !meta.file_type().is_dir() || !owned || (others_can_write && !sticky) {
            return Err(PathError::UnsafeAncestor {
                path: dir.to_path_buf(),
                owner: meta.uid(),
                mode,
            });
        }
    }
    Ok(())
}

fn io_err(path: &Path, source: io::Error) -> PathError {
    PathError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn ensure_private_dir(dir: &Path) -> Result<(), PathError> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .map_err(|source| io_err(dir, source))?;
            fs::symlink_metadata(dir).map_err(|source| io_err(dir, source))?
        }
        Err(source) => return Err(io_err(dir, source)),
    };
    if !meta.file_type().is_dir() {
        return Err(PathError::NotDirectory(dir.to_path_buf()));
    }
    let expected = Uid::current().as_raw();
    if meta.uid() != expected {
        return Err(PathError::WrongOwner {
            path: dir.to_path_buf(),
            owner: meta.uid(),
            expected,
        });
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(PathError::BadMode {
            path: dir.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
