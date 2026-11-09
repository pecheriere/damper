# AGENTS.md

Instructions for coding agents and humans working on damper.

damper is a macOS (Apple Silicon) broker that sizes a machine-wide cargo
jobserver pool from memory pressure. A shim named `cargo` joins the pool; the
daemon grows and shrinks it. damper never changes a build's outcome: when in
doubt, the shim execs the real cargo.

The design is in `docs/superpowers/specs/2026-11-08-damper-design.md`.

## Layout

| Path | Package | What it is |
|---|---|---|
| `crates/core` | `damper-core` | Policy, accounting and memory-pressure logic (lib) |
| `crates/proto` | `damper-proto` | Wire protocol between shim and daemon (lib) |
| `crates/daemon` | `damperd` | The broker daemon (bin + lib) |
| `crates/shim` | `damper-shim` | The cargo shim (bin `damper-shim`) |
| `crates/cli` | `damper` | The control CLI (bin) |
| `crates/e2e` | `damper-e2e` | End-to-end tests, never published |

## Ground rules

- **Do it right the first time.** No shortcuts, no `TODO` comments: do the work
  now or open an issue. No dead code.
- **No unsafe.** `unsafe_code = "forbid"` in every crate. Syscalls go through a
  safe wrapper or a system tool. If none exists, stop and ask.
- **No silenced lints.** No `#[allow(...)]` or `#[expect(...)]` without a
  `reason`. Clippy `all` and `pedantic` are denied, as are `dbg_macro`,
  `print_stdout`, `print_stderr`, `todo`, `unimplemented`, `unwrap_used` and
  `expect_used`. Binaries that print use `std::io::Write` on a locked handle.
- **Never log environment variables or full argument lists.** The shim sends the
  daemon only the classified subcommand name.
- **Keep files under about 800 lines.** Split by responsibility.
- Every dependency at its latest stable version, added with `cargo add`.
  `Cargo.lock` is committed and CI uses `--locked`.

## Building and testing

Never set `CARGO_TARGET_DIR` or any `CARGO_*` variable: the global `build-dir`
puts build files on a separate disk, and only final binaries land in `./target`.

Iterate on one crate with `cargo nextest run -p <crate>`. Run the full gate once
before each commit:

```sh
cargo fmt --check \
  && cargo clippy --workspace --all-targets --all-features --locked -- -D warnings \
  && cargo nextest run --workspace --locked \
  && RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked \
  && cargo deny check
```

Workflow changes must pass `uvx zizmor .`; the CI scripts' tests run with
`python3 -I -m unittest discover -s .github/scripts -p 'test_*.py'`.

## This is a public repository

Never commit private infrastructure details: real hostnames, addresses, user
names or account names. Examples and tests use generic names only.

## Commits and pull requests

- Commits are signed and signed off (`git commit -s`). Never bypass signing or
  hooks.
- No AI attribution lines in commits or pull request descriptions.
- One branch and one pull request per issue. Say in the pull request what was
  verified locally and what only CI ran.
- Conventional subjects (`feat:`, `fix:`, `chore:`, `docs:`).
