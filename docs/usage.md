# Using damper

damper has three parts. `damperd` is the daemon: it watches memory pressure and
hands out build slots. `damper-shim` is installed as `cargo`: it asks the daemon
for slots, then runs the real cargo. `damper` is the command line for looking at
what the daemon is doing. damper is for macOS on Apple Silicon only.

## Install

Build from source with a stable Rust toolchain:

```sh
cargo build --release
```

This puts `damper`, `damperd` and `damper-shim` in `target/release`. Install the
shim first on `PATH`, named `cargo`:

```sh
mkdir -p ~/.local/share/damper/shim && ln -sf "$(pwd)/target/release/damper-shim" ~/.local/share/damper/shim/cargo
```

and put its directory before cargo's in your shell profile:

```sh
export PATH="$HOME/.local/share/damper/shim:$PATH"
```

Run the daemon by hand, in a terminal you leave open:

```sh
target/release/damperd
```

There is no `brew services` support yet; it comes in Phase 1b. Then check the
install:

```sh
target/release/damper doctor
```

## What it does to a build

For `build`, `check`, `clippy`, `test`, `nextest`, `bench`, `doc`, `run`,
`install`, `rustc`, `rustdoc` and `fix`, the shim opens a session with the
daemon. The daemon feeds a jobserver FIFO with tokens as fast as memory
pressure allows, and the shim points cargo at it through `CARGO_MAKEFLAGS` and
`MAKEFLAGS`. Under pressure the pool shrinks and new builds wait ("hold"
lines on stderr, every 30 s) for at most `max_hold`; after that the build runs
anyway. Other subcommands (`fmt`, `--version`, and so on) go straight to cargo.

The shim sets `DAMPER_SESSION`, the jobserver variables and, for `test` and
`nextest`, `RUST_TEST_THREADS` / `NEXTEST_TEST_THREADS` unless you set them.
damper never changes whether a build succeeds.

### Priority

By default cargo runs at the `utility` QoS class. Builds take roughly 25 to 35%
longer, and the Mac stays responsive (measured in Phase 0, see
[the findings](superpowers/spikes/2026-11-09-phase0-findings.md)). Set
`priority = "none"` to give that up.

### Aliases

The built-in aliases `b`, `c`, `t`, `r` and `d` map to `build`, `check`,
`test`, `run` and `doc`. Aliases you define yourself in cargo's config pass
through, because the shim can't see what they expand to. To have one treated as
a build, list it in `extra_build_subcommands`.

## Configuration

The file is `~/.config/damper/config.toml`, with the home directory taken from
the passwd database, not `$HOME`. When `DAMPER_STATE_DIR` is set (development
and testing), the state and the config both live under it, and the file is
`$DAMPER_STATE_DIR/config.toml`. The daemon reads it at start; restart it after
a change. A bad key falls back to its default with one warning naming it; the
other keys are kept. `damper config check` prints the effective values and the
warnings.

| Key | Default | Meaning |
|---|---|---|
| `cores` | all cores | Most build jobs at once. An integer or a percentage of the machine's logical cores, such as `"50%"`. Clamped to the machine. |
| `memory` | `"75%"` of RAM | Most memory builds may use. A size such as `"24GiB"` or a percentage such as `"50%"`. Clamped to physical memory. |
| `max_hold` | `"10m"` | Longest a build waits for slots before running anyway. A duration such as `"90s"`, `"10m"`, `"1h"`. |
| `enabled` | `true` | Whether damper is on by default. `false` has the same effect as `damper disable`. |
| `priority` | `"utility"` | `"utility"` (QoS class), `"nice"` or `"none"`. |
| `extra_build_subcommands` | `[]` | Extra cargo subcommands or aliases to treat as builds. |

```toml
cores = "75%"
memory = "24GiB"
max_hold = "5m"
priority = "utility"
extra_build_subcommands = ["xtask"]
```

## Command line

| Command | What it shows or does |
|---|---|
| `damper status [--json]` | The current limit, the pressure inputs and the open sessions. |
| `damper log [--since 10m] [--session ID] [--json]` | The event log, oldest first. A long history is cut oldest-first, with a notice saying so; narrow it with `--since` or `--session`. |
| `damper explain ID [--json]` | What happened to one session (the id is in `damper status`). |
| `damper config check` | The effective configuration and any warnings. |
| `damper doctor` | Checks the install: the first `cargo` on `PATH` is the shim, the daemon answers, the run directory and socket are private, the config parses, the disabled flag. A set disabled flag is reported, not counted as a failure. |
| `damper disable` / `damper enable` | Set or remove the disabled flag. |

## Turning damper off

- One command: `DAMPER=off cargo build`.
- Everywhere: `damper disable` (and `damper enable` to undo). The shim checks
  the flag itself, so this works without the daemon running.

## Signals

Ctrl-C works as without damper. Signals your shell had ignored (`nohup`,
background jobs) stay ignored in the build. When the shim is in the terminal's
foreground process group, a SIGINT or SIGQUIT sent to the shim's pid alone is
not forwarded, because the terminal has already signalled cargo; other signals
(SIGTERM, SIGHUP) are forwarded.

## When the daemon is not there

damper fails open, silently:

- Daemon not running: the shim runs plain cargo at the configured priority.
- Daemon dies mid-build: the shim refills the jobserver FIFO itself, so the
  build carries on without limits.
- Daemon restarts: running builds keep running.
