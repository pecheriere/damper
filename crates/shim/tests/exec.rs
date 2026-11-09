//! End-to-end: the built shim finds and execs a fake rustup proxy.
#![expect(clippy::unwrap_used, reason = "test")]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

fn fake_proxy(root: &Path) -> std::path::PathBuf {
    let fakebin = root.join("fakebin");
    fs::create_dir_all(&fakebin).unwrap();
    let rustup = fakebin.join("rustup");
    fs::write(
        &rustup,
        "#!/bin/sh\nprintf '%s\\n' \"$0\"\nfor a in \"$@\"; do printf '[%s]\\n' \"$a\"; done\nexit 3\n",
    )
    .unwrap();
    fs::set_permissions(&rustup, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&rustup, fakebin.join("cargo")).unwrap();
    fakebin
}

fn shim_dir(root: &Path) -> std::path::PathBuf {
    let dir = root.join("shimdir");
    fs::create_dir_all(&dir).unwrap();
    symlink(env!("CARGO_BIN_EXE_damper-shim"), dir.join("cargo")).unwrap();
    dir
}

fn run(root: &Path, envs: &[(&str, &OsStr)], remove: &[&str]) -> (Vec<u8>, Option<i32>) {
    let fakebin = fake_proxy(root);
    let shim = shim_dir(root);
    let path = std::env::join_paths([&shim, &fakebin]).unwrap();
    let mut cmd = Command::new(shim.join("cargo"));
    cmd.env_clear().env("PATH", path);
    cmd.env("HOME", root);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    for k in remove {
        cmd.env_remove(k);
    }
    cmd.args(["build", "two words"]);
    cmd.arg(OsStr::from_bytes(b"bad\xffarg"));
    let out = cmd.output().unwrap();
    (out.stdout, out.status.code())
}

fn assert_proxy_output(stdout: &[u8]) {
    let lines: Vec<&[u8]> = stdout.split(|b| *b == b'\n').collect();
    assert!(
        lines[0].ends_with(b"fakebin/cargo"),
        "argv0 was {:?}",
        lines[0]
    );
    assert_eq!(lines[1], b"[build]");
    assert_eq!(lines[2], b"[two words]");
    assert_eq!(lines[3], b"[bad\xffarg]");
}

#[test]
fn damper_off_execs_proxy_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, code) = run(tmp.path(), &[("DAMPER", OsStr::new("off"))], &[]);
    assert_proxy_output(&stdout);
    assert_eq!(code, Some(3));
}

#[test]
fn utility_priority_without_daemon_keeps_output_and_exit_code() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let cfg = tmp.path().join(".config/damper");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("config.toml"), "priority = \"utility\"\n").unwrap();
    let (stdout, code) = run(tmp.path(), &[("DAMPER_STATE_DIR", state.as_os_str())], &[]);
    assert_proxy_output(&stdout);
    assert_eq!(code, Some(3));
}

#[test]
fn whitespace_in_state_dir_still_runs_cargo() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("has space");
    let (stdout, code) = run(tmp.path(), &[("DAMPER_STATE_DIR", state.as_os_str())], &[]);
    assert_proxy_output(&stdout);
    assert_eq!(code, Some(3));
}

#[test]
fn no_cargo_after_shim_exits_127() {
    let tmp = tempfile::tempdir().unwrap();
    let shim = shim_dir(tmp.path());
    let out = Command::new(shim.join("cargo"))
        .env_clear()
        .env("PATH", &shim)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert_eq!(
        out.stderr,
        b"damper: no cargo found on PATH after damper's shim\n"
    );
}
