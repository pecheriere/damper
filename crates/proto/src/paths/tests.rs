#![expect(clippy::unwrap_used, reason = "test")]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use super::{PathError, prepare_dirs, resolve_paths_from};

#[test]
fn paths_refuse_whitespace() {
    let err = resolve_paths_from(Some(Path::new("/tmp/with space/damper"))).unwrap_err();
    assert!(matches!(err, PathError::Whitespace(_)), "{err}");
}

#[test]
fn paths_refuse_long_socket() {
    let long = format!("/tmp/{}", "a".repeat(115));
    assert_eq!(long.len(), 120);
    let err = resolve_paths_from(Some(Path::new(&long))).unwrap_err();
    assert!(matches!(err, PathError::SocketTooLong { .. }), "{err}");
}

#[test]
fn paths_refuse_relative() {
    let err = resolve_paths_from(Some(Path::new("relative/damper"))).unwrap_err();
    assert!(matches!(err, PathError::NotAbsolute(_)), "{err}");
}

#[test]
fn paths_layout_follows_the_state_dir() {
    let p = resolve_paths_from(Some(Path::new("/tmp/d"))).unwrap();
    assert_eq!(p.run, Path::new("/tmp/d/run"));
    assert_eq!(p.socket, Path::new("/tmp/d/run/damperd.sock"));
    assert_eq!(p.lock, Path::new("/tmp/d/run/damperd.lock"));
    assert_eq!(p.log, Path::new("/tmp/d/log"));
    assert_eq!(p.disabled, Path::new("/tmp/d/disabled"));
    assert!(p.config.ends_with(".config/damper/config.toml"));
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn prepare_creates_0700() {
    let tmp = tempfile::tempdir().unwrap();
    let p = resolve_paths_from(Some(&tmp.path().join("state"))).unwrap();
    prepare_dirs(&p).unwrap();
    for dir in [&p.state, &p.run, &p.log] {
        assert_eq!(mode(dir), 0o700, "{}", dir.display());
    }
    prepare_dirs(&p).unwrap();
}

#[test]
fn prepare_refuses_group_writable() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let p = resolve_paths_from(Some(&state)).unwrap();
    prepare_dirs(&p).unwrap();
    fs::set_permissions(&p.run, fs::Permissions::from_mode(0o770)).unwrap();
    let err = prepare_dirs(&p).unwrap_err();
    assert!(matches!(err, PathError::BadMode { .. }), "{err}");
}

#[test]
fn prepare_refuses_symlinked_state() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("elsewhere");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    let state = tmp.path().join("state");
    symlink(&target, &state).unwrap();
    let p = resolve_paths_from(Some(&state)).unwrap();
    let err = prepare_dirs(&p).unwrap_err();
    assert!(matches!(err, PathError::NotDirectory(_)), "{err}");
}

#[test]
fn prepare_refuses_symlinked_run() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let p = resolve_paths_from(Some(&state)).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let target = tmp.path().join("elsewhere");
    fs::create_dir(&target).unwrap();
    symlink(&target, &p.run).unwrap();
    let err = prepare_dirs(&p).unwrap_err();
    assert!(matches!(err, PathError::NotDirectory(_)), "{err}");
}
