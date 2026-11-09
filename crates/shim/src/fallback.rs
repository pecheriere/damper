//! Failing open: feeding a session's FIFO when the daemon is gone, and
//! recording fallbacks for the daemon to pick up (spec §7.1).

use damper_proto::paths::Paths;
use damper_proto::{Event, EventKind};
use nix::fcntl::OFlag;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A jobserver token.
const TOKEN: u8 = b'+';
/// The shim's fallback log, under `Paths::log`.
const FALLBACKS: &str = "fallbacks.jsonl";

/// Writes `tokens` jobserver tokens into the session FIFO.
///
/// The FIFO is opened write-only and non-blocking: with no reader the open
/// fails (`ENXIO`) rather than hanging, and a full pipe fails rather than
/// blocking. Callers ignore the error: cargo keeps its implicit slot.
///
/// # Errors
/// The open or write error, or `InvalidInput` if `fifo` is not a FIFO.
pub fn self_feed(fifo: &Path, tokens: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .custom_flags((OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW).bits())
        .open(fifo)?;
    if !file.metadata()?.file_type().is_fifo() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a FIFO"));
    }
    let count = usize::try_from(tokens).map_err(io::Error::other)?;
    file.write_all(&vec![TOKEN; count])
}

/// Appends one `ShimFallback` event to `<log>/fallbacks.jsonl`. Every error
/// is ignored: recording a fallback must never be what fails a build.
pub fn record_fallback(paths: &Paths, reason: &str, cwd: &Path) {
    let _ = append_fallback(paths, reason, cwd);
}

fn append_fallback(paths: &Paths, reason: &str, cwd: &Path) -> io::Result<()> {
    let event = Event {
        ts_ms: now_ms(),
        kind: EventKind::ShimFallback {
            reason: reason.to_owned(),
            cwd: cwd.to_path_buf(),
        },
    };
    let mut line = serde_json::to_vec(&event)?;
    line.push(b'\n');
    // One write, so lines from concurrent shims do not interleave.
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(OFlag::O_NOFOLLOW.bits())
        .open(paths.log.join(FALLBACKS))?
        .write_all(&line)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test")]
mod tests {
    use super::{record_fallback, self_feed};
    use damper_proto::paths::resolve_paths_from;
    use damper_proto::{Event, EventKind};
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    #[test]
    fn self_feed_writes_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("s.fifo");
        mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();
        // The reader cargo would be: read-write so the open does not block.
        let mut reader = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        self_feed(&fifo, 3).unwrap();
        let mut buf = [0u8; 3];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"+++");
    }

    #[test]
    fn self_feed_without_a_reader_fails_instead_of_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("s.fifo");
        mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();
        assert!(self_feed(&fifo, 3).is_err());
    }

    #[test]
    fn self_feed_refuses_a_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain");
        std::fs::write(&file, "").unwrap();
        assert!(self_feed(&file, 3).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"");
    }

    #[test]
    fn record_fallback_appends_private_event_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = resolve_paths_from(Some(tmp.path())).unwrap();
        std::fs::create_dir_all(&paths.log).unwrap();
        record_fallback(&paths, "no reply", Path::new("/w"));
        record_fallback(&paths, "again", Path::new("/w"));
        let file = paths.log.join("fallbacks.jsonl");
        let text = std::fs::read_to_string(&file).unwrap();
        let events: Vec<Event> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert!(events[0].ts_ms > 0);
        assert_eq!(
            events[0].kind,
            EventKind::ShimFallback {
                reason: "no reply".to_owned(),
                cwd: "/w".into()
            }
        );
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn record_fallback_ignores_a_missing_log_dir_and_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = resolve_paths_from(Some(tmp.path())).unwrap();
        record_fallback(&paths, "no log dir", Path::new("/w"));
        std::fs::create_dir_all(&paths.log).unwrap();
        let target = tmp.path().join("elsewhere");
        std::os::unix::fs::symlink(&target, paths.log.join("fallbacks.jsonl")).unwrap();
        record_fallback(&paths, "symlinked", Path::new("/w"));
        assert!(!target.exists());
    }
}
