//! The machine's size, for `config check`.

use std::io;

use damper_core::config::Machine;
use sysctl::{Ctl, CtlValue, Sysctl};

/// Logical cores and physical memory, read as the daemon reads them.
///
/// # Errors
/// If `hw.memsize` cannot be read.
pub fn read() -> io::Result<Machine> {
    let cores = std::thread::available_parallelism()
        .map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX));
    let value = Ctl::new("hw.memsize")
        .and_then(|ctl| ctl.value())
        .map_err(|e| io::Error::other(format!("sysctl hw.memsize: {e}")))?;
    let memory = match value {
        CtlValue::Ulong(v) | CtlValue::U64(v) => Some(v),
        CtlValue::Long(v) | CtlValue::S64(v) => u64::try_from(v).ok(),
        CtlValue::Uint(v) => Some(u64::from(v)),
        CtlValue::Int(v) => u64::try_from(v).ok(),
        _ => None,
    }
    .ok_or_else(|| io::Error::other("sysctl hw.memsize: unexpected value"))?;
    Ok(Machine { cores, memory })
}
