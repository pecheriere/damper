//! Deciding whether an invocation gets a session (spec §4, §7.1).

use damper_core::classify::{Invocation, Kind};
use damper_core::config::{Effective, Priority};

/// What the environment says about this invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShimEnv {
    /// `DAMPER=off`.
    pub damper_off: bool,
    /// Already inside a damper session: `DAMPER_SESSION` is set, or
    /// `CARGO_MAKEFLAGS` names a FIFO in damper's run dir.
    pub nested: bool,
}

/// How the shim handles one invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// Exec the real cargo at this priority.
    Exec {
        /// Priority wrapper to apply.
        priority: Priority,
    },
    /// Open a session for this invocation.
    Session(Invocation),
}

/// Routes one invocation.
#[must_use]
pub fn route(env: ShimEnv, inv: Invocation, cfg: &Effective, disabled_flag: bool) -> Route {
    if env.damper_off || env.nested {
        return Route::Exec {
            priority: Priority::None,
        };
    }
    if disabled_flag || !cfg.enabled || inv.kind == Kind::PassThrough {
        return Route::Exec {
            priority: cfg.priority,
        };
    }
    Route::Session(inv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use damper_core::config::{Machine, load};

    fn cfg(enabled: bool, priority: &str) -> Effective {
        let text = format!("enabled = {enabled}\npriority = \"{priority}\"\n");
        load(
            Some(&text),
            Machine {
                cores: 8,
                memory: 16 << 30,
            },
        )
        .effective
    }

    fn inv(kind: Kind) -> Invocation {
        Invocation {
            kind,
            subcommand: Some("build".to_owned()),
            user_threads: false,
        }
    }

    const PLAIN: ShimEnv = ShimEnv {
        damper_off: false,
        nested: false,
    };

    fn exec(priority: Priority) -> Route {
        Route::Exec { priority }
    }

    #[test]
    fn route_table() {
        let on = cfg(true, "utility");
        let off = cfg(false, "nice");
        let none = Priority::None;
        let rows = [
            (
                ShimEnv {
                    damper_off: true,
                    ..PLAIN
                },
                inv(Kind::Build),
                &on,
                false,
                exec(none),
            ),
            (
                ShimEnv {
                    nested: true,
                    ..PLAIN
                },
                inv(Kind::Build),
                &on,
                false,
                exec(none),
            ),
            (PLAIN, inv(Kind::Build), &on, true, exec(Priority::Utility)),
            (PLAIN, inv(Kind::Build), &off, false, exec(Priority::Nice)),
            (
                PLAIN,
                inv(Kind::PassThrough),
                &on,
                false,
                exec(Priority::Utility),
            ),
            (
                PLAIN,
                inv(Kind::Build),
                &on,
                false,
                Route::Session(inv(Kind::Build)),
            ),
            (
                PLAIN,
                inv(Kind::Test),
                &on,
                false,
                Route::Session(inv(Kind::Test)),
            ),
        ];
        for (env, invocation, config, flag, expected) in rows {
            assert_eq!(route(env, invocation, config, flag), expected);
        }
    }
}
