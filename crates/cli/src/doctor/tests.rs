#![expect(clippy::unwrap_used, reason = "test")]

use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread;

use damper_proto::paths::resolve_paths_from;

use super::{Check, doctor, doctor_with};

const QUICK: std::time::Duration = std::time::Duration::from_millis(100);

fn script(path: &Path) {
    fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
    checks.iter().find(|c| c.name == name).unwrap()
}

fn join(dirs: &[&Path]) -> OsString {
    std::env::join_paths(dirs).unwrap()
}

#[test]
fn doctor_flags_shim_not_first_on_path() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = resolve_paths_from(Some(tmp.path())).unwrap();
    let (shim_dir, real_dir) = (tmp.path().join("shim"), tmp.path().join("real"));
    fs::create_dir(&shim_dir).unwrap();
    fs::create_dir(&real_dir).unwrap();
    script(&shim_dir.join("damper-shim"));
    symlink(shim_dir.join("damper-shim"), shim_dir.join("cargo")).unwrap();
    script(&real_dir.join("cargo"));

    let good = doctor(&paths, &join(&[&shim_dir, &real_dir]));
    assert!(find(&good, "cargo on PATH").ok, "{good:?}");

    let bad = doctor(&paths, &join(&[&real_dir, &shim_dir]));
    assert!(!find(&bad, "cargo on PATH").ok, "{bad:?}");

    let missing = doctor(&paths, &OsString::new());
    assert!(!find(&missing, "cargo on PATH").ok);
}

#[test]
fn doctor_flags_loose_socket_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = resolve_paths_from(Some(tmp.path())).unwrap();
    fs::create_dir(&paths.run).unwrap();
    fs::set_permissions(&paths.run, fs::Permissions::from_mode(0o700)).unwrap();
    let _listener = UnixListener::bind(&paths.socket).unwrap();
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600)).unwrap();
    let ok = doctor_with(&paths, &OsString::new(), QUICK);
    assert!(find(&ok, "run directory").ok, "{ok:?}");
    assert!(find(&ok, "socket").ok, "{ok:?}");

    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(&paths.run, fs::Permissions::from_mode(0o755)).unwrap();
    let loose = doctor_with(&paths, &OsString::new(), QUICK);
    let socket = find(&loose, "socket");
    assert!(!socket.ok);
    assert!(socket.detail.contains("0644"), "{}", socket.detail);
    assert!(!find(&loose, "run directory").ok);
}

#[test]
fn doctor_reports_config_and_disabled_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = resolve_paths_from(Some(tmp.path())).unwrap();
    fs::write(&paths.config, "bogus = 1\n").unwrap();
    fs::write(&paths.disabled, "").unwrap();
    let checks = doctor(&paths, &OsString::new());
    assert!(!find(&checks, "config").ok);
    assert!(find(&checks, "config").detail.contains("bogus"));
    assert!(
        find(&checks, "disabled flag (damper off when set)")
            .detail
            .contains("damper is off")
    );
}

#[test]
fn doctor_flags_symlinked_run_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = resolve_paths_from(Some(tmp.path())).unwrap();
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&elsewhere, &paths.run).unwrap();
    assert!(!find(&doctor(&paths, &OsString::new()), "run directory").ok);
}

#[test]
fn doctor_reports_a_protocol_version_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = resolve_paths_from(Some(tmp.path())).unwrap();
    fs::create_dir(&paths.run).unwrap();
    fs::set_permissions(&paths.run, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(&paths.socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        (&stream)
            .write_all(b"{\"v\":999,\"type\":\"status\"}\n")
            .unwrap();
    });
    let checks = doctor_with(&paths, &OsString::new(), std::time::Duration::from_secs(2));
    server.join().unwrap();
    let daemon = find(&checks, "daemon");
    assert!(!daemon.ok);
    assert!(daemon.detail.contains("version 999"), "{}", daemon.detail);
}

#[test]
fn doctor_names_the_disabled_flag_as_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = resolve_paths_from(Some(tmp.path())).unwrap();
    fs::write(&paths.disabled, "").unwrap();
    let checks = doctor(&paths, &OsString::new());
    let flag = find(&checks, "disabled flag (damper off when set)");
    assert!(flag.ok);
    assert!(flag.detail.contains("disabled"), "{}", flag.detail);
    assert!(flag.name.contains("disabled"));
}
