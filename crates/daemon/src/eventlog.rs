//! The event log: `events.jsonl`, rotated by size (spec §7.3).
//!
//! One [`Event`] per line. When the next line would take the file past the
//! size limit, `events.jsonl` becomes `events.jsonl.1`, `.1` becomes `.2` and
//! so on; `.4` is the oldest kept. Files are created with mode 0600 and never
//! through a symlink.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use damper_proto::{Event, EventKind, MAX_LINE};
use nix::libc;

/// The size at which the current file is rotated: 10 MiB.
pub const ROTATE_BYTES: u64 = 10 * 1024 * 1024;
/// Rotated files kept besides the current one.
pub const KEEP_ROTATED: u32 = 4;
const FILE_NAME: &str = "events.jsonl";

/// The append-only, size-rotated event log.
#[derive(Debug)]
pub struct EventLog {
    dir: PathBuf,
    file: File,
    size: u64,
    limit: u64,
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

fn open_read(path: &Path) -> io::Result<Option<File>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Milliseconds since the Unix epoch, now.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Every complete line of `file` that parses as an [`Event`]; others are
/// skipped.
fn parse_lines(file: File) -> io::Result<Vec<Event>> {
    let mut events = Vec::new();
    for line in BufReader::new(file).split(b'\n') {
        if let Ok(event) = serde_json::from_slice::<Event>(&line?) {
            events.push(event);
        }
    }
    Ok(events)
}

impl EventLog {
    /// Opens (creating if needed) `<dir>/events.jsonl`, rotating at 10 MiB.
    ///
    /// # Errors
    /// Failure to open the file or read its size.
    pub fn open(dir: &Path) -> io::Result<Self> {
        Self::open_with_limit(dir, ROTATE_BYTES)
    }

    /// Like [`EventLog::open`], rotating at `limit` bytes.
    ///
    /// # Errors
    /// Failure to open the file or read its size.
    pub fn open_with_limit(dir: &Path, limit: u64) -> io::Result<Self> {
        let path = dir.join(FILE_NAME);
        let mut file = open_append(&path)?;
        let mut size = file.metadata()?.len();
        // A crash mid-write can leave a last line without its newline; end
        // it, so the next event starts a line of its own.
        if let Some(last) = size.checked_sub(1)
            && let Some(reader) = open_read(&path)?
        {
            let mut byte = [0_u8; 1];
            reader.read_exact_at(&mut byte, last)?;
            if byte != *b"\n" {
                file.write_all(b"\n")?;
                size += 1;
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            file,
            size,
            limit,
        })
    }

    fn rotated(&self, k: u32) -> PathBuf {
        self.dir.join(format!("{FILE_NAME}.{k}"))
    }

    /// Appends `kind`, stamped with the current time.
    ///
    /// # Errors
    /// Failure to rotate or write.
    pub fn append(&mut self, kind: EventKind) -> io::Result<()> {
        self.write_event(&Event {
            ts_ms: now_ms(),
            kind,
        })
    }

