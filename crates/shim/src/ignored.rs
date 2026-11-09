//! The signals the shim inherited as ignored.
//!
//! A build started with `nohup` (SIGHUP ignored) or in the background of a
//! non-interactive shell (SIGINT and SIGQUIT ignored) must keep ignoring
//! them: installing a handler would replace `SIG_IGN`, and exec resets a
//! caught signal to its default, so cargo would die of a signal it was
//! meant to survive. std and nix offer no safe way to read a disposition,
//! so the shim reads its own `kp_proc.p_sigignore` from `kern.proc.pid`
//! through libproc's typed `kinfo_proc`.

use libproc::proc_pid::kproc_info_raw;
use std::io;

/// A set of signals, bit `n - 1` standing for signal `n` (`sigset_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SignalSet(u32);

impl SignalSet {
    /// Whether `sig` is in the set.
    #[must_use]
    pub fn contains(self, sig: i32) -> bool {
        (1..=32).contains(&sig) && self.0 & (1 << (sig - 1)) != 0
    }
}

/// The signals this process ignores.
///
/// # Errors
/// The sysctl fails or returns another process's record. Callers then
/// can't know what was ignored and must not install handlers.
pub fn ignored_signals() -> io::Result<SignalSet> {
    ignored_by(std::process::id())
}

fn ignored_by(pid: u32) -> io::Result<SignalSet> {
    let pid = i32::try_from(pid).map_err(io::Error::other)?;
    let info = kproc_info_raw(pid).map_err(io::Error::other)?;
    if info.kp_proc.p_pid != pid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kinfo_proc for another process",
        ));
    }
    Ok(SignalSet(info.kp_proc.p_sigignore))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test")]
mod tests {
    use super::{SignalSet, ignored_by};
    use nix::sys::signal::Signal;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    #[test]
    fn reads_the_ignores_a_process_was_started_with() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' HUP QUIT; echo ready; exec /bin/cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, "ready\n");
        let set = ignored_by(child.id());
        drop(child.stdin.take());
        child.wait().unwrap();
        let set = set.unwrap();
        assert!(set.contains(Signal::SIGHUP as i32), "{set:?}");
        assert!(set.contains(Signal::SIGQUIT as i32), "{set:?}");
        assert!(!set.contains(Signal::SIGTERM as i32), "{set:?}");
    }

    #[test]
    fn reads_its_own_record() {
        assert!(ignored_by(std::process::id()).is_ok());
    }

    #[test]
    fn contains_only_real_signal_numbers() {
        let all = SignalSet(u32::MAX);
        assert!(all.contains(1) && all.contains(32));
        assert!(!all.contains(0) && !all.contains(33) && !all.contains(-1));
    }
}
