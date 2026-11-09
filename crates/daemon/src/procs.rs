//! The daemon user's process table, from `libproc`.

use std::ffi::c_char;
use std::io;

use damper_core::account::ProcInfo;
use libproc::bsd_info::BSDInfo;
use libproc::pid_rusage::{RUsageInfoV4, pidrusage};
use libproc::proc_pid::{self, pidinfo};
use libproc::processes::{ProcFilter, pids_by_type};

use crate::sources::{ProcessSource, Snapshot};

/// The parent pid of processes reparented to `launchd`.
const LAUNCHD_PID: u32 = 1;
/// The name of sccache's long-lived server process.
const SCCACHE_NAME: &str = "sccache";

/// Lists the processes of one user through `libproc`.
///
/// Only that user's processes are listed, and only those the user can read.
#[derive(Debug, Clone, Copy)]
pub struct LibprocProcesses {
    uid: u32,
}

impl LibprocProcesses {
    /// A source for the processes owned by `uid`.
    #[must_use]
    pub fn new(uid: u32) -> Self {
        Self { uid }
    }
}

fn c_name(raw: &[c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| c.to_ne_bytes()[0])
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Prefers the 32-character `pbi_name` over the 16-character `pbi_comm`, so
/// names such as `build-script-build` survive intact.
fn pick_name(name: &[c_char], comm: &[c_char]) -> String {
    let long = c_name(name);
    if long.is_empty() { c_name(comm) } else { long }
}

/// Reads one process. A process that vanished or can't be read gives `None`.
fn read_proc(pid: u32) -> Option<ProcInfo> {
    let raw = i32::try_from(pid).ok()?;
    let bsd = pidinfo::<BSDInfo>(raw, 0).ok()?;
    let usage = pidrusage::<RUsageInfoV4>(raw).ok()?;
    Some(ProcInfo {
        pid,
        ppid: bsd.pbi_ppid,
        name: pick_name(&bsd.pbi_name, &bsd.pbi_comm),
        footprint: usage.ri_phys_footprint,
        peak: usage.ri_lifetime_max_phys_footprint,
    })
}

fn sccache_servers(procs: &[ProcInfo]) -> Vec<u32> {
    procs
        .iter()
        .filter(|p| p.ppid == LAUNCHD_PID && p.name == SCCACHE_NAME)
        .map(|p| p.pid)
        .collect()
}

impl ProcessSource for LibprocProcesses {
    fn snapshot(&self) -> io::Result<Snapshot> {
        let pids = pids_by_type(ProcFilter::ByUID { uid: self.uid })?;
        let procs: Vec<ProcInfo> = pids.into_iter().filter_map(read_proc).collect();
        let sccache_servers = sccache_servers(&procs);
        Ok(Snapshot {
            procs,
            sccache_servers,
        })
    }
}

/// The name of process `pid`, or `None` when it is gone or unreadable.
#[must_use]
pub fn process_name(pid: u32) -> Option<String> {
    let name = proc_pid::name(i32::try_from(pid).ok()?).ok()?;
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test")]
mod tests {
    use super::*;
    use crate::sources::ProcessSource;
    use nix::unistd::{getpid, getppid, getuid};
    use std::process::Command;

    fn source() -> LibprocProcesses {
        LibprocProcesses::new(getuid().as_raw())
    }

    #[test]
    fn libproc_finds_own_process() {
        let snap = source().snapshot().unwrap();
        let me = snap
            .procs
            .iter()
            .find(|p| p.pid == getpid().as_raw().unsigned_abs())
            .unwrap();
        assert!(me.footprint > 0);
        assert!(me.peak >= me.footprint);
        assert_eq!(me.ppid, getppid().as_raw().unsigned_abs());
        assert_ne!(me.name, "");
    }

    #[test]
    fn libproc_skips_vanished_pids() {
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(read_proc(pid).is_none());
        assert!(process_name(pid).is_none());
    }

    #[test]
    fn libproc_skips_unreadable_pids() {
        // pid 1 (launchd) belongs to root; a normal user can't read it.
        if getuid().is_root() {
            return;
        }
        assert!(read_proc(1).is_none());
    }

    #[test]
    fn name_prefers_long_name_over_comm() {
        let long = c_chars("build-script-build");
        let short = c_chars("build-script-bu");
        assert_eq!(pick_name(&long, &short), "build-script-build");
        assert_eq!(pick_name(&c_chars(""), &short), "build-script-bu");
        assert_eq!(pick_name(&c_chars(""), &c_chars("")), "");
    }

    fn c_chars(s: &str) -> Vec<std::ffi::c_char> {
        s.bytes()
            .map(|b| std::ffi::c_char::from_ne_bytes([b]))
            .chain(std::iter::once(0))
            .collect()
    }

    #[test]
    fn sccache_servers_are_named_sccache_with_ppid_1() {
        let procs = vec![
            info(10, 1, "sccache"),
            info(11, 500, "sccache"),
            info(12, 1, "rustc"),
        ];
        assert_eq!(sccache_servers(&procs), vec![10]);
    }

    fn info(pid: u32, parent: u32, name: &str) -> damper_core::account::ProcInfo {
        damper_core::account::ProcInfo {
            pid,
            ppid: parent,
            name: name.to_owned(),
            footprint: 0,
            peak: 0,
        }
    }
}