    fn write_event(&mut self, event: &Event) -> io::Result<()> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let len = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if self.size > 0 && self.size.saturating_add(len) > self.limit {
            self.rotate()?;
        }
        self.file.write_all(&line)?;
        self.size = self.size.saturating_add(len);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        for k in (1..KEEP_ROTATED).rev() {
            match fs::rename(self.rotated(k), self.rotated(k + 1)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        let current = self.dir.join(FILE_NAME);
        fs::rename(&current, self.rotated(1))?;
        self.file = open_append(&current)?;
        self.size = 0;
        Ok(())
    }

    /// The logged events at or after `since_ms`, oldest first across the
    /// rotated files. Lines that do not parse are skipped.
    ///
    /// # Errors
    /// Failure to open or read a file that exists.
    pub fn read(&self, since_ms: Option<u64>) -> io::Result<Vec<Event>> {
        let mut paths: Vec<PathBuf> = (1..=KEEP_ROTATED).rev().map(|k| self.rotated(k)).collect();
        paths.push(self.dir.join(FILE_NAME));
        let mut events = Vec::new();
        for path in paths {
            if let Some(file) = open_read(&path)? {
                events.extend(
                    parse_lines(file)?
                        .into_iter()
                        .filter(|e| since_ms.is_none_or(|since| e.ts_ms >= since)),
                );
            }
        }
        Ok(events)
    }

    /// Moves the shim's fallback records from `path` into the log and returns
    /// how many there were, reading about `budget` bytes at most.
    ///
    /// Each line of `path` is one [`Event`] whose kind is
    /// [`EventKind::ShimFallback`], stamped by the shim; its time is kept.
    /// Other lines are skipped, so the shim's file cannot forge other events.
    /// The file is first renamed to `<path>.ingest` so that a shim appending
    /// meanwhile starts a new file instead of losing its line; a `.ingest`
    /// file left by an earlier run is taken first. Lines longer than
    /// [`MAX_LINE`] are skipped. A file that can't be opened or read is
    /// logged as `Error` and renamed to `<path>.bad`, and ingesting carries
    /// on, so a bad leftover cannot fail every start.
    ///
    /// Reading stops at the first line end at or past `budget` bytes, so a
    /// large file cannot stall the caller. The rest stays in `.ingest`, and
    /// how far it was read is kept in `<path>.ingest.pos`, so the next run
    /// (in this daemon or the next) carries on where this one stopped. A run
    /// interrupted between appending and recording its position takes those
    /// lines again: a record may be logged twice, never lost.
    ///
    /// # Errors
    /// Failure to rename, remove or write the files, or to append.
    pub fn ingest_fallbacks(&mut self, path: &Path, budget: u64) -> io::Result<u32> {
        let with_suffix = |suffix: &str| {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            PathBuf::from(name)
        };
        let files = IngestFiles {
            taking: with_suffix(".ingest"),
            pos: with_suffix(".ingest.pos"),
            bad: with_suffix(".bad"),
        };
        let leftover = self.ingest_file(&files, budget)?;
        if !leftover.finished || leftover.read >= budget {
            return Ok(leftover.count);
        }
        // No `.ingest` is left, so any position is stale.
        remove_if_exists(&files.pos)?;
        match fs::rename(path, &files.taking) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(leftover.count),
            other => other?,
        }
        let rest = self.ingest_file(&files, budget - leftover.read)?;
        Ok(leftover.count.saturating_add(rest.count))
    }

    /// Ingests `files.taking` from its recorded position, if it exists, up to
    /// `budget` bytes. Removes it once read to the end, or records how far it
    /// got. A file that can't be read is logged and renamed to `files.bad`,
    /// so it cannot stop every later start.
    fn ingest_file(&mut self, files: &IngestFiles, budget: u64) -> io::Result<Taken> {
        let mut file = match open_read(&files.taking) {
            Ok(None) => return Ok(Taken::NOTHING),
            Ok(Some(file)) => file,
            Err(e) => return self.set_aside(files, &e),
        };
        let start = read_pos(&files.pos);
        if let Err(e) = file.seek(SeekFrom::Start(start)) {
            return self.set_aside(files, &e);
        }
        let (count, progress) = match self.take_fallbacks(file, budget) {
            Ok(taken) => taken,
            Err(IngestError::Read(e)) => return self.set_aside(files, &e),
            Err(IngestError::Write(e)) => return Err(e),
        };
        if progress.eof {
            // The position goes first: losing it re-reads, never skips.
            remove_if_exists(&files.pos)?;
            fs::remove_file(&files.taking)?;
        } else {
            write_pos(&files.pos, start.saturating_add(progress.read))?;
        }
        Ok(Taken {
            count,
            read: progress.read,
            finished: progress.eof,
        })
    }

    fn set_aside(&mut self, files: &IngestFiles, cause: &io::Error) -> io::Result<Taken> {
        remove_if_exists(&files.pos)?;
        let (path, bad) = (&files.taking, &files.bad);
        let message = match fs::rename(path, bad) {
            Ok(()) => format!(
                "reading {}: {cause}; moved it to {}",
                path.display(),
                bad.display()
            ),
            Err(e) => format!(
                "reading {}: {cause}; moving it aside failed too: {e}",
                path.display()
            ),
        };
        self.append(EventKind::Error { message })?;
        Ok(Taken::NOTHING)
    }

