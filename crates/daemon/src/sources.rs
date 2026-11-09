//! Traits for the daemon's system sources, so the sampling loop can run
//! against fakes in tests.

use std::io;

use damper_core::account::ProcInfo;
use damper_core::broker::MemorySample;

/// Reads the system's memory state.
pub trait MemorySource: Send + Sync {
    /// Takes one memory sample.
    ///
    /// # Errors
    ///
    /// Returns an error when a required value can't be read.
    fn sample(&self) -> io::Result<MemorySample>;
}

/// Everything the process table shows for one tick.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// The readable processes of the daemon's user.
    pub procs: Vec<ProcInfo>,
    /// Pids of the user's sccache servers (named `sccache`, parent pid 1).
    pub sccache_servers: Vec<u32>,
}

/// Reads the process table.
pub trait ProcessSource: Send + Sync {
    /// Takes one snapshot of the process table.
    ///
    /// # Errors
    ///
    /// Returns an error when the pids can't be listed at all. Processes that
    /// vanish or can't be read are skipped.
    fn snapshot(&self) -> io::Result<Snapshot>;
}
