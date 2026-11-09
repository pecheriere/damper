# damper: design

Date: 2026-11-08. Status: approved by the owner on 2026-11-08. Updated 2026-11-09 with the owner's rulings on the Phase 0 findings (`docs/superpowers/spikes/2026-11-09-phase0-findings.md`).

How to read the tags: `(owner's decision)` and `(owner's idea)` come from the owner. **[call]** is the author's decision, open to override. **[guess]** is an assumption the builder confirms, or a Phase 0 spike settles.

## 1. Why

The owner runs several coding agents at once, each in its own git worktree, and each one builds and tests Rust on its own. Every `cargo` assumes the whole Mac is free, so several of them together swamp it: memory pressure goes critical, macOS compresses and swaps, and the Mac crawls until processes are killed by hand. (owner's decision: this is the problem to solve)

The current mitigations protect the CPU but not memory:

- A `cargo` shim in the owner's dotfiles runs every cargo under `nice -n 15`.
- sccache, a compiler cache shared by every project.
- A written rule telling agents to run "at most about three building agents" and "check `uptime`". Agents must remember it, and the rule is static.

damper replaces the written rule with a machine-wide broker that hands out build slots and sizes the pool from what the Mac is actually going through, mostly memory. Agents need no instructions: every cargo goes through damper without knowing it.

**Success:** the owner never has to kill builds to get the Mac back.

## 2. Goals, non-goals, anti-goals

### Goals

- One machine-wide pool of build slots, shared by every cargo invocation from the owner and from agents. (owner's decision)
- The pool size adapts to live load, mainly memory pressure, inside a budget the user sets once. There are no per-worker or per-agent limits. (owner's decision)
- Easy to configure, with defaults that work on any supported Mac with no setup. A user can dedicate a share of memory and cores to builds. (owner's decision)
- When the Mac is in a bad enough state, damper may hold new builds back, and the hold is always visible: to the build's own output, to the CLI and API, and later to a notification. (owner's decision)
- An API and CLI that agents use to see status and logs and to understand what happened to a build. (owner's idea)
- A menu bar app showing slots in use, the limit and the reason for it, with settings and notifications. It's a later phase: the engine comes first. (owner's decision)
- Open source, installed from the owner's Homebrew tap. (owner's decision)
- Rust and cargo only for now, with the core kept tool-agnostic so other build tools can be added later. (owner's decision)

### Non-goals

- make, ninja and Xcode support in this design. make and ninja speak the same jobserver protocol and are cheap to add later. Xcode doesn't speak it and would be a separate project (§10). (owner's decision: Rust only for now)
- Managing anything other than builds and tests launched through cargo, such as rust-analyzer, Docker builds or CI.
- Remote or distributed builds.
- Changing where builds go or how they are cached: build dirs, `CARGO_*` env vars and sccache settings stay untouched. **[call]**, because the owner's sccache cache keys depend on env vars staying constant.

### Anti-goals: failure even if it technically works

- **damper makes a build fail, or blocks it with no way out.** (owner's decision) damper may slow a build or hold it back, but it never changes its outcome.
- **damper holds builds silently.** An agent that sees no output for minutes assumes a hang, times out and retries, and that makes things worse. (owner's decision: no silent holds)
- **damper is a required dependency.** If damper is broken, missing or misconfigured, cargo must behave exactly as if damper weren't installed.
- **damper adds noise.** No output in build logs, except while a build is being held.

## 3. Constraints

- macOS on Apple Silicon only. **[call]**: the QoS behaviour this relies on is Apple Silicon behaviour, and the owner's tooling (warden's formula) is arm64-only on macOS. Minimum macOS version: the oldest one that still gets security updates when v1 ships. **[call]**
- Core in Rust, following the owner's `rules/rust.md` in full, including the "Public repositories" section. **[call]**
- The menu bar app in Swift and SwiftUI (`MenuBarExtra`), signed and notarized with the owner's Apple Developer account. **[call]**: it's the native, smallest way to build a menu bar app with notifications, and the owner has the account.
- Public repository `pecheriere/damper`, following `rules/github-public-repo.md`. License: `MIT OR Apache-2.0`, as warden uses. **[call]**
- Distribution through `pecheriere/homebrew-tap` in the same way as warden: a signed release manifest, and a formula rendered by the tap's `bump` workflow. The app phase adds the tap's first cask, so the tap's `formula.py`, its CI and `AGENTS.md` need a matching cask path. Agents never sign releases, and only the owner makes releases and tags. (owner's rule)
- Security from the start:
  - The broker's socket and runtime dir are reachable only by the user: dir mode 0700, socket 0600, and every connection checked with `getpeereid` for the same uid.
  - No network listener.
  - Logs never contain environment variables or full command lines (§7.3).

## 4. How it fits together

```
 agent / user shell
        │  runs `cargo …`  (PATH resolves to damper's shim first)
        ▼
 ┌──────────────┐  Unix socket (session open, hold, close)  ┌──────────────┐
 │ damper shim  │──────────────────────────────────────────▶│   damperd    │
 │ (per build)  │◀──────────────────────────────────────────│ (launchd     │
 └──────┬───────┘   per-session FIFO path, hold status      │  user agent) │
        │ spawns at utility QoS,                            └──────┬───────┘
        │ MAKEFLAGS/CARGO_MAKEFLAGS=--jobserver-auth=fifo:<path>   │ feeds and drains
        ▼                                                          │ tokens; samples
 real cargo ──▶ rustc / sccache ──▶ tokens from the session FIFO ◀─┘ memory pressure
        ▲
 `damper` CLI, menu bar app ──── same socket, read-only plus admin commands
```

Components:

1. **`damperd`**: the broker, a launchd user agent started by `brew services start damper`. It owns the global limit, one FIFO per build session and the event log, and serves the API.
2. **The shim**: an executable named `cargo` in damper's shim dir, which the user puts first on `PATH`. It wraps every cargo invocation, including subcommands such as `cargo nextest` and `cargo clippy`.
3. **The `damper` CLI**: status, log, explain, enable and disable, config check (§7).
4. **The menu bar app**: Phase 2 (§8).

The shim finds the real cargo by searching `PATH` and skipping its own dir. That way, rustup's proxy and the toolchain it selects keep working. (owner's decision, 2026-11-09, from Phase 0)

- The shim runs the `cargo` it finds exactly as found, with `argv[0]` set to `cargo`. It never resolves that path to the file behind it, and never runs a toolchain's own cargo binary (`rustup which cargo`). rustup's proxy is a link to the `rustup` binary, which decides what to run from the name it was called by. Running the toolchain's cargo directly leaves rustc without a toolchain, and Phase 0 saw that break warden's build with `E0514`.
- A test covers this: a fake proxy that is a symlink named `cargo` on `PATH` must be the one run, with `argv[0]` set to `cargo`.

## 5. Build sessions and tokens

### 5.1 The protocol damper relies on

Cargo and rustc are clients of the GNU make jobserver protocol. If `CARGO_MAKEFLAGS` or `MAKEFLAGS` carries `--jobserver-auth=fifo:<path>`, cargo takes job tokens from that FIFO instead of creating its own pool, and gives them back when jobs finish. Each client may also run one job without a token: the "implicit slot". Phase 0 confirmed this on cargo 1.99.1: concurrency is always tokens + 1, cargo picks up new tokens within a second, and every token comes back when the build ends. Cargo honours `CARGO_MAKEFLAGS`, `MAKEFLAGS` and `MFLAGS` alike.

**With sccache as `RUSTC_WRAPPER`** (confirmed in Phase 0, recorded by the owner's decision, 2026-11-09):

- Session tokens limit how many rustc *processes* run. Cargo holds a token for each job while the sccache client waits, so this is the part that bounds memory.
- rustc's *internal* threads draw on the sccache server's own jobserver, sized to the Mac's core count and shared by every build. sccache strips the jobserver variables before it spawns rustc. damper doesn't limit thread parallelism in that case, and doesn't need to: sccache's pool is already machine-wide.
- Cache keys ignore `CARGO_MAKEFLAGS`, so a different FIFO path per session doesn't cost cache hits.

### 5.2 Why one FIFO per session, not one global FIFO

A single global FIFO has no owner tracking. Tokens held by a process that gets killed are lost for good, and the pool shrinks until nothing builds. Steve (Gentoo) fixes this with per-process tracking through Linux CUSE, which macOS lacks. damper gets the same recovery by giving each build session its own FIFO: the broker knows how many tokens it put into that FIFO, so when the session ends it takes all of them back, whatever happened to the processes. **[call]**

### 5.3 Session lifecycle

1. **Open.** The shim connects to `damperd` and opens a session, sending its working dir, the cargo subcommand name and its own pid.
   - `damperd` creates a FIFO in its runtime dir and returns the path.
   - The shim keeps this connection open for the whole build.
2. **First slot.** The shim waits until `damperd` grants the session's first slot. That slot counts against the global limit and stands for cargo's implicit slot, which is otherwise invisible to the pool. This is where holds happen (§6.3).
3. **Run.**
   - The shim spawns the real cargo at utility QoS, with `CARGO_MAKEFLAGS` and `MAKEFLAGS` pointing at the session FIFO, and tells `damperd` cargo's pid.
   - Arguments, stdin, stdout, stderr and signals pass straight through. Ctrl-C and SIGTERM reach cargo.
   - The shim exits with cargo's exit code, or reproduces cargo's death by signal.
4. **Feed.**
   - `damperd` adds tokens to the session FIFO while the global limit and the session's fair share allow.
   - It reads the FIFO's fill level (`FIONREAD`) to know how many tokens are outstanding: tokens given minus tokens sitting in the FIFO. `FIONREAD` must go through a separate `O_RDONLY | O_NONBLOCK` fd: on macOS it always reads 0 on an `O_RDWR` fd of a FIFO. Phase 0 confirmed that the read-only fd reads the right count, doesn't block and takes nothing from cargo. (owner's decision, 2026-11-09)
   - When the limit drops, it stops refilling and drains unused tokens back out.
   - Cargo holds on to tokens between jobs (cargo issue #17391), so tokens already taken come back only as cargo releases them. The policy must allow for that lag (§6.2).
5. **Close.** The session ends when the shim's connection closes *and* cargo's process is gone.
   - `damperd` watches cargo's pid with kqueue (`EVFILT_PROC`, `NOTE_EXIT`), so even a SIGKILLed shim with a cargo still running doesn't free the session's tokens too early.
   - On close, every token given to the session returns to the pool, and the FIFO is removed.

### 5.4 Which invocations get a session

- **Build subcommands open a session:** `build`, `check`, `clippy`, `test`, `nextest`, `bench`, `doc`, `run`, `install`, `rustc`, `rustdoc`, `fix`, plus any configured extras. **[call]**
- **Everything else passes straight through** with no session and no hold: `metadata`, `fmt`, `tree`, `--version`, `help`, `locate-project`, `update` and the like. **[call]** These don't compile, and blocking them would freeze tools that call them often, rust-analyzer among them.
- **A nested cargo joins its parent's session.**
  - This covers an `xtask` binary that calls `cargo` through `PATH`, and a build script running cargo.
  - The shim detects the case when the inherited `CARGO_MAKEFLAGS` points at a live damper session FIFO, or when a `DAMPER_SESSION` variable set by the outer shim is present. It then passes straight through: no new session, no hold, same FIFO.
  - Subcommands cargo starts through `$CARGO` skip the shim anyway and inherit the FIFO the same way.
  - A nested build is never held, so an outer session can't deadlock waiting on its own inner build. **[call]**
- **A session counts only while it compiles.** Cargo returns its tokens when compilation ends. For the floor (§5.5) and fairness, a session counts as active only while compiler or linker processes run in its process tree: rustc, the linker, or an sccache client waiting on a compile. With sccache, rustc itself runs under the sccache server, outside the tree (Phase 0), so the client stands for it. A long `cargo run` or a test run therefore doesn't keep a slot. **[call]** Tests are accounted for separately (§5.6).
- **rust-analyzer's `cargo check` gets a session like any build**, because it uses real memory. But it's never held, only slowed: the shim recognises rust-analyzer as the parent and skips the hold. **[guess]**: the builder confirms a reliable way to recognise it, such as an env var rust-analyzer sets or the parent process name. Without a reliable signal, it's treated as a normal build.

### 5.5 Floors

- A running session always keeps at least one job (its implicit slot). damper never cuts a running build to zero. (owner's decision: slow down, never fail)
- The global limit therefore counts against the sum of granted tokens plus one per running session. Under heavy pressure, the number of running jobs can exceed the limit by up to one per running session. That's accepted.

### 5.6 Tests

- **Test threads come from the session's share.** nextest runs test binaries itself and doesn't use the jobserver. When the shim sees `cargo nextest run` and the user hasn't set `NEXTEST_TEST_THREADS` or `--test-threads`, it sets `NEXTEST_TEST_THREADS` from the session's current share at start. Plain `cargo test` gets the same treatment through `RUST_TEST_THREADS`. **[call]**
- **Test runs count against the pool.**
  - While test processes run, the session counts as that many jobs. `damperd` sees them in the session's process tree.
  - Their memory counts in the session's measured footprint (§6.2), since test runs can be where the memory goes.
  - The thread count is fixed for that run, so the limit catches up on the next run, not mid-run. **[call]**
- **A user-set value always wins.**
  - The thread count never drops below 1.
  - The shim doesn't touch the variables when a user has set them.
  - Fewer threads means less concurrency, not slower individual tests. The owner's nextest time limits are per test, so this doesn't change outcomes. A test suite that needs a minimum concurrency sets its own threads, and that wins. **[call]**: this is accepted as not changing a build's outcome.

## 6. Sizing the pool

### 6.1 Budget (user config)

- `cores`: the most build jobs at once. Default: all cores. **[call]**
- `memory`: the most memory builds may use. Default: 75% of physical RAM. **[guess]** The builder tunes it in the acceptance test (§11).

These are ceilings. The limit inside them is always computed from live state. (owner's decision: no hardcoded per-worker limits)

### 6.2 Inputs and policy

Inputs, sampled every second or event-driven where macOS offers it. **[call]**

- The system memory-pressure level: `kern.memorystatus_vm_pressure_level` (normal, warning, critical), polled every second, plus a dispatch memory-pressure source if it fires in a user agent. Phase 0 confirmed both are readable without privileges, but couldn't make events fire (simulating them needs root), so the polled sysctl is the input the policy relies on. **[call]**
- `kern.memorystatus_level`, the percentage of memory available.
- The trend in swap use (`vm.swapusage`) and in the compressor's size.
- The physical footprint of the build processes (`proc_pid_rusage`, `ri_phys_footprint`), which works on the user's own processes without privileges. Build processes are every session's process tree *plus* the children of the user's sccache server, because with sccache rustc runs under the server, not under cargo (Phase 0). The policy needs only the total, not a figure per session. **[call]**
- CPU load is a secondary input only. Utility QoS already makes builds give way to interactive work on CPU.

**Why not an average per job.** Phase 0 measured warden's release build: the median job peaks at 41 MB, the p90 at 120 MB, and one job (the final binary's crate) at 1.3 GB. An average per job either starves the many small jobs or lets the big one through with no room. So the pool is sized by a closed loop on the measured total instead. (owner's decision, 2026-11-09)

Policy requirements. The exact constants are left to the builder.

- **The loop.** Compare the measured total footprint of all sessions against the memory budget (§6.1), and against live pressure and swap.
- **Grow ahead of the memory.** A job's memory grows after it gets its token, and cargo keeps tokens before giving them back (§5.3), so a loop that only reacts would act too late. damper adds a token only when the memory left in the budget still covers the largest job footprint seen recently. That reserve is a peak that fades over a few minutes, so one big link step doesn't shrink the pool for the rest of the day. (owner's decision, 2026-11-09)
- **Grow slowly, shrink fast.** Add at most one token per interval. Cut at once when pressure reaches warning or swap is growing. Never raise the limit while swap is growing.
- The limit never exceeds `cores`.
- Staying out of critical memory pressure beats throughput. When in doubt, the policy picks the smaller limit. (owner's decision)
- Share the limit fairly across sessions, so one big workspace build doesn't take every token from later builds.
- The limit only changes with a reason, which is logged (§7.3) and shown in status.

### 6.3 Holds (critical state)

When pressure is critical, or would become critical by the policy's estimate, new sessions wait at the first-slot step (§5.3). Running sessions drop to their floor: `damperd` stops refilling and drains their FIFOs. (owner's decision: holding is acceptable when the Mac is in a bad state, as long as it is visible)

A hold is never silent:

- While waiting, the shim prints to its stderr, at the start and then every 30 seconds **[call]**, a line such as: `damper: holding this build, memory pressure critical (waited 40s). Run 'damper status' for details.`
- Holds appear in `damper status` and the event log, and in Phase 2 as a notification.
- A hold lasts at most 10 minutes **[guess]**, configurable. After that the build runs with one job, and the shim says so on stderr. A hold delays a build, it never fails it.

## 7. Fail-open, API and logs

### 7.1 Fail-open rules (all required)

| Situation | Behaviour |
|---|---|
| `damperd` not running or socket unreachable | The shim gives up connecting after a short timeout **[guess: about 200 ms]** and runs cargo with no jobserver, still at lowered priority (§7.5), with no output. The event log records it once `damperd` is back, if it can. |
| `damperd` dies or hangs mid-build | The shim notices (connection closed, or no reply to heartbeats) and refills the session FIFO itself so cargo finishes at full speed. Even without that, cargo keeps its implicit slot, so a build never deadlocks. |
| Invalid config | Defaults are used, and a warning shows in `damper status` and the log. |
| Bypass for one command | `DAMPER=off cargo …` runs plain cargo: no session, no lowered priority. It's the real escape hatch. **[call]** |
| Bypass everywhere | `damper disable` and `damper enable`. The shim checks a flag file, so this works even when `damperd` is down. **[call]** |
| `brew upgrade` during builds | Running shims and sessions finish untouched. A new `damperd` must not orphan sessions: it either picks them up or lets their shims fall back to self-feeding. The builder picks the mechanism. |
| The shim's own bug or panic | The shim falls back to exec'ing the real cargo whenever it can. A damper error must never be what fails a build. |

### 7.2 API and CLI

- **Transport.** Line-delimited JSON over the Unix socket, with a protocol version in every message. **[call]** The shim, the CLI and the app use the same API.
- **`damper status [--json]`**: the current limit and its reason, the budget, the pressure inputs, active sessions (working dir, subcommand, tokens, age) and held sessions (waiting time).
- **`damper log [--json] [--since …] [--session …]`**: the event history.
- **`damper explain <session> [--json]`**: one build's story. It shows when it started, its hold time, its tokens over time and the pressure at each change. Session ids appear in the hold message and in `status`.
- **`damper config check`**: validates the config file and prints the effective values.
- **`damper doctor`**: checks the setup: that the shim comes first on `PATH`, that `damperd` is running, and the socket permissions. **[call]**: it's the first thing an agent or new user runs when something seems off.

### 7.3 Event log

- JSON Lines in damper's state dir, rotated with a size cap. **[call: 10 MB × 5 files]**
- Events: limit changes with their inputs, session open and close, holds and releases, tokens recovered at close, fallbacks, config reloads and errors.
- Recorded per session: working dir, cargo subcommand name and pids. Never environment variables, and never the full argument list. **[call]**: arguments can contain secrets, and the owner's rule is to never dump env.

### 7.4 Config

- TOML at `~/.config/damper/config.toml`. **[call]** Every key is optional.
- Keys: `cores`, `memory`, `max_hold`, and an `enabled` default. Values accept absolutes or percentages (`memory = "24GiB"` or `"75%"`).
- `damperd` reloads the file when it changes. The menu bar app writes the same file.
- `priority`: `"utility"` (QoS class, `taskpolicy -c utility` or its API equivalent), `"nice"` or `"none"`. **The default is `"utility"`.** (owner's decision, 2026-11-09) Phase 0 measured that `nice -n 15` keeps a probe's wake-up latency the same as plain cargo, while utility QoS keeps it at idle level, for builds about 25–35% longer. Background QoS was 5–6 times slower and isn't offered.

### 7.5 Priority doesn't depend on the broker

The shim lowers cargo's priority by itself, from the config's `priority` key, whether `damperd` is up or not and whether damper is disabled. Without that, a broker outage would remove the protection the owner's current `nice` shim gives today. Only `DAMPER=off` skips it. **[call]** In the anti-goal "cargo behaves as if damper weren't installed", that phrase covers outcomes and output, not scheduling priority.

## 8. Phase 2: menu bar app

- It shows slots in use against the current limit, held builds, and the reason for the limit, with a short history graph. Clicking a session shows what `damper explain` shows.
- It posts a notification when a hold starts, such as "damper: holding 3 builds, memory critical", and another when holds clear. **[call]**
- A settings pane edits the config file: budget, maximum hold, and launch at login. The app also offers to start `damperd` if it isn't running.
- It's read-mostly. Builds never depend on the app.
- It's signed and notarized, shipped as a cask in the tap, and uses the same socket API. The builder adds the cask path to the tap (§3).

## 9. Phases

0. **Spikes (throwaway, before any product code)**. Done on 2026-11-09, see `docs/superpowers/spikes/2026-11-09-phase0-findings.md`. Nothing refuted the design. The owner's rulings on the five changes it proposed are folded into §4, §5.1, §5.3, §6.2, §7.4 and §11. Still unverified: memory-pressure events firing in a user agent under real pressure, which the Phase 1 acceptance test covers.
   - Cargo 1.99 honours an inherited FIFO jobserver, and the job count follows the tokens fed in, live.
   - **sccache stays correct under it.** sccache runs rustc from its own long-lived server process. Confirm that rustc's parallelism still follows the session's tokens, and that cache hits are unchanged. If sccache breaks the scheme, stop and bring it back to the owner: the design changes.
   - The `FIONREAD` accounting on a macOS FIFO works as §5.3 assumes.
   - The memory-pressure signals in §6.2 are readable without special entitlements.
   - **QoS against `nice`**: measure, on the owner's Mac, interactive responsiveness and build time with utility QoS against `nice -n 15`. Pick the default from data. (owner's agreement to measure, not assume)
1. **Engine**: `damperd`, shim, sessions, sizing policy, holds, fail-open rules, CLI, event log, config, `brew services` formula, docs. Includes migrating the owner's dotfiles `bin/cargo` shim to damper's, and updating `rules/rust.md` to drop the manual three-agent rule. That migration lands in the dotfiles repo.
2. **Menu bar app** (§8).

## 10. Later, not in this design

- **make and ninja**: same protocol. A wrapper per tool plus the existing session machinery.
- **Xcode**: `xcodebuild` and the Swift build tools pick their parallelism once at launch (`-jobs` and related settings) and don't use the jobserver. Supporting them would mean damper chooses that number when the build starts, plus QoS. That's coarser, and kept out of the core's shape for now. (owner's idea, as a future direction)
- **Heavy jobs costing several slots** (cargo issue #8405) and memory-aware cargo (#12912): adopt them if cargo ever ships them.

## 11. Acceptance test

On the owner's Mac (M4, 10 cores, 32 GB), start 5 full workspace builds of a real project at once, such as warden, in 5 worktrees with cold build dirs, and sccache warm or cold, the same in both runs.

- **Without damper (the baseline):** the owner's current setup, meaning the dotfiles `nice -n 15` shim plus sccache. **[call]** Record memory pressure, swap and responsiveness. The expectation is critical pressure and a sluggish Mac.
- **With damper, hard gates:**
  - memory pressure never stays critical for more than a short spike;
  - the Mac stays responsive to interactive use;
  - all 5 builds pass.
- **With damper, not a gate:** record the total wall time in both runs. Builds will be slower: utility QoS alone cost about 25–35% in Phase 0, and damper may also hold builds back. That's expected and accepted, because responsiveness and staying out of critical pressure win over wall time. (owner's decision, 2026-11-09) Flag it for the owner if the batch takes more than twice the baseline. **[call]**
- **Fail-open checks:** kill `damperd` mid-run; SIGKILL a shim; SIGKILL a cargo; break the config. Every build still finishes or fails exactly as cargo would, and `damper log` explains each event.

## 12. Left to the builder

- The sizing formula and its constants, within §6.2's requirements.
- File locations for the runtime dir, the socket and the state dir. Mind macOS's 104-byte limit on Unix socket paths, and use user-owned dirs.
- The heartbeat and timeout values, and the upgrade handover mechanism (§7.1).
- The crate layout. Keep files under about 800 lines, per the owner's rules.
- How the shim is put first on `PATH`: a documented line for shell profiles, plus `damper doctor` to check it. Agents inherit it from the shell.