    fn take_fallbacks(&mut self, file: File, budget: u64) -> Result<(u32, Progress), IngestError> {
        let mut count: u32 = 0;
        let progress = for_each_line(BufReader::new(file), MAX_LINE, budget, |line| {
            if let Ok(event) = serde_json::from_slice::<Event>(line)
                && matches!(event.kind, EventKind::ShimFallback { .. })
            {
                self.write_event(&event)?;
                count = count.saturating_add(1);
            }
            Ok(())
        })?;
        Ok((count, progress))
    }
}

/// The files of one fallback ingest.
struct IngestFiles {
    /// The fallback file being read, renamed from the shims'.
    taking: PathBuf,
    /// How far `taking` has been read, in bytes, as decimal text.
    pos: PathBuf,
    /// Where an unreadable `taking` is moved.
    bad: PathBuf,
}

/// What one pass over a fallback file took.
struct Taken {
    /// Fallback records appended to the log.
    count: u32,
    /// Bytes read.
    read: u64,
    /// The file was read to its end and removed, or was not there.
    finished: bool,
}

impl Taken {
    const NOTHING: Self = Self {
        count: 0,
        read: 0,
        finished: true,
    };
}

/// The recorded read position; 0 when there is none. An unreadable or
/// garbled position also reads as 0: records may then be logged twice, but
/// none is lost.
fn read_pos(path: &Path) -> u64 {
    let mut text = String::new();
    match open_read(path) {
        Ok(Some(file)) => match file.take(32).read_to_string(&mut text) {
            Ok(_) => text.trim().parse().unwrap_or(0),
            Err(_) => 0,
        },
        Ok(None) | Err(_) => 0,
    }
}

/// Records the read position atomically: written aside, then renamed over.
fn write_pos(path: &Path, pos: u64) -> io::Result<()> {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    let tmp = PathBuf::from(name);
    remove_if_exists(&tmp)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    file.write_all(pos.to_string().as_bytes())?;
    fs::rename(&tmp, path)
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Why ingesting a fallback file stopped.
#[derive(Debug)]
enum IngestError {
    /// The shim's file could not be read.
    Read(io::Error),
    /// The event log could not be written.
    Write(io::Error),
}

/// How far [`for_each_line`] read.
struct Progress {
    /// Bytes consumed.
    read: u64,
    /// The reader was read to its end.
    eof: bool,
}

/// Calls `f` with every line of `reader` that, with its newline, is at most
/// `max` bytes (passed without the newline). Longer lines are skipped
/// without ever buffering more than `max + 1` bytes of them. Stops at the
/// first line end at or past `budget` bytes, so at least one line is read.
fn for_each_line(
    mut reader: impl BufRead,
    max: usize,
    budget: u64,
    mut f: impl FnMut(&[u8]) -> io::Result<()>,
) -> Result<Progress, IngestError> {
    let mut line = Vec::new();
    let mut skipping = false;
    let mut read: u64 = 0;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(IngestError::Read(e)),
        };
        if available.is_empty() {
            // A last line without a newline is still offered.
            if !skipping && !line.is_empty() {
                f(&line).map_err(IngestError::Write)?;
            }
            return Ok(Progress { read, eof: true });
        }
        let (take, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if !skipping {
            let room = max.saturating_add(1).saturating_sub(line.len());
            line.extend_from_slice(&available[..take.min(room)]);
            if line.len() > max {
                skipping = true;
                line.clear();
            }
        }
        reader.consume(take);
        read = read.saturating_add(u64::try_from(take).unwrap_or(u64::MAX));
        if done {
            if !skipping {
                line.pop();
                f(&line).map_err(IngestError::Write)?;
            }
            line.clear();
            skipping = false;
            if read >= budget {
                return Ok(Progress { read, eof: false });
            }
        }
    }
}

#[cfg(test)]
mod tests;
