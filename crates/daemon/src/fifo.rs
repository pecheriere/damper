//! Session FIFOs: the jobserver token pipe of one build session.
//!
//! Each FIFO is opened twice. The read-write descriptor feeds tokens; the
//! read-only one counts and drains them, because `FIONREAD` on a macOS FIFO
//! reads 0 on a read-write descriptor (Phase 0, S3).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use damper_proto::SessionId;
use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::kill;
use nix::sys::stat::Mode;
use nix::unistd::{Pid, mkfifo};

/// The byte written as one token.
const TOKEN: u8 = b'+';
const CHUNK: usize = 4096;

/// A session's token FIFO. Dropping it closes the descriptors but leaves the
/// file; use [`SessionFifo::remove`] to unlink it.
#[derive(Debug)]
pub struct SessionFifo {
    path: PathBuf,
    /// `O_RDWR | O_NONBLOCK`: keeps the FIFO open and feeds tokens.
    rw: File,
    /// `O_RDONLY | O_NONBLOCK`: `FIONREAD` accounting and draining.
    ro: File,
}

fn open(path: &Path, read: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(!read)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl SessionFifo {
    /// Create `<run_dir>/s-<shim_pid>-<session>.fifo` with mode 0600 and open it.
    ///
    /// # Errors
    /// `InvalidInput` for a session id that is not `[A-Za-z0-9_-]+`;
    /// `AlreadyExists` if the path exists (nothing is followed or replaced);
    /// any other error from `mkfifo` or `open`.
    pub fn create(run_dir: &Path, shim_pid: u32, session: &SessionId) -> io::Result<Self> {
        if !valid_session_id(&session.0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session id must match [A-Za-z0-9_-]+",
            ));
        }
        let path = run_dir.join(format!("s-{shim_pid}-{}.fifo", session.0));
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).map_err(io::Error::from)?;
        match (open(&path, true), open(&path, false)) {
            (Ok(ro), Ok(rw)) => Ok(Self { path, rw, ro }),
            (Err(e), _) | (_, Err(e)) => {
                // Best effort: the open error is the one worth reporting.
                let _ = fs::remove_file(&path);
                Err(e)
            }
        }
    }

    /// The FIFO's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Tokens currently in the pipe, without consuming any.
    ///
    /// # Errors
    /// The `ioctl` error.
    pub fn fill(&self) -> io::Result<u32> {
        let n = rustix::io::ioctl_fionread(&self.ro)?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Write up to `n` tokens; a full pipe stops the write early.
    ///
    /// Returns the number actually written.
    ///
    /// # Errors
    /// Any write error other than `WouldBlock` / `Interrupted`.
    pub fn feed(&self, n: u32) -> io::Result<u32> {
        let buf = [TOKEN; CHUNK];
        let mut written: u32 = 0;
        let mut w = &self.rw;
        while written < n {
            let want = usize::try_from(n - written).map_or(CHUNK, |r| r.min(CHUNK));
            match w.write(&buf[..want]) {
                Ok(0) => break,
                Ok(k) => written += u32::try_from(k).unwrap_or(0),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(written)
    }

    /// Read and discard up to `n` tokens. An empty pipe yields 0.
    ///
    /// Returns the number actually read.
    ///
    /// # Errors
    /// Any read error other than `WouldBlock` / `Interrupted`.
    pub fn drain(&self, n: u32) -> io::Result<u32> {
        let mut buf = [0_u8; CHUNK];
        let mut read: u32 = 0;
        let mut r = &self.ro;
        while read < n {
            let want = usize::try_from(n - read).map_or(CHUNK, |rest| rest.min(CHUNK));
            match r.read(&mut buf[..want]) {
                Ok(0) => break,
                Ok(k) => read += u32::try_from(k).unwrap_or(0),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(read)
    }

    /// Unlink the FIFO, then close the descriptors.
    ///
    /// # Errors
    /// The unlink error (the descriptors are closed regardless).
    pub fn remove(self) -> io::Result<()> {
        fs::remove_file(&self.path)
    }
}

/// The shim pid encoded in a `s-<pid>-<id>.fifo` file name.
///
/// `None` for any other name, a pid of 0 or one above `i32::MAX`.
#[must_use]
pub fn shim_pid_of(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("s-")?.strip_suffix(".fifo")?;
    let (pid, id) = rest.split_once('-')?;
    if id.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pid: u32 = pid.parse().ok()?;
    (pid != 0 && i32::try_from(pid).is_ok()).then_some(pid)
}

fn pid_is_dead(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    // EPERM means the pid exists but belongs to another user: alive.
    matches!(kill(Pid::from_raw(raw), None), Err(Errno::ESRCH))
}

/// Remove the FIFOs in `run_dir` left behind by dead shims.
///
/// Only FIFOs named `s-<pid>-<id>.fifo` whose pid no longer exists are
/// touched. Returns the removed paths.
///
/// # Errors
/// Failure to read the directory. Entries that vanish or cannot be removed
/// are skipped.
pub fn sweep_stale_fifos(run_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for entry in fs::read_dir(run_dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Some(pid) = shim_pid_of(&path) else {
            continue;
        };
        let is_fifo = fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_fifo());
        if is_fifo && pid_is_dead(pid) && fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests;
