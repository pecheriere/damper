#![expect(clippy::unwrap_used, reason = "test")]

use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};
use std::process::Command;

use damper_proto::SessionId;
use nix::sys::stat::{Mode, umask};

use super::{SessionFifo, shim_pid_of, sweep_stale_fifos};

fn sid(s: &str) -> SessionId {
    SessionId(s.to_owned())
}

#[test]
fn fifo_fill_counts_fed_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let f = SessionFifo::create(dir.path(), 1234, &sid("abc")).unwrap();
    assert_eq!(f.feed(5).unwrap(), 5);
    assert_eq!(f.drain(2).unwrap(), 2);
    assert_eq!(f.fill().unwrap(), 3);
}

#[test]
fn fifo_fill_reads_zero_without_consuming() {
    let dir = tempfile::tempdir().unwrap();
    let f = SessionFifo::create(dir.path(), 1234, &sid("abc")).unwrap();
    assert_eq!(f.fill().unwrap(), 0);
    assert_eq!(f.feed(4).unwrap(), 4);
    assert_eq!(f.fill().unwrap(), 4);
    assert_eq!(f.fill().unwrap(), 4);
    assert_eq!(f.drain(10).unwrap(), 4);
    assert_eq!(f.drain(10).unwrap(), 0);
}

#[test]
fn fifo_is_named_and_private() {
    let dir = tempfile::tempdir().unwrap();
    let f = SessionFifo::create(dir.path(), 77, &sid("s9")).unwrap();
    assert_eq!(f.path(), dir.path().join("s-77-s9.fifo"));
    let meta = fs::symlink_metadata(f.path()).unwrap();
    assert!(meta.file_type().is_fifo());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

#[test]
fn fifo_feed_stops_when_full() {
    let dir = tempfile::tempdir().unwrap();
    let f = SessionFifo::create(dir.path(), 1, &sid("a")).unwrap();
    let fed = f.feed(1_000_000).unwrap();
    assert!(fed > 0 && fed < 1_000_000, "{fed}");
    assert_eq!(f.fill().unwrap(), fed);
}

#[test]
fn fifo_remove_unlinks() {
    let dir = tempfile::tempdir().unwrap();
    let f = SessionFifo::create(dir.path(), 1, &sid("a")).unwrap();
    let path = f.path().to_path_buf();
    f.remove().unwrap();
    assert!(!path.exists());
}

#[test]
fn fifo_create_fails_when_path_exists() {
    let dir = tempfile::tempdir().unwrap();
    let _f = SessionFifo::create(dir.path(), 1, &sid("a")).unwrap();
    let err = SessionFifo::create(dir.path(), 1, &sid("a")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
}

#[test]
fn fifo_create_refuses_planted_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    fs::write(&target, b"x").unwrap();
    symlink(&target, dir.path().join("s-1-a.fifo")).unwrap();
    assert!(SessionFifo::create(dir.path(), 1, &sid("a")).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"x");
}

#[test]
fn fifo_create_rejects_unsafe_session_ids() {
    let dir = tempfile::tempdir().unwrap();
    for bad in ["", "../x", "a/b", "a.b", "a b"] {
        let err = SessionFifo::create(dir.path(), 1, &sid(bad)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}");
    }
}

#[test]
fn shim_pid_parses_fifo_names() {
    assert_eq!(shim_pid_of("/r/s-123-abc.fifo".as_ref()), Some(123));
    assert_eq!(shim_pid_of("/r/s-123-a-b.fifo".as_ref()), Some(123));
    for bad in [
        "s-x-abc.fifo",
        "s-123-.fifo",
        "s-123.fifo",
        "t-123-abc.fifo",
        "s-123-abc.sock",
        "s-0-abc.fifo",
        "s-99999999999-abc.fifo",
        "s--abc.fifo",
    ] {
        assert_eq!(shim_pid_of(bad.as_ref()), None, "{bad}");
    }
}

#[test]
fn sweep_keeps_live_shim_fifo() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let live = SessionFifo::create(dir.path(), std::process::id(), &sid("live")).unwrap();
    let stale = SessionFifo::create(dir.path(), dead, &sid("dead")).unwrap();
    let stale_path = stale.path().to_path_buf();
    // A regular file with a matching name, and an unrelated file, are left alone.
    let regular = dir.path().join(format!("s-{dead}-reg.fifo"));
    fs::write(&regular, b"").unwrap();
    let other = dir.path().join("damperd.lock");
    fs::write(&other, b"").unwrap();

    let report = sweep_stale_fifos(dir.path()).unwrap();

    assert_eq!(report.removed, vec![stale_path.clone()]);
    assert!(report.failed.is_empty());
    assert!(!stale_path.exists());
    assert!(live.path().exists());
    assert!(regular.exists());
    assert!(other.exists());
}

#[test]
fn fifo_mode_is_0600_whatever_the_umask() {
    let dir = tempfile::tempdir().unwrap();
    let old = umask(Mode::from_bits_truncate(0o277));
    let created = SessionFifo::create(dir.path(), 1234, &sid("masked"));
    umask(old);
    let fifo = created.unwrap();
    let mode = fs::metadata(fifo.path()).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o600);
    assert_eq!(fifo.feed(2).unwrap(), 2);
    assert_eq!(fifo.fill().unwrap(), 2);
}

#[test]
fn sweep_reports_removal_failures() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let stale = SessionFifo::create(dir.path(), dead, &sid("stuck")).unwrap();
    let stale_path = stale.path().to_path_buf();
    // A read-only directory makes the unlink fail.
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();

    let report = sweep_stale_fifos(dir.path()).unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(report.removed, Vec::<std::path::PathBuf>::new());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, stale_path);
    assert!(stale_path.exists());
}
