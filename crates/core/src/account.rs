//! Memory accounting for the process trees of tracked builds (spec §5.5).

use std::collections::{HashMap, HashSet};

use crate::classify::Kind;

/// One process as the sampler saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcInfo {
    /// Process id.
    pub pid: u32,
    /// Parent process id.
    pub ppid: u32,
    /// Executable name.
    pub name: String,
    /// Current physical footprint in bytes.
    pub footprint: u64,
    /// Peak physical footprint in bytes.
    pub peak: u64,
}

/// A session's cargo process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tracked {
    /// Pid of the cargo process that opened the session.
    pub root_pid: u32,
    /// What the invocation does.
    pub kind: Kind,
}

/// What one session's tree is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionUsage {
    /// The tree holds a compile process.
    pub compiling: bool,
    /// Test binaries running (test and nextest sessions only).
    pub test_jobs: u32,
}

/// The result of one accounting pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accounting {
    /// Footprint of every counted process, each pid once.
    pub total_footprint: u64,
    /// Largest peak among the counted compile processes.
    pub largest_job: u64,
    /// One entry per tracked session, in the order given.
    pub sessions: Vec<SessionUsage>,
}

/// Whether `name` is a compiler, linker or build script.
#[must_use]
pub fn is_compile_process(name: &str) -> bool {
    matches!(
        name,
        "rustc"
            | "rustdoc"
            | "clippy-driver"
            | "sccache"
            | "ld"
            | "ld64"
            | "cc"
            | "c++"
            | "clang"
            | "clang++"
            | "gcc"
            | "g++"
    ) || name.starts_with("build-script-")
}

/// Sums build memory over the tracked trees and the sccache servers.
///
/// A session tree is the root plus every descendant by parent pid. `Run`
/// sessions count only their compile processes, since the program they run is
/// not part of the build. Each pid is counted once, however many trees reach
/// it. `uid_sccache_servers` are the pids of sccache servers (parent pid 1);
/// all their descendants are counted too.
#[must_use]
pub fn account(procs: &[ProcInfo], tracked: &[Tracked], uid_sccache_servers: &[u32]) -> Accounting {
    let by_pid: HashMap<u32, &ProcInfo> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for process in procs {
        children.entry(process.ppid).or_default().push(process.pid);
    }
    let tree = |root: u32| descendants(root, &children);

    let mut counter = Counter::default();
    let mut sessions = Vec::with_capacity(tracked.len());
    for session in tracked {
        let members: Vec<&ProcInfo> = by_pid
            .get(&session.root_pid)
            .map(|root| {
                std::iter::once(*root)
                    .chain(
                        tree(root.pid)
                            .iter()
                            .filter_map(|pid| by_pid.get(pid).copied()),
                    )
                    .collect()
            })
            .unwrap_or_default();
        let compiling = members.iter().any(|p| is_compile_process(&p.name));
        let test_jobs = if matches!(session.kind, Kind::Test | Kind::Nextest) {
            members
                .iter()
                .skip(1)
                .filter(|p| !is_compile_process(&p.name) && !is_cargo(&p.name))
                .count()
        } else {
            0
        };
        for process in members {
            if session.kind != Kind::Run || is_compile_process(&process.name) {
                counter.add(process);
            }
        }
        sessions.push(SessionUsage {
            compiling,
            test_jobs: u32::try_from(test_jobs).unwrap_or(u32::MAX),
        });
    }
    for server in uid_sccache_servers {
        for pid in tree(*server) {
            if let Some(process) = by_pid.get(&pid) {
                counter.add(process);
            }
        }
    }
    Accounting {
        total_footprint: counter.total,
        largest_job: counter.largest_job,
        sessions,
    }
}

fn is_cargo(name: &str) -> bool {
    name == "cargo" || name.starts_with("cargo-")
}

/// Every pid below `root` (not `root` itself), each once, safe against cycles.
fn descendants(root: u32, children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut seen = HashSet::from([root]);
    let mut order = Vec::new();
    let mut pending = vec![root];
    while let Some(pid) = pending.pop() {
        for &child in children.get(&pid).into_iter().flatten() {
            if seen.insert(child) {
                order.push(child);
                pending.push(child);
            }
        }
    }
    order
}

#[derive(Default)]
struct Counter {
    counted: HashSet<u32>,
    total: u64,
    largest_job: u64,
}

impl Counter {
    fn add(&mut self, process: &ProcInfo) {
        if !self.counted.insert(process.pid) {
            return;
        }
        self.total = self.total.saturating_add(process.footprint);
        if is_compile_process(&process.name) {
            self.largest_job = self.largest_job.max(process.peak);
        }
    }
}

#[cfg(test)]
#[path = "account/tests.rs"]
mod tests;
