//! Classification of cargo invocations: which ones open a build session and
//! whether the user already chose a test thread count (spec §5.4, §5.6).

use std::ffi::{OsStr, OsString};

/// What a cargo invocation does, as far as damper is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Compiles; opens a session.
    Build,
    /// `test` or `bench`: compiles, then runs libtest binaries.
    Test,
    /// `nextest`: compiles, then nextest runs the test binaries itself.
    Nextest,
    /// `run`: compiles, then runs the program.
    Run,
    /// Does not compile; no session, no hold.
    PassThrough,
}

impl Kind {
    /// Daemon-side mapping from the subcommand name the shim sent.
    #[must_use]
    pub fn from_subcommand(name: &str) -> Kind {
        match name {
            "test" | "bench" => Kind::Test,
            "nextest" => Kind::Nextest,
            "run" => Kind::Run,
            _ => Kind::Build,
        }
    }
}

/// The result of classifying one invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// What the invocation does.
    pub kind: Kind,
    /// Canonical subcommand name (`t` becomes `test`), `None` for pass-through.
    pub subcommand: Option<String>,
    /// The user already set the test thread count; the shim must not.
    pub user_threads: bool,
}

/// Which test-thread environment variables the user has set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThreadEnv {
    /// `NEXTEST_TEST_THREADS` is set.
    pub nextest: bool,
    /// `RUST_TEST_THREADS` is set.
    pub rust_test: bool,
}

const BUILD_SUBCOMMANDS: [&str; 12] = [
    "build", "check", "clippy", "test", "nextest", "bench", "doc", "run", "install", "rustc",
    "rustdoc", "fix",
];

/// Options that consume the next argument when written without `=`.
const VALUE_OPTIONS: [&str; 4] = ["--config", "-C", "--color", "-Z"];

/// Options that make cargo print something and exit without building.
const INFO_OPTIONS: [&str; 5] = ["--version", "-V", "--help", "-h", "--list"];

fn canonical(name: &str) -> &str {
    match name {
        "b" => "build",
        "c" => "check",
        "t" => "test",
        "r" => "run",
        "d" => "doc",
        other => other,
    }
}

fn pass_through() -> Invocation {
    Invocation {
        kind: Kind::PassThrough,
        subcommand: None,
        user_threads: false,
    }
}

/// Finds the subcommand: skips the toolchain override and global options.
/// Returns its index in `args`, or `None` when cargo only prints information
/// or the arguments end first.
fn find_subcommand(args: &[OsString]) -> Option<usize> {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let text = arg.to_string_lossy();
        if INFO_OPTIONS.contains(&&*text) {
            return None;
        }
        if VALUE_OPTIONS.contains(&&*text) {
            // `--opt value`: the value is skipped with the option.
            i += 2;
        } else if text.starts_with('-') || text.starts_with('+') {
            // `--opt=value`, joined `-Zfoo` / `-Cdir`, or a plain flag.
            i += 1;
        } else {
            return Some(i);
        }
    }
    None
}

fn is_thread_flag(arg: &OsStr, short_jobs: bool) -> bool {
    let text = arg.to_string_lossy();
    text == "--test-threads"
        || text.starts_with("--test-threads=")
        || (short_jobs && text.starts_with("-j"))
}

fn user_set_threads(kind: Kind, rest: &[OsString], env: ThreadEnv) -> bool {
    match kind {
        Kind::Nextest => {
            if rest.first().is_none_or(|a| a != "run") {
                return false;
            }
            env.nextest
                || rest[1..]
                    .iter()
                    .take_while(|a| *a != "--")
                    .any(|a| is_thread_flag(a, true))
        }
        Kind::Test => {
            env.rust_test
                || rest
                    .iter()
                    .skip_while(|a| *a != "--")
                    .skip(1)
                    .any(|a| is_thread_flag(a, false))
        }
        Kind::Build | Kind::Run | Kind::PassThrough => false,
    }
}

