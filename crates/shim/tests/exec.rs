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
        "#!/bin/sh\nprintf '%s\\n' \"$0\"\nfor a in \"$@\"; do printf '[%s]\\n' \"$a\"; done\nprintf 'nice=%s\\n' \"$(/bin/ps -o nice= -p $$ | /usr/bin/tr -d ' ')\"\nexit 3\n",
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

fn run_output(root: &Path, envs: &[(&str, &OsStr)]) -> std::process::Output {
    let fakebin = fake_proxy(root);
    let shim = shim_dir(root);
    let path = std::env::join_paths([&shim, &fakebin]).unwrap();
    let mut cmd = Command::new(shim.join("cargo"));
    cmd.env_clear().env("PATH", path);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.args(["build", "two words"]);
    cmd.arg(OsStr::from_bytes(b"bad\xffarg"));
    cmd.output().unwrap()
}

fn run(root: &Path, envs: &[(&str, &OsStr)]) -> (Vec<u8>, Option<i32>) {
    let out = run_output(root, envs);
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
    let (stdout, code) = run(tmp.path(), &[("DAMPER", OsStr::new("off"))]);
    assert_proxy_output(&stdout);
    assert_eq!(code, Some(3));
}

fn write_state_config(root: &Path, text: &str) -> std::path::PathBuf {
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("config.toml"), text).unwrap();
    state
}

/// The nice value the fake cargo reported.
fn reported_nice(stdout: &[u8]) -> i32 {
    let line = stdout.split(|b| *b == b'\n').nth(4).unwrap();
    std::str::from_utf8(line)
        .unwrap()
        .strip_prefix("nice=")
        .unwrap()
        .parse()
        .unwrap()
}

/// The nice value this test process runs at: the harness may already be
/// niced, so assertions are relative to it.
fn own_nice() -> i32 {
    let out = Command::new("/bin/ps")
        .args(["-o", "nice=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// What `nice -n 15` yields from the harness's own value (capped at 20).
fn niced() -> i32 {
    (own_nice() + 15).min(20)
}

#[test]
fn utility_config_in_state_dir_keeps_output_and_exit_code() {
    // taskpolicy's clamp has no unprivileged read-back, so this proves only
    // the passthrough with the config loaded; the wrapper argv is covered by
    // launch's unit test and the config-to-priority mapping by main's.
    let tmp = tempfile::tempdir().unwrap();
    let state = write_state_config(tmp.path(), "priority = \"utility\"\n");
    let (stdout, code) = run(tmp.path(), &[("DAMPER_STATE_DIR", state.as_os_str())]);
    assert_proxy_output(&stdout);
    assert_eq!(code, Some(3));
}

#[test]
fn nice_config_in_state_dir_lowers_cargos_priority() {
    let tmp = tempfile::tempdir().unwrap();
    let state = write_state_config(tmp.path(), "priority = \"nice\"\n");
    let (stdout, code) = run(tmp.path(), &[("DAMPER_STATE_DIR", state.as_os_str())]);
    assert_proxy_output(&stdout);
    assert_eq!(reported_nice(&stdout), niced());
    assert_eq!(code, Some(3));
}

#[test]
fn disabled_config_still_applies_priority() {
    let tmp = tempfile::tempdir().unwrap();
    let state = write_state_config(tmp.path(), "enabled = false\npriority = \"nice\"\n");
    let (stdout, _) = run(tmp.path(), &[("DAMPER_STATE_DIR", state.as_os_str())]);
    assert_eq!(reported_nice(&stdout), niced());
}

#[test]
fn damper_off_ignores_the_config_priority() {
    let tmp = tempfile::tempdir().unwrap();
    let state = write_state_config(tmp.path(), "priority = \"nice\"\n");
    let envs = [
        ("DAMPER_STATE_DIR", state.as_os_str()),
        ("DAMPER", OsStr::new("off")),
    ];
    let (stdout, _) = run(tmp.path(), &envs);
    assert_eq!(reported_nice(&stdout), own_nice());
}

#[test]
fn too_long_state_dir_still_runs_cargo_silently() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("s".repeat(120));
    let out = run_output(tmp.path(), &[("DAMPER_STATE_DIR", state.as_os_str())]);
    assert_proxy_output(&out.stdout);
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stderr.is_empty(), "stderr: {:?}", out.stderr);
}

#[test]
fn unexecutable_cargo_exits_127_naming_only_the_error_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let fakebin = tmp.path().join("fakebin");
    fs::create_dir_all(&fakebin).unwrap();
    let cargo = fakebin.join("cargo");
    fs::write(&cargo, "#!/nonexistent/interpreter\n").unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    let shim = shim_dir(tmp.path());
    let out = Command::new(shim.join("cargo"))
        .env_clear()
        .env("PATH", std::env::join_paths([&shim, &fakebin]).unwrap())
        // No wrapper: with one, taskpolicy would be the process that fails.
        .env("DAMPER", "off")
        .args(["build", "secret-arg"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert_eq!(
        out.stderr,
        b"damper: cannot execute cargo: entity not found\n"
    );
}

#[test]
fn whitespace_in_state_dir_still_runs_cargo() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("has space");
    let (stdout, code) = run(tmp.path(), &[("DAMPER_STATE_DIR", state.as_os_str())]);
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
