use super::*;

const GIB: u64 = 1 << 30;

fn proc(id: u32, parent: u32, name: &str, footprint: u64, peak: u64) -> ProcInfo {
    ProcInfo {
        pid: id,
        ppid: parent,
        name: name.to_owned(),
        footprint,
        peak,
    }
}

fn tracked(root_pid: u32, kind: Kind) -> Tracked {
    Tracked { root_pid, kind }
}

#[test]
fn account_counts_sccache_children() {
    let procs = [
        proc(100, 1, "cargo", 50, 50),
        proc(101, 100, "sccache", 10, 10),
        proc(50, 1, "sccache", 20, 20),
        proc(51, 50, "rustc", 2 * GIB, 5 * GIB / 2),
    ];
    let acct = account(&procs, &[tracked(100, Kind::Build)], &[50]);
    assert_eq!(acct.total_footprint, 50 + 10 + 2 * GIB);
    assert_eq!(acct.largest_job, 5 * GIB / 2);
    assert!(acct.sessions[0].compiling);
}

#[test]
fn account_run_session_ignores_app_memory() {
    let procs = [
        proc(100, 1, "cargo", 50, 50),
        proc(101, 100, "myserver", 3 * GIB, 3 * GIB),
    ];
    let acct = account(&procs, &[tracked(100, Kind::Run)], &[]);
    assert_eq!(acct.total_footprint, 0);
    assert_eq!(acct.largest_job, 0);
    assert!(!acct.sessions[0].compiling);
    assert_eq!(acct.sessions[0].test_jobs, 0);
}

#[test]
fn account_run_session_counts_its_compile_processes() {
    let procs = [
        proc(100, 1, "cargo", 50, 50),
        proc(101, 100, "rustc", 7, 9),
        proc(102, 100, "myserver", 3 * GIB, 3 * GIB),
    ];
    let acct = account(&procs, &[tracked(100, Kind::Run)], &[]);
    assert_eq!(acct.total_footprint, 7);
    assert_eq!(acct.largest_job, 9);
    assert!(acct.sessions[0].compiling);
}

#[test]
fn account_counts_test_jobs() {
    let procs = [
        proc(100, 1, "cargo", 1, 1),
        proc(101, 100, "cargo-nextest", 1, 1),
        proc(102, 101, "test-a", 1, 1),
        proc(103, 101, "test-b", 1, 1),
        proc(104, 101, "test-c", 1, 1),
        proc(105, 102, "rustc", 1, 1),
    ];
    let acct = account(&procs, &[tracked(100, Kind::Nextest)], &[]);
    assert_eq!(acct.sessions[0].test_jobs, 3);
    assert!(acct.sessions[0].compiling);
}

#[test]
fn account_ignores_test_jobs_for_other_kinds() {
    let procs = [proc(100, 1, "cargo", 1, 1), proc(101, 100, "test-a", 1, 1)];
    for kind in [Kind::Build, Kind::Run, Kind::PassThrough] {
        let acct = account(&procs, &[tracked(100, kind)], &[]);
        assert_eq!(acct.sessions[0].test_jobs, 0, "{kind:?}");
    }
    let acct = account(&procs, &[tracked(100, Kind::Test)], &[]);
    assert_eq!(acct.sessions[0].test_jobs, 1);
}

#[test]
fn account_counts_each_pid_once() {
    let procs = [
        proc(100, 1, "cargo", 1, 1),
        proc(50, 100, "sccache", 1, 1),
        proc(51, 50, "rustc", 100, 150),
    ];
    // sccache server 50 sits inside the session tree and is also passed in.
    let acct = account(&procs, &[tracked(100, Kind::Build)], &[50]);
    assert_eq!(acct.total_footprint, 102);
    assert_eq!(acct.largest_job, 150);
}

#[test]
fn account_counts_a_pid_shared_by_two_trees_once() {
    let procs = [
        proc(100, 1, "cargo", 1, 1),
        proc(200, 1, "cargo", 1, 1),
        proc(101, 100, "rustc", 10, 10),
    ];
    let both = [tracked(100, Kind::Build), tracked(100, Kind::Test)];
    let acct = account(&procs, &both, &[]);
    assert_eq!(acct.total_footprint, 11);
    assert_eq!(acct.sessions.len(), 2);
    assert!(acct.sessions.iter().all(|s| s.compiling));
}

#[test]
fn account_survives_parent_cycles() {
    let procs = [
        proc(100, 101, "cargo", 1, 1),
        proc(101, 100, "rustc", 10, 10),
    ];
    let acct = account(&procs, &[tracked(100, Kind::Build)], &[]);
    assert_eq!(acct.total_footprint, 11);
    assert!(acct.sessions[0].compiling);
}

#[test]
fn account_reports_sessions_in_tracked_order() {
    let procs = [
        proc(100, 1, "cargo", 1, 1),
        proc(200, 1, "cargo", 1, 1),
        proc(201, 200, "rustc", 1, 1),
    ];
    let acct = account(
        &procs,
        &[
            tracked(100, Kind::Build),
            tracked(200, Kind::Build),
            tracked(999, Kind::Build),
        ],
        &[],
    );
    let compiling: Vec<bool> = acct.sessions.iter().map(|s| s.compiling).collect();
    assert_eq!(compiling, [false, true, false]);
}

#[test]
fn compile_processes_are_recognised() {
    assert!(is_compile_process("rustc"));
    assert!(is_compile_process("build-script-build"));
    assert!(is_compile_process("clang++"));
    assert!(!is_compile_process("cargo"));
    assert!(!is_compile_process("myserver"));
}
