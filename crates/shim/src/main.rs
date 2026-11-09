//! The cargo shim that joins damper's jobserver pool.
//!
//! Every path that does not open a session execs the real cargo (spec §4).
//! When in doubt, the shim execs the real cargo: damper never changes a
//! build's outcome.

mod decide;
mod fallback;
mod hold;
mod ignored;
mod launch;
mod resolve;
mod session;
mod signals;
mod supervise;

use damper_core::classify::{Invocation, ThreadEnv, classify};
use damper_core::config::{Effective, Machine, Priority, load};
use damper_proto::paths::{Paths, resolve_paths, resolve_paths_from};
use decide::{Route, ShimEnv, route};
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const NO_CARGO: &str = "damper: no cargo found on PATH after damper's shim\n";

/// Memory handed to `config::load`. The shim's routing reads only `enabled`
/// and `priority`, which do not depend on the machine's size.
const ROUTING_MEMORY: u64 = 1 << 30;

fn main() -> ExitCode {
    std::panic::set_hook(Box::new(|_| {}));
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let Some(cargo) = std::env::current_exe()
        .ok()
        .and_then(|own| resolve::find_real_cargo(&path_var, &own))
    else {
        return fail(NO_CARGO, 127);
    };
    let plan = catch_unwind(|| plan(&args)).unwrap_or(Plan::Exec(Priority::None));
    let (cargo_path, cargo_args) = (cargo.as_path(), args.as_slice());
    let with_priority = |priority| {
        move || {
            launch::command_for(
                cargo_path,
                cargo_args,
                priority,
                Path::new(launch::TASKPOLICY),
                Path::new(launch::NICE),
            )
        }
    };
    let command = match plan {
        Plan::Exec(priority) => guarded(with_priority(priority), &cargo, &args),
        Plan::Session {
            paths,
            inv,
            priority,
        } => {
            let session = catch_unwind(AssertUnwindSafe(|| {
                supervise::run(&paths, &inv, with_priority(priority))
            }));
            match session {
                Ok(supervise::Outcome::Exit(code)) => return code,
                Ok(supervise::Outcome::Exec(command)) => command,
                // A panic before cargo starts: run it plainly. Once cargo
                // has started, `supervise::run` catches its own panics.
                Err(_) => direct_command(&cargo, &args),
            }
        }
    };
    exec_or_fall_back(command, &cargo, &args)
}

/// How this invocation runs.
enum Plan {
    /// Exec the real cargo at this priority.
    Exec(Priority),
    /// Open a session; on any failure, exec at `priority`.
    Session {
        paths: Paths,
        inv: Invocation,
        priority: Priority,
    },
}

/// Runs `build`; if it panics, the command is cargo itself with no wrapper.
fn guarded(build: impl FnOnce() -> Command, cargo: &Path, args: &[OsString]) -> Command {
    catch_unwind(AssertUnwindSafe(build)).unwrap_or_else(|_| direct_command(cargo, args))
}

fn direct_command(cargo: &Path, args: &[OsString]) -> Command {
    let mut command = Command::new(cargo);
    command.args(args);
    command
}

/// Execs `command`; if that fails, execs cargo directly; if that fails too,
/// reports the error kind (never the arguments or environment).
fn exec_or_fall_back(mut command: Command, cargo: &Path, args: &[OsString]) -> ExitCode {
    let _ = command.exec();
    let error = direct_command(cargo, args).exec();
    let kind = error.kind();
    fail(&exec_failure_message(kind), exec_failure_code(kind))
}

fn exec_failure_message(kind: io::ErrorKind) -> String {
    format!("damper: cannot execute cargo: {kind}\n")
}

/// 127 for "not found", 126 for anything else, as shells do.
fn exec_failure_code(kind: io::ErrorKind) -> u8 {
    if kind == io::ErrorKind::NotFound {
        127
    } else {
        126
    }
}

fn fail(message: &str, code: u8) -> ExitCode {
    let _ = std::io::stderr().lock().write_all(message.as_bytes());
    ExitCode::from(code)
}

