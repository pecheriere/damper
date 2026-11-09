//! Failing open: feeding a session's FIFO when the daemon is gone, and
//! recording fallbacks for the daemon to pick up (spec §7.1).

use damper_proto::paths::Paths;
use damper_proto::{Event, EventKind};
use nix::fcntl::OFlag;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A jobserver token.
const TOKEN: u8 = b'+';
/// The shim's fallback log, under `Paths::log`.
const FALLBACKS: &str = "fallbacks.jsonl";

/// Opens the session FIFO the daemon named, read-write and non-blocking.
///
/// The shim holds this descriptor until cargo exits and never reads from
/// it: being a reader itself, it can still feed the FIFO if the daemon dies
/// before cargo has opened it, where a write-only open would fail with
/// `ENXIO`.
///
/// # Errors
/// `InvalidInput` if `fifo` is not directly inside `run` or is not a FIFO,
/// or the open error (a symlink is refused).
pub fn open_fifo(fifo: &Path, run: &Path) -> io::Result<File> {
    if fifo.parent() != Some(run) || fifo.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FIFO outside the runtime directory",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags((OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW).bits())
        .open(fifo)?;
    if !file.metadata()?.file_type().is_fifo() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a FIFO"));
    }
    Ok(file)
}

/// Writes `tokens` jobserver tokens into the session FIFO opened by
/// [`open_fifo`]. A full pipe fails rather than blocking; callers ignore
/// the error, and cargo keeps its implicit slot.
///
/// # Errors
/// The write error.
pub fn self_feed(mut fifo: &File, tokens: u32) -> io::Result<()> {
    let count = usize::try_from(tokens).map_err(io::Error::other)?;
    fifo.write_all(&vec![TOKEN; count])
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
    use super::{open_fifo, record_fallback, self_feed};
    use damper_proto::paths::resolve_paths_from;
    use damper_proto::{Event, EventKind};
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// A FIFO in a runtime directory, as the daemon would make it.
    fn fifo_in(run: &Path) -> std::path::PathBuf {
        let fifo = run.join("s.fifo");
        mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();
        fifo
    }

    #[test]
    fn self_feed_writes_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = fifo_in(tmp.path());
        let held = open_fifo(&fifo, tmp.path()).unwrap();
        // The reader cargo would be: read-write so the open does not block.
        let mut reader = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        self_feed(&held, 3).unwrap();
        let mut buf = [0u8; 3];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"+++");
    }

    #[test]
    fn self_feed_works_before_any_reader_opens_the_fifo() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = fifo_in(tmp.path());
        let held = open_fifo(&fifo, tmp.path()).unwrap();
        self_feed(&held, 2).unwrap();
        let mut reader = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"++");
    }

    #[test]
    fn open_fifo_refuses_a_regular_file_or_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain");
        std::fs::write(&file, "").unwrap();
        assert!(open_fifo(&file, tmp.path()).is_err());
        let fifo = fifo_in(tmp.path());
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();
        assert!(open_fifo(&link, tmp.path()).is_err());
    }

    #[test]
    fn open_fifo_refuses_a_path_outside_the_runtime_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run");
        std::fs::create_dir(&run).unwrap();
        let fifo = fifo_in(tmp.path());
        assert!(open_fifo(&fifo, &run).is_err());
        assert!(open_fifo(&run.join("../s.fifo"), &run).is_err());
        assert!(open_fifo(&run.join(".."), &run).is_err());
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