/// Classifies a cargo invocation. `args` excludes `argv[0]`; `extra` holds
/// the configured extra build subcommands.
#[must_use]
pub fn classify(args: &[OsString], extra: &[String], env_threads: ThreadEnv) -> Invocation {
    let Some(index) = find_subcommand(args) else {
        return pass_through();
    };
    let name = canonical(&args[index].to_string_lossy()).to_owned();
    let kind = if BUILD_SUBCOMMANDS.contains(&name.as_str()) {
        Kind::from_subcommand(&name)
    } else if extra.contains(&name) {
        Kind::Build
    } else {
        return pass_through();
    };
    let user_threads = user_set_threads(kind, &args[index + 1..], env_threads);
    Invocation {
        kind,
        subcommand: Some(name),
        user_threads,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn run(args: &[&str], extra: &[&str], env: ThreadEnv) -> Invocation {
        let extra: Vec<String> = extra.iter().map(|s| (*s).to_owned()).collect();
        classify(&os(args), &extra, env)
    }

    fn inv(kind: Kind, sub: Option<&str>, user_threads: bool) -> Invocation {
        Invocation {
            kind,
            subcommand: sub.map(str::to_owned),
            user_threads,
        }
    }

    const NONE: ThreadEnv = ThreadEnv {
        nextest: false,
        rust_test: false,
    };

    #[test]
    fn classifies_build_subcommands() {
        let cases: Vec<(&[&str], &[&str], ThreadEnv, Invocation)> = vec![
            (
                &["build"],
                &[],
                NONE,
                inv(Kind::Build, Some("build"), false),
            ),
            (
                &[
                    "+nightly",
                    "--locked",
                    "-C",
                    "dir",
                    "--config",
                    "k=v",
                    "build",
                    "--release",
                ],
                &[],
                NONE,
                inv(Kind::Build, Some("build"), false),
            ),
            (
                &["--config=k=v", "t"],
                &[],
                NONE,
                inv(Kind::Test, Some("test"), false),
            ),
            (
                &["-Zunstable-options", "-Cdir", "b"],
                &[],
                NONE,
                inv(Kind::Build, Some("build"), false),
            ),
            (
                &["--color=never", "c"],
                &[],
                NONE,
                inv(Kind::Build, Some("check"), false),
            ),
            (&["r"], &[], NONE, inv(Kind::Run, Some("run"), false)),
            (&["d"], &[], NONE, inv(Kind::Build, Some("doc"), false)),
            (&["bench"], &[], NONE, inv(Kind::Test, Some("bench"), false)),
            (
                &["xtask"],
                &["xtask"],
                NONE,
                inv(Kind::Build, Some("xtask"), false),
            ),
            (
                &["-Z", "unstable-options", "check"],
                &[],
                NONE,
                inv(Kind::Build, Some("check"), false),
            ),
        ];
        for (args, extra, env, want) in cases {
            assert_eq!(run(args, extra, env), want, "args: {args:?}");
        }
    }

    #[test]
    fn classifies_thread_ownership() {
        let cases: Vec<(&[&str], &[&str], ThreadEnv, Invocation)> = vec![
            (
                &["nextest", "run", "-j", "4"],
                &[],
                NONE,
                inv(Kind::Nextest, Some("nextest"), true),
            ),
            (
                &["nextest", "run", "-j4"],
                &[],
                NONE,
                inv(Kind::Nextest, Some("nextest"), true),
            ),
            (
                &["nextest", "run", "--test-threads=2"],
                &[],
                NONE,
                inv(Kind::Nextest, Some("nextest"), true),
            ),
            (
                &["nextest", "run"],
                &[],
                NONE,
                inv(Kind::Nextest, Some("nextest"), false),
            ),
            (
                &["nextest", "run"],
                &[],
                ThreadEnv {
                    nextest: true,
                    rust_test: false,
                },
                inv(Kind::Nextest, Some("nextest"), true),
            ),
            (
                &["nextest", "list"],
                &[],
                NONE,
                inv(Kind::Nextest, Some("nextest"), false),
            ),
            (
                &["nextest", "list"],
                &[],
                ThreadEnv {
                    nextest: true,
                    rust_test: false,
                },
                inv(Kind::Nextest, Some("nextest"), false),
            ),
            (
                &["test", "--", "--test-threads=2"],
                &[],
                NONE,
                inv(Kind::Test, Some("test"), true),
            ),
            (
                &["test", "--test-threads=2"],
                &[],
                NONE,
                inv(Kind::Test, Some("test"), false),
            ),
            (
                &["test"],
                &[],
                ThreadEnv {
                    nextest: false,
                    rust_test: true,
                },
                inv(Kind::Test, Some("test"), true),
            ),
            (
                &["run", "--", "--test-threads"],
                &[],
                NONE,
                inv(Kind::Run, Some("run"), false),
            ),
        ];
        for (args, extra, env, want) in cases {
            assert_eq!(run(args, extra, env), want, "args: {args:?}");
        }
    }

    #[test]
    fn classifies_pass_through() {
        let cases: Vec<(&[&str], &[&str], ThreadEnv, Invocation)> = vec![
            (
                &["metadata"],
                &[],
                NONE,
                inv(Kind::PassThrough, None, false),
            ),
            (&["fmt"], &[], NONE, inv(Kind::PassThrough, None, false)),
            (
                &["--version"],
                &[],
                NONE,
                inv(Kind::PassThrough, None, false),
            ),
            (&["-V"], &[], NONE, inv(Kind::PassThrough, None, false)),
            (&["--list"], &[], NONE, inv(Kind::PassThrough, None, false)),
            (
                &["--help", "build"],
                &[],
                NONE,
                inv(Kind::PassThrough, None, false),
            ),
            (&[], &[], NONE, inv(Kind::PassThrough, None, false)),
            (&["-C"], &[], NONE, inv(Kind::PassThrough, None, false)),
            (&["xtask"], &[], NONE, inv(Kind::PassThrough, None, false)),
        ];
        for (args, extra, env, want) in cases {
            assert_eq!(run(args, extra, env), want, "args: {args:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_value_before_subcommand_is_skipped() {
        let args = vec![
            OsString::from("-C"),
            OsString::from_vec(vec![0x66, 0xff, 0x6f]),
            OsString::from("build"),
        ];
        assert_eq!(
            classify(&args, &[], NONE),
            inv(Kind::Build, Some("build"), false)
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_flag_before_subcommand_is_skipped() {
        let args = vec![
            OsString::from_vec(vec![b'-', b'-', 0xff]),
            OsString::from("check"),
        ];
        assert_eq!(
            classify(&args, &[], NONE),
            inv(Kind::Build, Some("check"), false)
        );
    }

    #[test]
    fn from_subcommand_maps_names() {
        assert_eq!(Kind::from_subcommand("test"), Kind::Test);
        assert_eq!(Kind::from_subcommand("bench"), Kind::Test);
        assert_eq!(Kind::from_subcommand("nextest"), Kind::Nextest);
        assert_eq!(Kind::from_subcommand("run"), Kind::Run);
        assert_eq!(Kind::from_subcommand("build"), Kind::Build);
        assert_eq!(Kind::from_subcommand("xtask"), Kind::Build);
    }
}