/// Works out how to run cargo: plainly at a priority, or in a session.
fn plan(args: &[OsString]) -> Plan {
    let paths = resolve_paths();
    // A bad state dir must not turn off the user's priority, so the config
    // path falls back to the default location.
    let config_path = match &paths {
        Ok(p) => Some(p.config.clone()),
        Err(_) => resolve_paths_from(None).ok().map(|p| p.config),
    };
    let text = config_path.and_then(|p| std::fs::read_to_string(p).ok());
    let cfg = effective_config(text.as_deref());
    let threads = ThreadEnv {
        nextest: std::env::var_os("NEXTEST_TEST_THREADS").is_some(),
        rust_test: std::env::var_os("RUST_TEST_THREADS").is_some(),
    };
    let inv = classify(args, &cfg.extra_build_subcommands, threads);
    let env = shim_env(paths.as_ref().ok());
    let disabled = paths.as_ref().is_ok_and(|p| p.disabled.exists());
    match (route(env, inv, &cfg, disabled), paths) {
        (Route::Exec { priority }, _) => Plan::Exec(priority),
        (Route::Session(inv), Ok(paths)) => Plan::Session {
            paths,
            inv,
            priority: cfg.priority,
        },
        // No usable state dir means no daemon to talk to.
        (Route::Session(_), Err(_)) => Plan::Exec(cfg.priority),
    }
}

fn effective_config(text: Option<&str>) -> Effective {
    let cores = std::thread::available_parallelism()
        .map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX));
    load(
        text,
        Machine {
            cores,
            memory: ROUTING_MEMORY,
        },
    )
    .effective
}

fn shim_env(paths: Option<&Paths>) -> ShimEnv {
    let damper_off = std::env::var_os("DAMPER").is_some_and(|v| v == OsStr::new("off"));
    let session = std::env::var_os("DAMPER_SESSION").is_some();
    let in_run_dir = paths
        .zip(std::env::var_os("CARGO_MAKEFLAGS"))
        .is_some_and(|(p, flags)| names_fifo_in(&flags, &p.run));
    ShimEnv {
        damper_off,
        nested: session || in_run_dir,
    }
}

/// Whether `flags` has a `--jobserver-auth=fifo:<path>` inside `run_dir`.
fn names_fifo_in(flags: &OsStr, run_dir: &Path) -> bool {
    flags
        .to_string_lossy()
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("--jobserver-auth=fifo:"))
        .any(|fifo| PathBuf::from(fifo).starts_with(run_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_before_exec_falls_back_to_plain_cargo() {
        let args = vec![OsString::from("build")];
        let cargo = Path::new("/x/cargo");
        let c = guarded(|| panic!("boom"), cargo, &args);
        assert_eq!(c.get_program(), cargo.as_os_str());
        assert_eq!(c.get_args().collect::<Vec<_>>(), [OsStr::new("build")]);
    }

    #[test]
    fn exec_failures_map_to_shell_exit_codes() {
        assert_eq!(exec_failure_code(io::ErrorKind::NotFound), 127);
        assert_eq!(exec_failure_code(io::ErrorKind::PermissionDenied), 126);
        assert_eq!(
            exec_failure_message(io::ErrorKind::NotFound),
            "damper: cannot execute cargo: entity not found\n"
        );
    }

    #[test]
    fn config_text_selects_the_priority() {
        let text = |p: &str| format!("priority = \"{p}\"\n");
        for (name, expected) in [
            ("utility", Priority::Utility),
            ("nice", Priority::Nice),
            ("none", Priority::None),
        ] {
            assert_eq!(effective_config(Some(&text(name))).priority, expected);
        }
        assert_eq!(effective_config(None).priority, Priority::Utility);
    }

    #[test]
    fn fifo_in_run_dir_is_nested() {
        let run = Path::new("/s/run");
        assert!(names_fifo_in(
            OsStr::new("-j --jobserver-auth=fifo:/s/run/s-1-2.fifo"),
            run
        ));
        assert!(!names_fifo_in(
            OsStr::new("-j --jobserver-auth=fifo:/other/x.fifo"),
            run
        ));
        assert!(!names_fifo_in(OsStr::new("-j --jobserver-auth=3,4"), run));
    }
}
