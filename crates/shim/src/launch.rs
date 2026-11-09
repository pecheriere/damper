//! Building the command that runs the real cargo, with priority applied.

use damper_core::config::Priority;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// `/usr/sbin/taskpolicy`, which moves cargo to the utility `QoS` class.
pub const TASKPOLICY: &str = "/usr/sbin/taskpolicy";
/// `/usr/bin/nice`.
pub const NICE: &str = "/usr/bin/nice";

/// The command that runs `cargo` with `args` at `priority`.
///
/// `cargo` is passed as given, so its basename, `cargo`, is what rustup's
/// proxy sees. If the wrapper tool is not executable, cargo runs directly.
#[must_use]
pub fn command_for(
    cargo: &Path,
    args: &[OsString],
    priority: Priority,
    taskpolicy: &Path,
    nice: &Path,
) -> Command {
    let wrapper: Option<(&Path, &[&str])> = match priority {
        Priority::Utility => Some((taskpolicy, &["-c", "utility"])),
        Priority::Nice => Some((nice, &["-n", "15"])),
        Priority::None => None,
    };
    let mut command = match wrapper.filter(|(tool, _)| is_executable(tool)) {
        Some((tool, tool_args)) => {
            let mut c = Command::new(tool);
            c.args(tool_args).arg(cargo);
            c
        }
        None => Command::new(cargo),
    };
    command.args(args);
    command
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test")]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn argv(c: &Command) -> Vec<&OsStr> {
        c.get_args().collect()
    }

    fn exe(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn utility_wraps_with_taskpolicy() {
        let tmp = tempfile::tempdir().unwrap();
        let (tp, nice) = (exe(tmp.path(), "taskpolicy"), exe(tmp.path(), "nice"));
        let args = vec![OsString::from("build"), OsString::from("a b")];
        let c = command_for(Path::new("/x/cargo"), &args, Priority::Utility, &tp, &nice);
        assert_eq!(c.get_program(), tp.as_os_str());
        assert_eq!(
            argv(&c),
            ["-c", "utility", "/x/cargo", "build", "a b"].map(OsStr::new)
        );
    }

    #[test]
    fn nice_wraps_with_nice() {
        let tmp = tempfile::tempdir().unwrap();
        let (tp, nice) = (exe(tmp.path(), "taskpolicy"), exe(tmp.path(), "nice"));
        let c = command_for(Path::new("/x/cargo"), &[], Priority::Nice, &tp, &nice);
        assert_eq!(c.get_program(), nice.as_os_str());
        assert_eq!(argv(&c), ["-n", "15", "/x/cargo"].map(OsStr::new));
    }

    #[test]
    fn none_runs_cargo_directly() {
        let args = vec![OsString::from("check")];
        let c = command_for(
            Path::new("/x/cargo"),
            &args,
            Priority::None,
            Path::new("/nope"),
            Path::new("/nope"),
        );
        assert_eq!(c.get_program(), OsStr::new("/x/cargo"));
        assert_eq!(argv(&c), [OsStr::new("check")]);
    }

    #[test]
    fn missing_taskpolicy_runs_cargo_directly() {
        let args = vec![OsString::from("check")];
        let c = command_for(
            Path::new("/x/cargo"),
            &args,
            Priority::Utility,
            Path::new("/nope/taskpolicy"),
            Path::new("/nope/nice"),
        );
        assert_eq!(c.get_program(), OsStr::new("/x/cargo"));
        assert_eq!(argv(&c), [OsStr::new("check")]);
    }
}
