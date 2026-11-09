//! The damper broker daemon.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use damper_proto::paths::resolve_paths;
use damperd::memory::SysctlMemory;
use damperd::procs::LibprocProcesses;
use damperd::runtime::run;
use nix::unistd::getuid;

fn start() -> anyhow::Result<()> {
    let paths = resolve_paths().context("resolving damper's paths")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    runtime.block_on(run(
        paths,
        Arc::new(SysctlMemory),
        Arc::new(LibprocProcesses::new(getuid().as_raw())),
    ))
}

fn main() -> ExitCode {
    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Nothing else can be done if stderr is gone too.
            let _ = writeln!(std::io::stderr().lock(), "damperd: {e:#}");
            ExitCode::FAILURE
        }
    }
}
