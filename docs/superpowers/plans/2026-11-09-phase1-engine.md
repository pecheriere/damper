# damper Phase 1 (engine) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build damper's engine: the `damperd` broker, the `cargo` shim and the `damper` CLI, built from source and run by hand, passing the design's §11 acceptance test on the owner's Mac.

**Architecture:** A pure, tool-agnostic core crate holds all decisions: config, subcommand classification, the sizing policy, fair allocation, process accounting and a sans-IO broker state machine that turns inputs into actions. `damperd` is a thin tokio shell: it samples macOS (sysctl, libproc, kqueue), owns the session FIFOs and the socket, feeds inputs to the core broker and carries out the actions it returns. The shim is a small synchronous binary that fails open: any error before cargo starts means it execs the real cargo.

**Tech Stack:** Rust 1.99.1, edition 2024, tokio, serde/serde_json, toml, clap, thiserror/anyhow, nix (mkfifo, kqueue, signals, user lookup), rustix (`ioctl_fionread`), libproc (process table, rusage), sysctl, signal-hook, proptest, insta, cargo-nextest.

**Spec:** `docs/superpowers/specs/2026-11-08-damper-design.md`. Phase 0 evidence: `docs/superpowers/spikes/2026-11-09-phase0-findings.md`. Read both before any task.

**Not in this plan (the next plan, "Phase 1b: distribution"):** release workflow and signed manifest, the tap formula with `brew services`, the dotfiles `bin/cargo` migration, the `rules/rust.md` update, and pinning the minimum macOS version (spec §3) in the release build. They depend on this plan's acceptance test passing. Track them as one GitHub issue when the repo is created.

## Global Constraints

- macOS on Apple Silicon only. Every crate sets nothing for other platforms. CI runs on GitHub's arm64 macOS runner (the newest GA `macos-*` label at implementation time).
- `unsafe_code = "forbid"` in every crate, no exceptions. Every syscall goes through a safe wrapper (nix, rustix, libproc, sysctl, signal-hook) or a system tool (`/usr/sbin/taskpolicy`, `/usr/bin/nice`). If a needed call has no safe wrapper, stop and report it: don't add `unsafe`.
- Workspace lints: warden's (`clippy::all` and `clippy::pedantic` at `deny`), plus `rules/rust.md`'s restriction lints (`dbg_macro`, `print_stdout`, `print_stderr`, `todo`, `unimplemented`, `unwrap_used`, `expect_used` all `deny`). Binaries that must print use `std::io::Write` on a locked handle, not `println!`. Tests may use `#[expect(clippy::unwrap_used, reason = "test")]` at module level.
- Every dependency at its latest stable version at implementation time (`cargo add` picks it). `Cargo.lock` committed; `--locked` in CI.
- Files under about 800 lines. Split by responsibility before that.
- Builds: never set `CARGO_TARGET_DIR` or any `CARGO_*` variable. The global `build-dir` puts build files on the external SSD. Iterate with `cargo nextest run -p <crate>`; run the full gate once before each commit.
- The full gate: `cargo fmt --check && cargo clippy --workspace --all-targets --all-features --locked -- -D warnings && cargo nextest run --workspace --locked && RUSTDOCFLAGS=-D\ warnings cargo doc --workspace --no-deps --locked && cargo deny check`.
- Never print or log environment variables or full argument lists (spec §7.3). The shim sends the daemon only the classified subcommand name.
- Commits: signed, signed off (`git commit -s`), no AI attribution of any kind. Public repository.
- damper never changes a build's outcome (spec §2 anti-goals). When in doubt in the shim, exec the real cargo.

### Fixed names and values

| Item | Value |
|---|---|
| Crates | `crates/core` (`damper-core`, lib), `crates/proto` (`damper-proto`, lib), `crates/daemon` (`damperd`, bin + lib), `crates/shim` (`damper-shim`, bin), `crates/cli` (`damper`, bin), `crates/e2e` (tests only, `publish = false`) |
| State dir | `<pw_dir>/.local/state/damper`, with `pw_dir` from `nix::unistd::User::from_uid(getuid())`, never `$HOME`. Overridable only by `DAMPER_STATE_DIR` (tests and development). |
| Runtime dir | `<state>/run`, mode 0700. Socket `<state>/run/damperd.sock`, mode 0600. FIFOs `<state>/run/s-<shim_pid>-<session_id>.fifo`, mode 0600. Lock `<state>/run/damperd.lock`. |
| Log dir | `<state>/log`: `events.jsonl`, rotated at 10 MiB, keeping `events.jsonl.1` … `.4`. Shim fallbacks: `<state>/log/fallbacks.jsonl`. |
| Disable flag | `<state>/disabled` |
| Config | `<pw_dir>/.config/damper/config.toml` |
| Socket path limit | 103 bytes. Longer: daemon refuses to start with a clear error; shim falls back. |
| Whitespace | A state dir path containing whitespace can't go in `MAKEFLAGS`. Daemon refuses to start with a clear error; shim falls back. |
| Token byte | `b'+'` |
| Env set by shim | `CARGO_MAKEFLAGS` and `MAKEFLAGS` = `-j --jobserver-auth=fifo:<path>`, `DAMPER_SESSION=<session id>`, and `NEXTEST_TEST_THREADS` / `RUST_TEST_THREADS` per §5.6. Nothing else. |
| Bypass | `DAMPER=off` |

### Policy constants **[call]**, tuned in Task 15

| Constant | Value |
|---|---|
| `TICK` | 1 s (sampling, policy, feeding) |
| `GROW_INTERVAL` | 2 s: at most one slot added per interval |
| `SWAP_WINDOW`, `SWAP_GROWTH` | swap is "growing" if it rose by at least 64 MiB over the last 10 s |
| `RESERVE_HALF_LIFE` | 180 s |
| `INITIAL_RESERVE` | `min(2 GiB, budget / 4)` |
| `SAFETY_MARGIN` | 1 GiB kept free of the system's available memory |
| `IDLE_DECAY` | if used slots + 1 < limit for 10 s, limit drops to used + 1 |
| `HEARTBEAT`, `HEARTBEAT_TIMEOUT` | daemon sends every 2 s; shim treats 10 s of silence as a dead daemon |
| `CONNECT_TIMEOUT` | 200 ms from connect to `Opened` |
| `HOLD_NOTICE` | 30 s between hold lines |
| `MAX_HOLD` default | 10 min (`max_hold` in config) |
| `MAX_LINE` | 64 KiB per protocol line |
| `MAX_SESSIONS` | 256 open sessions; beyond that `Error`, and the shim falls back |

## Review Focus

1. **Whitespace or a long path in the state dir.** `MAKEFLAGS` is split on spaces, and Unix socket paths are capped at 104 bytes. Expected: the daemon refuses to start with an explicit error, and cargo still works through the shim, silently. Tests in Task 8 and Task 11.
2. **Cargo options before the subcommand.** `cargo +nightly --locked -C dir --config k=v build`, built-in aliases (`b`, `t`, `c`, `r`, `d`) and user aliases. Expected: the subcommand is still found, built-in aliases map to their command, and user aliases pass through unless listed in `extra_build_subcommands`. Tests in Task 3.
3. **Ctrl-C or SIGTERM while held or while cargo runs.** Expected: the shim exits exactly as cargo would (130 for SIGINT before cargo starts, cargo's own status or signal after), and the daemon closes the session and takes all its tokens back. Tests in Task 12 and Task 14.
4. **The daemon restarts while builds run (`brew upgrade`, crash).** Expected: running builds finish at full speed through the shim's self-feed, and the new daemon never deletes a FIFO whose shim is alive. Tests in Task 10 and Task 14.
5. **A hostile or broken client.** Another uid, a line over 64 KiB, a wrong protocol version, garbage JSON, 300 sessions at once. Expected: that connection is refused or closed, nothing else is affected, and the event log records it. Tests in Task 10.

---

### Task 1: Repository scaffold and public baseline

**Files:**
- Create: `Cargo.toml` (workspace), `rust-toolchain.toml`, `.config/nextest.toml`, `deny.toml`, `.gitignore`, `LICENSE-MIT`, `LICENSE-APACHE`, `README.md`, `AGENTS.md`, `CLAUDE.md`, `SECURITY.md`, `CONTRIBUTING.md`
- Create: `crates/{core,proto,daemon,shim,cli,e2e}/Cargo.toml` and minimal `src/lib.rs` or `src/main.rs` that compile
- Create: `.github/dependabot.yml`, `.github/pull_request_template.md`, `.github/workflows/{ci,zizmor,scorecard,advisories,dependency-review}.yml`, `.github/scripts/crate_age.py`, `.github/scripts/test_crate_age.py`, `.github/crate-age-allow.toml`, `.github/requirements-lint.txt`, `.github/ruff.toml`

**Interfaces:**
- Produces: the workspace every later task adds to. Package names as in the Fixed names table. `damper-shim`'s binary is named `damper-shim`; installing it as `cargo` is the formula's job (Phase 1b) and the developer's in `docs/usage.md` (Task 15).

- [ ] **Step 1: Copy the reference files from warden (`~/Code/warden`)**, adapting names: `rust-toolchain.toml` (channel `1.99.1`), `deny.toml`, `.github/dependabot.yml` (cargo, github-actions, pip for `/.github`; `cooldown: default-days: 7`; minor and patch grouped), the crate-age files, `zizmor.yml`, `scorecard.yml`, `advisories.yml`, `dependency-review.yml`, `SECURITY.md` (private advisories at `/security/advisories/new`), the PR template. Drop everything warden-specific (OpenSSH, OpenSSL, Docker, fuzz, musl).
- [ ] **Step 2: Write `ci.yml`** with jobs on the arm64 macOS runner: `lint` (fmt, clippy `-D warnings`, `cargo doc` with `RUSTDOCFLAGS=-D warnings`, `cargo deny check`), `test` (`cargo nextest run --workspace --locked`, `timeout-minutes: 15` until measured in Task 15), `crate-age` (Ubuntu, as in warden). `permissions: contents: read`, `persist-credentials: false`, actions pinned by full SHA of a release at least 7 days old, `Swatinem/rust-cache` in `lint` and `test`.
- [ ] **Step 3: Write the workspace `Cargo.toml`**: `resolver = "3"`, `[workspace.package]` with `version = "0.1.0"`, `edition = "2024"`, `rust-version = "1.99"`, `license = "MIT OR Apache-2.0"`, `repository = "https://github.com/pecheriere/damper"`; the lints from Global Constraints; every member has `[lints] workspace = true`.
- [ ] **Step 4: Write `.config/nextest.toml`** with `slow-timeout = { period = "10s", terminate-after = 3 }` and a comment that Task 15 measures and tunes it.
- [ ] **Step 5: Write `AGENTS.md`** (what damper is in two lines, layout table, the full gate, the build-dir rule, "no unsafe", "never log env or args", commit rules) and `CLAUDE.md` containing `@AGENTS.md`. `README.md`: what it is, status "pre-release, Phase 1", link to the spec.
- [ ] **Step 6: Run the full gate and `uvx zizmor .`**
  Expected: all pass, zizmor reports no findings, `python3 -I -m unittest discover -s .github/scripts -p 'test_*.py'` passes.
- [ ] **Step 7: Commit** `chore: scaffold the workspace and public repository baseline`

---

### Task 2: Config (core)

**Files:**
- Create: `crates/core/src/config.rs`, `crates/core/src/units.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum Priority { Utility, Nice, None }            // default Utility (spec §7.4)
  pub struct Machine { pub cores: u32, pub memory: u64 } // physical bytes
  pub struct Effective {
      pub cores: u32, pub memory: u64, pub max_hold: Duration,
      pub enabled: bool, pub priority: Priority, pub extra_build_subcommands: Vec<String>,
  }
  pub struct Loaded { pub effective: Effective, pub warnings: Vec<String> }
  pub fn load(text: Option<&str>, machine: Machine) -> Loaded;   // None = no file
  pub fn parse_bytes(s: &str) -> Result<u64, UnitError>;        // "24GiB", "512MiB", "1073741824"
  pub fn parse_duration(s: &str) -> Result<Duration, UnitError>; // "10m", "90s", "1h"
  ```
- `load` never fails: an invalid file, key or value gives that key's default and one warning naming the key. Unknown keys warn. Keys: `cores` (integer or `"NN%"`), `memory` (bytes string or `"NN%"`), `max_hold`, `enabled`, `priority`, `extra_build_subcommands`.

- [ ] **Step 1: Write the failing tests** in `config.rs`:
  - `defaults_with_no_file`: cores = machine cores, memory = 75% of machine memory (integer arithmetic, rounded down), max_hold 600 s, enabled, `Priority::Utility`, no warnings.
  - `percent_and_absolute_values`: `memory = "24GiB"` → 25_769_803_776; `memory = "50%"` on 32 GiB → 17_179_869_184; `cores = "50%"` on 10 → 5; `cores = 4` → 4.
  - `clamps_to_machine_with_warning`: `memory = "64GiB"` on 32 GiB → 32 GiB and one warning; `cores = 20` on 10 → 10 and one warning.
  - `rejects_zero_and_garbage_per_key`: `cores = 0` → default + warning; `memory = "lots"` → default + warning, while a valid `max_hold = "5m"` in the same file is kept.
  - `invalid_toml_gives_defaults_and_one_warning`.
  - `unknown_key_warns`: `colour = "red"` → defaults, warning mentions `colour`.
  - `priority_values`: `"utility"`, `"nice"`, `"none"`; `"fast"` → Utility + warning.
- [ ] **Step 2: Run** `cargo nextest run -p damper-core config` — Expected: FAIL (not defined).
- [ ] **Step 3: Implement** `units.rs` and `config.rs`. Parse into `toml::Table` and validate key by key, so one bad key never discards the others.
- [ ] **Step 4: Add a proptest** `parse_bytes_round_trips`: for any `n: u32` and unit in {B, KiB, MiB, GiB}, `parse_bytes(format!("{n}{unit}"))` equals `n × multiplier` (checked multiply, overflow → `Err`).
- [ ] **Step 5: Run** `cargo nextest run -p damper-core` — Expected: PASS.
- [ ] **Step 6: Commit** `feat(core): parse and resolve the config`

---

### Task 3: Subcommand classification (core)

**Files:**
- Create: `crates/core/src/classify.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum Kind { Build, Test, Nextest, Run, PassThrough }
  pub struct Invocation { pub kind: Kind, pub subcommand: Option<String>, pub user_threads: bool }
  pub fn classify(args: &[OsString], extra: &[String], env_threads: ThreadEnv) -> Invocation;
  pub struct ThreadEnv { pub nextest: bool, pub rust_test: bool } // is NEXTEST_TEST_THREADS / RUST_TEST_THREADS set
  impl Kind { pub fn from_subcommand(name: &str) -> Kind; } // daemon side: "test"/"bench" → Test, "nextest" → Nextest, "run" → Run, anything else → Build
  ```
- `args` excludes argv[0]. `subcommand` is the canonical name sent to the daemon (`"build"`, `"nextest"`, …) or the extra's own name; `None` for `PassThrough`.
- Build subcommands (spec §5.4): `build check clippy test nextest bench doc run install rustc rustdoc fix`, plus `extra`. `Test` = `test`, `bench`; `Nextest` = `nextest` (threads only for `nextest run`); `Run` = `run`; all others in the list = `Build`.
- Global options skipped before the subcommand: `+<toolchain>`, any `-…` flag, and the value of `--config`, `-C`, `--color`, `-Z` (both `--config v` and `--config=v` forms). `--version`, `-V`, `--help`, `-h`, `--list` with no subcommand → `PassThrough`.
- Built-in aliases: `b`→build, `c`→check, `t`→test, `r`→run, `d`→doc.
- `user_threads` is true when the user set the thread count: for `Nextest`, `env_threads.nextest` or `--test-threads`/`-j` in the args after `run`; for `Test`, `env_threads.rust_test` or `--test-threads` after a `--`.

- [ ] **Step 1: Write the failing tests** (a table test `classifies` with one row per case):
  `["build"]`→Build "build"; `["+nightly","--locked","-C","dir","--config","k=v","build","--release"]`→Build; `["--config=k=v","t"]`→Test "test"; `["nextest","run","-j","4"]`→Nextest, user_threads; `["nextest","list"]`→Nextest, not user_threads; `["test","--","--test-threads=2"]`→Test, user_threads; `["metadata"]`, `["fmt"]`, `["--version"]`, `[]`, `["xtask"]`→PassThrough; `["xtask"]` with extra `["xtask"]`→Build "xtask"; `["run","--","--test-threads"]`→Run, not user_threads; non-UTF-8 arg before the subcommand → still finds it; `["-Z","unstable-options","check"]`→Build "check". Plus `from_subcommand_maps_names`.
- [ ] **Step 2: Run** `cargo nextest run -p damper-core classify` — Expected: FAIL.
- [ ] **Step 3: Implement** `classify`.
- [ ] **Step 4: Run** — Expected: PASS. Then **commit** `feat(core): classify cargo invocations`

---

### Task 4: Protocol, events and blocking client (proto)

**Files:**
- Create: `crates/proto/src/{lib.rs,msg.rs,event.rs,status.rs,framing.rs,client.rs}`

**Interfaces:**
- Produces (all `Serialize + Deserialize + Debug + Clone + PartialEq`):
  ```rust
  pub const PROTOCOL_VERSION: u32 = 1;
  pub struct SessionId(pub String);            // daemon-assigned, "<daemon start unix secs, hex>-<counter>"
  #[serde(tag = "type", rename_all = "snake_case")]
  pub enum ClientMsg {
      Open { cwd: PathBuf, subcommand: String, shim_pid: u32, parent_pid: u32, nested: bool },
      Started { cargo_pid: u32 },
      Status, Log { since_ms: Option<u64>, session: Option<SessionId> }, Explain { session: SessionId },
  }
  pub enum ServerMsg {
      Opened { session: SessionId, fifo: PathBuf },
      Held { reason: HoldReason },
      Granted { threads: u32, after_timeout: bool },
      Heartbeat, Status(Status), Events(Vec<Event>), Error { message: String },
  }
  pub enum HoldReason { PressureCritical, MemoryNearlyFull }   // Display: "memory pressure critical", "memory nearly full"
  pub struct Envelope<T> { pub v: u32, #[serde(flatten)] pub msg: T }
  pub struct Event { pub ts_ms: u64, #[serde(flatten)] pub kind: EventKind }
  pub enum EventKind {
      LimitChanged { from: u32, to: u32, reason: LimitReason, inputs: InputsSummary },
      SessionOpened { session: SessionId, cwd: PathBuf, subcommand: String, shim_pid: u32, never_hold: bool },
      SessionStarted { session: SessionId, cargo_pid: u32 },
      SessionTokens { session: SessionId, tokens: u32 },
      HoldStarted { session: SessionId, reason: HoldReason },
      HoldReleased { session: SessionId, waited_ms: u64, timed_out: bool },
      SessionClosed { session: SessionId, tokens_recovered: u32, duration_ms: u64 },
      ShimFallback { reason: String, cwd: PathBuf },
      ConfigLoaded { warnings: Vec<String> },
      ClientRejected { reason: String },
      Error { message: String },
  }
  pub enum LimitReason { Startup, Critical, Warning, SwapGrowing, Headroom, Growth, IdleDecay, Config }
  pub struct InputsSummary { pub pressure: Pressure, pub available_pct: u8, pub swap_used: u64,
      pub swap_growing: bool, pub build_footprint: u64, pub reserve: u64 }
  pub enum Pressure { Normal, Warning, Critical }
  pub struct Status { pub limit: u32, pub reason: LimitReason, pub cores: u32, pub memory: u64,
      pub inputs: InputsSummary, pub holding: bool, pub sessions: Vec<SessionStatus>, pub warnings: Vec<String> }
  pub struct SessionStatus { pub session: SessionId, pub cwd: PathBuf, pub subcommand: String,
      pub state: SessionState, pub slots: u32, pub tokens_outstanding: u32, pub age_secs: u64 }
  pub enum SessionState { Held { waited_secs: u64 }, Compiling, Idle }
  // framing.rs
  pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError>;   // Envelope + '\n'
  pub fn decode<T: DeserializeOwned>(line: &[u8]) -> Result<T, ProtoError>; // checks v, MAX_LINE
  pub const MAX_LINE: usize = 64 * 1024;
  // client.rs (blocking, std only, used by shim and CLI)
  pub struct Client { /* UnixStream + BufReader */ }
  impl Client {
      pub fn connect(socket: &Path, timeout: Duration) -> Result<Client, ProtoError>;
      pub fn send(&mut self, msg: &ClientMsg) -> Result<(), ProtoError>;
      pub fn recv(&mut self, timeout: Option<Duration>) -> Result<ServerMsg, ProtoError>;
  }
  pub fn explain_events(all: &[Event], session: &SessionId) -> Vec<Event>; // the session's events plus LimitChanged between its open and close
  ```
- `ProtoError` (thiserror, `#[non_exhaustive]`): `Io`, `Json`, `Version { got: u32 }`, `LineTooLong`, `Timeout`, `Closed`.

- [ ] **Step 1: Write the failing tests**: `round_trips_every_message` (each variant encodes and decodes to itself); `snapshot_wire_format` (insta snapshot of one `Open` and one `Status`, so wire changes are deliberate); `rejects_other_version` (`{"v":2,"type":"status"}` → `Version { got: 2 }`); `rejects_long_line`; `explain_events_picks_session_and_limit_changes`; `client_times_out_on_silent_server` (a test `UnixListener` that accepts and never writes; `recv(Some(50ms))` → `Timeout`).
- [ ] **Step 2: Run** `cargo nextest run -p damper-proto` — Expected: FAIL.
- [ ] **Step 3: Implement.** `HoldReason` and `LimitReason` implement `Display` with the exact strings above (hold) and lowercase words (limit).
- [ ] **Step 4: Run** — Expected: PASS. **Commit** `feat(proto): define the wire protocol and event log types`

---

### Task 5: Sizing policy (core)

**Files:**
- Create: `crates/core/src/policy.rs`, `crates/core/src/peak.rs`

**Interfaces:**
- Consumes: `Effective` (Task 2), `Pressure`, `LimitReason`, `HoldReason`, `InputsSummary` (Task 4; core depends on proto for these types).
- Produces:
  ```rust
  pub struct DecayingPeak { /* value, last update */ }
  impl DecayingPeak {
      pub fn new(initial: u64, half_life: Duration, now: Instant) -> Self;
      pub fn observe(&mut self, value: u64, now: Instant);   // value = max(decayed, value)
      pub fn get(&self, now: Instant) -> u64;                  // exponential decay since last observe
  }
  pub struct PolicyInputs { pub pressure: Pressure, pub available: u64, pub available_pct: u8,
      pub swap_used: u64, pub build_footprint: u64, pub largest_job: u64,
      pub used_slots: u32, pub hungry: bool }
  pub struct Decision { pub limit: u32, pub hold: Option<HoldReason>, pub changed: Option<LimitReason>,
      pub summary: InputsSummary }
  pub struct Policy { /* config, limit, reserve: DecayingPeak, swap samples, last growth, idle since */ }
  impl Policy {
      pub fn new(cfg: &Effective, now: Instant) -> Self;          // limit 1, reason Startup
      pub fn reconfigure(&mut self, cfg: &Effective);              // clamps limit to new cores
      pub fn tick(&mut self, now: Instant, inp: &PolicyInputs) -> Decision;
  }
  ```
- `limit` counts slots, implicit ones included. The broker gives every running session at least one slot whatever the limit (spec §5.5).

The algorithm, which the tests pin (constants from the table above):

```
reserve.observe(inp.largest_job)
swap_growing = swap rose ≥ SWAP_GROWTH within SWAP_WINDOW
headroom = min(budget − build_footprint, available − SAFETY_MARGIN)   (saturating)
match pressure:
  Critical            → limit = 0,           reason Critical,     hold PressureCritical
  Warning | swap_growing → limit = limit / 2, reason Warning or SwapGrowing,
                        hold MemoryNearlyFull if headroom < reserve
  Normal:
    headroom < reserve → limit = limit − 1 (min 1), reason Headroom
    hungry ∧ now − last_growth ≥ GROW_INTERVAL ∧ limit < cores ∧ headroom ≥ reserve
                       → limit + 1, reason Growth
    used_slots + 1 < limit for IDLE_DECAY → limit = used_slots + 1, reason IdleDecay
limit = min(limit, cores); changed = Some(reason) only if the value changed
```

- [ ] **Step 1: Write the failing tests** with a fixed `Instant` base and a 32 GiB, 10-core `Effective`:
  - `peak_decays_by_half_per_half_life`; `peak_takes_new_max_immediately`.
  - `grows_one_slot_per_interval_while_hungry`: normal pressure, large headroom, hungry: limits after ticks at 0, 1, 2, 3, 4 s are 1, 2, 2, 3, 3 … and never exceed 10.
  - `does_not_grow_when_not_hungry`.
  - `does_not_grow_without_room_for_largest_job`: headroom 1.5 GiB, largest job seen 2 GiB → no growth, then shrinks by 1 (Headroom).
  - `critical_drops_to_zero_and_holds`.
  - `warning_halves_and_holds_only_without_headroom`.
  - `swap_growth_halves_and_blocks_growth`: swap +100 MiB over 5 s → halves, reason SwapGrowing, no growth on the next ticks while it keeps rising.
  - `idle_limit_decays_to_usage`: limit 8, used 2 for 10 s → limit 3, reason IdleDecay. (This stops an idle Mac from handing a burst of new builds 10 slots at once.)
  - `reserve_fades_after_big_link`: largest job 1.3 GiB once, then 40 MiB jobs: after 6 min the reserve is under 400 MiB and growth resumes with 1 GiB headroom.
  - `reconfigure_clamps_limit`.
  - `changed_is_none_when_value_unchanged`.
- [ ] **Step 2: Run** `cargo nextest run -p damper-core policy` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS. **Commit** `feat(core): size the pool from measured footprint and pressure`

---

### Task 6: Allocation and process accounting (core)

**Files:**
- Create: `crates/core/src/allocate.rs`, `crates/core/src/account.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Demand { pub hungry: bool, pub outstanding: u32 }  // tokens cargo holds now
  pub fn allocate(limit: u32, demands: &[Demand]) -> Vec<u32>;  // slots per session, same order; caller passes oldest first

  pub struct ProcInfo { pub pid: u32, pub ppid: u32, pub name: String, pub footprint: u64, pub peak: u64 }
  pub struct Tracked { pub root_pid: u32, pub kind: Kind }        // Kind from Task 3; root = cargo pid
  pub struct SessionUsage { pub compiling: bool, pub test_jobs: u32 }
  pub struct Accounting { pub total_footprint: u64, pub largest_job: u64, pub sessions: Vec<SessionUsage> }
  pub fn account(procs: &[ProcInfo], tracked: &[Tracked], uid_sccache_servers: &[u32]) -> Accounting;
  pub fn is_compile_process(name: &str) -> bool;
  ```
- `allocate`: every session gets 1; the rest of `limit` is shared max-min fairly. A non-hungry session's demand is `outstanding + 1`; a hungry one's is unbounded. Ties go to the earlier session. If `limit` < number of sessions, every session still gets 1.
- `account`:
  - Compile processes: `rustc`, `rustdoc`, `clippy-driver`, `sccache`, `ld`, `ld64`, `cc`, `c++`, `clang`, `clang++`, `gcc`, `g++`, and names starting with `build-script-`.
  - A session is `compiling` if its tree (root and all descendants) holds a compile process.
  - `test_jobs` counts, for `Test` and `Nextest` sessions only, descendants that are neither compile processes nor named `cargo` or `cargo-*`.
  - `total_footprint` = footprint of every process in every tracked tree (for `Run` sessions, only compile processes), plus all descendants of each sccache server pid. A pid is counted once.
  - `largest_job` = the largest `peak` among compile processes counted.
  - An sccache server is a process named `sccache` whose `ppid` is 1. The daemon passes those pids in.

- [ ] **Step 1: Write the failing tests**:
  - `allocate_gives_everyone_one`: limit 2, 4 sessions → `[1,1,1,1]`.
  - `allocate_splits_fairly_among_hungry`: limit 10, 3 hungry → `[4,3,3]`.
  - `allocate_caps_satisfied_sessions`: limit 10, `[satisfied outstanding 1, hungry, hungry]` → `[2,4,4]`.
  - proptest `allocate_invariants`: for any limit 0..64 and 0..16 demands: length matches; each ≥ 1; sum ≤ max(limit, n); no non-hungry session above its demand unless every session is at its cap; no two hungry sessions differ by more than 1.
  - `account_counts_sccache_children`: cargo(100)→sccache client(101); sccache server(50, ppid 1)→rustc(51, 2 GiB, peak 2.5 GiB) → total includes 51, `largest_job` 2.5 GiB, session compiling.
  - `account_run_session_ignores_app_memory`: `Run` tree with `myserver` at 3 GiB, no compile process → not compiling, footprint 0 from it.
  - `account_counts_test_jobs`: `Nextest` tree: cargo → cargo-nextest → 3 test binaries → `test_jobs` 3.
  - `account_counts_each_pid_once` (a rustc in a session tree that is also under the sccache server).
- [ ] **Step 2: Run** `cargo nextest run -p damper-core allocate account` — Expected: FAIL.
- [ ] **Step 3: Implement.** Build a ppid → children map once per call.
- [ ] **Step 4: Run** — Expected: PASS. **Commit** `feat(core): allocate slots fairly and account build memory`

---

### Task 7: Broker state machine (core, sans-IO)

**Files:**
- Create: `crates/core/src/broker/mod.rs` (types and public API), `crates/core/src/broker/session.rs` (per-session state), `crates/core/src/broker/tick.rs` (the tick), `crates/core/src/broker/tests.rs`

**Interfaces:**
- Consumes: `Policy` (5), `allocate`, `account`, `ProcInfo` (6), `classify::Kind` (3), proto types (4).
- Produces:
  ```rust
  pub struct OpenRequest { pub cwd: PathBuf, pub subcommand: String, pub kind: Kind,
      pub shim_pid: u32, pub never_hold: bool, pub nested: bool }
  pub struct TickInput { pub now: Instant, pub memory: MemorySample, pub procs: Vec<ProcInfo>,
      pub fifo_fill: HashMap<SessionId, u32>, pub sccache_servers: Vec<u32> }
  pub struct MemorySample { pub pressure: Pressure, pub available_pct: u8, pub physical: u64, pub swap_used: u64 }
  pub enum Action {
      CreateFifo { session: SessionId, shim_pid: u32 },
      Feed { session: SessionId, tokens: u32 },
      Drain { session: SessionId, tokens: u32 },
      RemoveFifo { session: SessionId },
      Send { session: SessionId, msg: ServerMsg },
      WatchPid { pid: u32 },
      Log(EventKind),
  }
  pub struct Broker { /* policy, sessions in open order, counters */ }
  impl Broker {
      pub fn new(cfg: Effective, warnings: Vec<String>, start_unix_secs: u64, now: Instant) -> Self;
      pub fn open(&mut self, now: Instant, req: OpenRequest) -> Result<(SessionId, Vec<Action>), OpenError>; // OpenError::TooManySessions
      pub fn fifo_ready(&mut self, session: &SessionId, path: PathBuf, now: Instant) -> Vec<Action>; // sends Opened, then Granted or Held
      pub fn started(&mut self, session: &SessionId, cargo_pid: u32) -> Vec<Action>;
      pub fn connection_closed(&mut self, session: &SessionId, now: Instant) -> Vec<Action>;
      pub fn pid_exited(&mut self, pid: u32, now: Instant) -> Vec<Action>;
      pub fn tick(&mut self, input: &TickInput) -> Vec<Action>;
      pub fn reconfigure(&mut self, cfg: Effective, warnings: Vec<String>) -> Vec<Action>;
      pub fn status(&self, now: Instant) -> Status;
  }
  ```

Rules the tests pin:

- **Accounting per session.** `given` = tokens fed − tokens drained. `outstanding` = `given − fifo_fill`. `hungry` = compiling and `fifo_fill == 0`. `used_slots` (for the policy) = sum over compiling sessions of `outstanding + 1`, plus `test_jobs`.
- **Grant.** A new session is granted at once unless the last decision holds, and the session is neither `nested` nor `never_hold`. `Granted.threads` = its fair share if it were hungry, computed with `allocate` over the current sessions plus itself, at least 1.
- **Hold.** A held session gets `Held{reason}` once, and its own `HoldStarted` event. It's granted when the hold clears (`HoldReleased{timed_out: false}`) or after `max_hold` (`Granted{after_timeout: true}`, `HoldReleased{timed_out: true}`).
- **Feeding.** Each tick, for compiling sessions, `target = slots − 1` from `allocate(limit − test_jobs, …)`. Idle and held sessions have target 0. Feed `target − given` when positive. Drain `min(fifo_fill, given − target)` when positive. A `SessionTokens` event is logged when a target changes.
- **Close.** A session closes when its connection is closed and, if `started`, its cargo pid has exited (`WatchPid` is emitted on `started`). Closing emits `RemoveFifo` and `SessionClosed{tokens_recovered: given}`.
- **Heartbeat.** Every second tick sends `Heartbeat` to every open session.
- **Limit changes** emit `LimitChanged` with the decision's reason and summary.

- [ ] **Step 1: Write the failing tests** in `broker/tests.rs`, each driving the broker with synthetic `TickInput`s:
  - `open_creates_fifo_then_grants`: `open` → `[CreateFifo, Log(SessionOpened)]`; `fifo_ready` → `Send Opened` then `Send Granted{threads ≥ 1}`.
  - `feeds_hungry_session_up_to_share`: limit grown to 4 with one compiling hungry session → feeds total 3 over the ticks, never more.
  - `drains_unused_tokens_when_limit_drops`: given 3, fifo holds 2, pressure critical → `Drain 2`, then nothing more until cargo returns the last.
  - `holds_new_session_in_critical_and_releases`: `Held{PressureCritical}`, then pressure normal → `Granted{after_timeout: false}` and `HoldReleased`.
  - `hold_times_out_after_max_hold`: still critical after 10 min → `Granted{threads: 1, after_timeout: true}`.
  - `nested_and_never_hold_are_never_held`.
  - `fair_share_between_two_sessions`: limit 10, two hungry → targets 4 and 4 (slots 5 and 5).
  - `idle_session_gets_no_tokens`: started, no compile process in tree → target 0.
  - `close_waits_for_cargo_exit`: connection closed while the pid lives → no `RemoveFifo`; `pid_exited` → `RemoveFifo` and `SessionClosed{tokens_recovered: given}`.
  - `close_without_start_is_immediate`.
  - `too_many_sessions_is_an_error`: the 257th `open` → `Err(TooManySessions)`.
  - `heartbeat_every_two_ticks`.
  - `status_reports_sessions_and_holds`.
- [ ] **Step 2: Run** `cargo nextest run -p damper-core broker` — Expected: FAIL.
- [ ] **Step 3: Implement.** Keep each file under 400 lines; the tests file may be longer.
- [ ] **Step 4: Run** — Expected: PASS. **Commit** `feat(core): add the sans-IO broker state machine`

---

### Task 8: Daemon platform layer: paths, FIFOs, pid watcher

**Files:**
- Create: `crates/proto/src/paths.rs` (shared with the shim and CLI, so it can't live in the daemon), `crates/daemon/src/lib.rs`, `crates/daemon/src/fifo.rs`, `crates/daemon/src/pidwatch.rs`

**Interfaces:**
- Produces:
  ```rust
  // damper_proto::paths
  pub struct Paths { pub state: PathBuf, pub run: PathBuf, pub socket: PathBuf, pub lock: PathBuf,
      pub log: PathBuf, pub disabled: PathBuf, pub config: PathBuf }
  pub fn resolve_paths() -> Result<Paths, PathError>;  // DAMPER_STATE_DIR or pw_dir; checks whitespace and socket length
  pub fn prepare_dirs(p: &Paths) -> Result<(), PathError>; // create 0700, refuse if owner ≠ uid or mode has group/other bits
  // fifo.rs
  pub struct SessionFifo { /* path, rw: OwnedFd (O_RDWR|O_NONBLOCK|O_CLOEXEC), ro: OwnedFd (O_RDONLY|O_NONBLOCK|O_CLOEXEC) */ }
  impl SessionFifo {
      pub fn create(run_dir: &Path, shim_pid: u32, session: &SessionId) -> io::Result<Self>; // mkfifo 0600
      pub fn path(&self) -> &Path;
      pub fn fill(&self) -> io::Result<u32>;            // rustix::io::ioctl_fionread on `ro`
      pub fn feed(&self, n: u32) -> io::Result<u32>;     // writes n × b'+' to `rw`, returns written
      pub fn drain(&self, n: u32) -> io::Result<u32>;    // reads up to n from `ro`, WouldBlock → 0
      pub fn remove(self) -> io::Result<()>;             // unlink, then drop fds
  }
  pub fn sweep_stale_fifos(run_dir: &Path) -> io::Result<Vec<PathBuf>>; // removes s-<pid>-*.fifo whose pid is dead (kill(pid, None) → ESRCH)
  pub fn shim_pid_of(path: &Path) -> Option<u32>;
  // pidwatch.rs
  pub struct PidWatcher { /* Arc<nix::sys::event::Kqueue> */ }
  impl PidWatcher {
      pub fn spawn(exits: tokio::sync::mpsc::Sender<u32>) -> io::Result<Self>; // dedicated std thread waiting on kevent
      pub fn watch(&self, pid: u32) -> io::Result<()>; // EVFILT_PROC/NOTE_EXIT; ESRCH → sends pid at once
  }
  ```

- [ ] **Step 1: Write the failing tests**:
  - `paths_refuse_whitespace` (a `DAMPER_STATE_DIR` with a space → `PathError::Whitespace`); `paths_refuse_long_socket` (a 120-byte state dir → `PathError::SocketTooLong`); `prepare_refuses_group_writable` (0770 dir → error); `prepare_creates_0700`.
  - `fifo_fill_counts_fed_tokens` (feed 5, drain 2, fill 3 — the Phase 0 S3 case); `fifo_fill_reads_zero_without_consuming`; `fifo_remove_unlinks`; `sweep_keeps_live_shim_fifo` (one FIFO named with the test's own pid, one with a dead pid from a spawned and reaped `true` process → only the dead one removed).
  - `pidwatch_reports_exit` (spawn `sleep 0.2`, watch, receive its pid within 2 s); `pidwatch_reports_already_dead_pid`.
- [ ] **Step 2: Run** `cargo nextest run -p damperd -p damper-proto` — Expected: FAIL.
- [ ] **Step 3: Implement.** Tests that change `DAMPER_STATE_DIR` must not set process env (nextest runs tests in separate processes, but don't rely on it): make `resolve_paths_from(state_override: Option<&Path>)` the tested function, and `resolve_paths()` a one-line wrapper.
- [ ] **Step 4: Run** — Expected: PASS. **Commit** `feat(daemon): add paths, session FIFOs and the pid watcher`

---

### Task 9: Daemon platform layer: memory and process sources

**Files:**
- Create: `crates/daemon/src/sources.rs` (traits), `crates/daemon/src/memory.rs`, `crates/daemon/src/procs.rs`

**Interfaces:**
- Produces:
  ```rust
  pub trait MemorySource: Send + Sync { fn sample(&self) -> io::Result<MemorySample>; }
  pub trait ProcessSource: Send + Sync { fn snapshot(&self) -> io::Result<Snapshot>; }
  pub struct Snapshot { pub procs: Vec<ProcInfo>, pub sccache_servers: Vec<u32> }
  pub struct SysctlMemory;   // kern.memorystatus_vm_pressure_level (1 normal, 2 warning, 4 critical), kern.memorystatus_level, hw.memsize, vm.swapusage (xsw_usage: total, avail, used — read `used`)
  pub struct LibprocProcesses { uid: u32 }  // pids_by_type(ByUID), pidinfo BSDInfo (ppid, name), pidrusage RUsageInfoV4 (ri_phys_footprint, ri_lifetime_max_phys_footprint)
  pub fn pressure_from_level(level: i32) -> Pressure; // 4 → Critical, 2 → Warning, anything else → Normal
  pub fn swap_used_from_xsw(bytes: &[u8]) -> io::Result<u64>;
  pub fn process_name(pid: u32) -> Option<String>; // used for rust-analyzer detection
  ```
- Processes that vanish between listing and reading are skipped, not errors. A process the user can't read is skipped.

- [ ] **Step 1: Write the failing tests**: `pressure_levels_map` (1, 2, 4, 0, 99); `swap_used_parses_xsw_usage` (a hand-built 32-byte buffer); `sysctl_memory_reads_live_values` (live: physical > 0, available_pct ≤ 100); `libproc_finds_own_process` (snapshot contains `std::process::id()` with footprint > 0 and the right ppid); `libproc_skips_vanished_pids`.
- [ ] **Step 2: Run** `cargo nextest run -p damperd sources memory procs` — Expected: FAIL.
- [ ] **Step 3: Implement.** Check the field names in the libproc version `cargo add` picks; if `RUsageInfoV4` lacks a lifetime peak, use `ri_phys_footprint` as `peak` and say so in the report.
- [ ] **Step 4: Run** — Expected: PASS. **Commit** `feat(daemon): read memory pressure and the process table`

---

### Task 10: Daemon: event log, server and main loop

**Files:**
- Create: `crates/daemon/src/eventlog.rs`, `crates/daemon/src/server.rs`, `crates/daemon/src/runtime.rs` (broker task, applies actions), `crates/daemon/src/main.rs`

**Interfaces:**
- Consumes: everything from Tasks 2–9.
- Produces:
  ```rust
  pub struct EventLog { /* dir, current file, size */ }
  impl EventLog {
      pub fn open(dir: &Path) -> io::Result<Self>;
      pub fn append(&mut self, kind: EventKind) -> io::Result<()>;   // stamps ts_ms, rotates at 10 MiB keeping 4 old files
      pub fn read(&self, since_ms: Option<u64>) -> io::Result<Vec<Event>>; // oldest first across rotated files; skips corrupt lines
      pub fn ingest_fallbacks(&mut self, path: &Path) -> io::Result<u32>;  // moves fallbacks.jsonl entries in, then truncates it
  }
  pub async fn run(paths: Paths, mem: Arc<dyn MemorySource>, procs: Arc<dyn ProcessSource>) -> anyhow::Result<()>;
  ```
- Startup order in `run`: `prepare_dirs`; take an exclusive `flock` on the lock file (held: exit with "damperd is already running"); `sweep_stale_fifos`; open the event log and ingest fallbacks; load config; remove a leftover socket file (safe under the lock); bind, chmod 0600; spawn the pid watcher; start the broker task with a 1 s interval.
- Connection handling: check `peer_cred().uid() == getuid()` first, else log `ClientRejected` and drop. Read lines with a `MAX_LINE` cap: an oversized line, bad JSON or a wrong version → `Error{message}` and close. `Open` → `Broker::open`; the parent's process name (`process_name(parent_pid) == "rust-analyzer"`) sets `never_hold`. **[guess]** from spec §5.4: confirm in Task 15 that rust-analyzer's `cargo check` has rust-analyzer as its direct parent.
- The broker task owns the `Broker`, the `SessionFifo`s and one `mpsc::Sender<ServerMsg>` per session connection. Each tick, it runs both sources in `spawn_blocking`, reads every FIFO's `fill`, calls `tick` and applies the actions. Config: it checks the config file's mtime each tick and calls `reconfigure` when it changes.
- Shutdown on SIGTERM or SIGINT: stop accepting, drop all connections (shims self-feed), remove the socket, leave FIFOs in place, exit 0.
- Errors applying an action (a feed failing) go to the event log as `Error` and never stop the daemon.

- [ ] **Step 1: Write the failing tests**:
  - `eventlog_rotates_and_reads_in_order` (with a small rotate size injected through `EventLog::open_with_limit(dir, bytes)`); `eventlog_skips_corrupt_line`; `ingest_fallbacks_moves_entries`.
  - In `crates/daemon/tests/server.rs` with fake `MemorySource`/`ProcessSource` and a temp state dir: `serves_status`; `open_returns_fifo_and_grant`; `rejects_oversized_line_and_keeps_serving` (another client still gets `Status`); `rejects_wrong_version`; `rejects_garbage_json`; `second_daemon_refuses_to_start`; `session_cap_returns_error` (lower the cap through a test constructor); `restart_keeps_live_fifos` (daemon 1 opens a session for a live child pid, is shut down, daemon 2 starts: the FIFO still exists); `shutdown_removes_socket`.
  - The other-uid check is covered by a unit test on the pure `peer_allowed(peer_uid, own_uid) -> bool`, since tests can't connect as another user.
- [ ] **Step 2: Run** `cargo nextest run -p damperd` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Manual check.** Run `cargo run -p damperd` in one pane, and in another `printf '{"v":1,"type":"status"}\n' | nc -U ~/.local/state/damper/run/damperd.sock`. Expected: one `status` JSON line with the limit at 1 and the live pressure.
- [ ] **Step 6: Commit** `feat(daemon): serve the socket API and run the broker loop`

---

### Task 11: Shim: finding cargo, priority and every path without a session

**Files:**
- Create: `crates/shim/src/main.rs`, `crates/shim/src/resolve.rs`, `crates/shim/src/launch.rs`, `crates/shim/src/decide.rs`

**Interfaces:**
- Consumes: `classify` (3), `config::load` and `Priority` (2), `paths` (8).
- Produces:
  ```rust
  pub fn find_real_cargo(path_var: &OsStr, own_exe: &Path) -> Option<PathBuf>;
  pub fn command_for(cargo: &Path, args: &[OsString], priority: Priority, taskpolicy: &Path, nice: &Path) -> Command;
  pub enum Route { Exec { priority: Priority }, Session(Invocation) }
  pub fn route(env: &ShimEnv, inv: Invocation, cfg: &Effective, disabled_flag: bool) -> Route;
  pub struct ShimEnv { pub damper_off: bool, pub nested: bool }
  ```
- `find_real_cargo` walks `PATH` entries in order and returns the first `<entry>/cargo` that is an executable file and whose `canonicalize()` differs from `canonicalize(own_exe)`. It returns the **un-canonicalized** `<entry>/cargo` path (spec §4: never resolve the link, never run a toolchain's own cargo). Empty entries are skipped.
- `command_for`: `Utility` → `taskpolicy -c utility <cargo> args…`; `Nice` → `nice -n 15 <cargo> args…`; `None` → `<cargo> args…`. If the wrapper tool isn't executable, run cargo directly. The cargo path is passed as given, so its basename, `cargo`, is what rustup's proxy sees.
- `route`: `DAMPER=off` → `Exec{None}`. Nested (`DAMPER_SESSION` set, or `CARGO_MAKEFLAGS` names a FIFO in damper's run dir) → `Exec{None}`: priority is already inherited. Disabled flag or `enabled = false` → `Exec{cfg.priority}`. `PassThrough` → `Exec{cfg.priority}`. Otherwise `Session`.
- `main`, this task: compute the route; `Exec` → `command_for(…).exec()` (replaces the process). `Session` → for now also exec (Task 12 replaces this branch). If no real cargo is found, print `damper: no cargo found on PATH after damper's shim` to stderr and exit 127. Any panic or error before exec → exec the real cargo with no wrapper.

- [ ] **Step 1: Write the failing tests**:
  - `finds_next_cargo_skipping_itself` (temp dirs `a/cargo` → symlink to the shim test binary, `b/cargo` → a script; PATH `a:b` → `b/cargo`).
  - `returns_symlink_path_not_target` (`b/cargo` is a symlink to `b/rustup` → result is `b/cargo`).
  - `skips_non_executable_and_empty_entries`.
  - `utility_wraps_with_taskpolicy` and `missing_taskpolicy_runs_cargo_directly` (inspect `Command::get_program`/`get_args`).
  - `route_table`: one row per rule above.
  - `crates/shim/tests/exec.rs`: build a fake proxy (a shell script `fakebin/rustup` printing `$0` and `$@`, symlinked as `fakebin/cargo`), run the built `damper-shim` with `PATH=<shimdir>:<fakebin>` and `DAMPER=off`: output shows `$0` ending in `fakebin/cargo` and the args unchanged, including one with spaces and one non-UTF-8. With `priority = "utility"` and no daemon: same output, and the exit code of the fake (exit 3) comes through.
- [ ] **Step 2: Run** `cargo nextest run -p damper-shim` — Expected: FAIL.
- [ ] **Step 3: Implement.** The shim must not read the env beyond `PATH`, `DAMPER`, `DAMPER_SESSION`, `CARGO_MAKEFLAGS`, `DAMPER_STATE_DIR`, `NEXTEST_TEST_THREADS` and `RUST_TEST_THREADS`.
- [ ] **Step 4: Run** — Expected: PASS. **Commit** `feat(shim): find the real cargo and apply priority`

---

### Task 12: Shim: sessions, holds, threads, signals and self-feed

**Files:**
- Create: `crates/shim/src/session.rs`, `crates/shim/src/hold.rs`, `crates/shim/src/supervise.rs`, `crates/shim/src/fallback.rs`
- Modify: `crates/shim/src/main.rs` (the `Session` branch)

**Interfaces:**
- Consumes: `Client`, messages and `paths` (4, 8), `command_for` (11).
- Produces:
  ```rust
  pub enum Opened { Granted { session: SessionId, fifo: PathBuf, threads: u32, client: Client }, Unavailable(String) }
  pub fn open_session(paths: &Paths, inv: &Invocation, out: &mut dyn Write, interrupted: &AtomicBool) -> Opened;
  pub fn hold_line(reason: HoldReason, waited: Duration, session: &SessionId) -> String;
  pub fn timeout_line(waited: Duration, session: &SessionId) -> String;
  pub fn session_env(fifo: &Path, session: &SessionId, inv: &Invocation, threads: u32) -> Vec<(OsString, OsString)>;
  pub fn self_feed(fifo: &Path, tokens: u32) -> io::Result<()>;
  pub fn record_fallback(paths: &Paths, reason: &str, cwd: &Path);  // appends one JSON line, ignores every error
  ```
- Exact copy (spec §6.3):
  - `hold_line`: `damper: holding this build, {reason} (waited {secs}s). Run 'damper status' for details. Session {id}.`
  - `timeout_line`: `damper: held for {minutes}m, running this build with one job. Run 'damper explain {id}' for details.`
- Flow:
  1. Connect with `CONNECT_TIMEOUT` and send `Open`. No `Opened` within the timeout, an `Error`, or a version mismatch → `Unavailable` → `record_fallback` and exec the real cargo with priority. No output.
  2. On `Held`, print `hold_line` at once and every `HOLD_NOTICE`. Wait for `Granted`, printing `timeout_line` if `after_timeout`. SIGINT or SIGTERM while held → exit as that signal (`emulate_default_handler`).
  3. Spawn `command_for(...)` with `session_env` added, and send `Started{cargo_pid}`.
  4. A reader thread watches the socket. On close, or no message for `HEARTBEAT_TIMEOUT`, it calls `self_feed(fifo, cores − 1)` once (cores from `available_parallelism`), sets a flag so the shim unlinks the FIFO at exit, and stops reading.
  5. Forward SIGINT, SIGTERM, SIGHUP and SIGQUIT to cargo (signal-hook iterator thread, `nix::sys::signal::kill`).
  6. Wait for cargo. Exit with its code, or with `emulate_default_handler` for its signal.
- `session_env`: `CARGO_MAKEFLAGS` and `MAKEFLAGS` = `-j --jobserver-auth=fifo:<fifo>`, `DAMPER_SESSION=<id>`; `NEXTEST_TEST_THREADS=<threads>` for `Nextest` invocations that are `nextest run` without `user_threads`; `RUST_TEST_THREADS=<threads>` for `Test` without `user_threads`. An existing `MAKEFLAGS` from a parent make is replaced: damper's pool is machine-wide. **[call]**

- [ ] **Step 1: Write the failing tests**:
  - Unit tests: `hold_line_exact_copy`; `timeout_line_exact_copy`; `session_env_sets_threads_only_when_user_did_not` (four cases); `self_feed_writes_tokens` (real FIFO in a temp dir).
  - `crates/shim/tests/session.rs` with a scripted fake daemon (a test `UnixListener` thread speaking the protocol, `DAMPER_STATE_DIR` pointing at a temp dir) and the fake cargo from Task 11, extended to print `CARGO_MAKEFLAGS` and `DAMPER_SESSION`:
    - `granted_session_runs_cargo_with_fifo_env`;
    - `silent_daemon_falls_back_within_timeout` (accepts, never answers: cargo runs within 1 s, stderr empty, a line appears in `fallbacks.jsonl`);
    - `held_build_prints_hold_line_then_runs`;
    - `sigterm_while_held_exits_as_sigterm`;
    - `daemon_vanishing_mid_build_triggers_self_feed` (the fake daemon closes after `Started`; the fake cargo reads the FIFO and sees `cores − 1` tokens);
    - `exit_code_and_signal_pass_through` (fake cargo exits 101; fake cargo kills itself with SIGKILL → the shim's status is signal 9).
- [ ] **Step 2: Run** `cargo nextest run -p damper-shim` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS. **Commit** `feat(shim): open sessions, show holds and fail open`

---

### Task 13: CLI

**Files:**
- Create: `crates/cli/src/main.rs`, `crates/cli/src/render.rs` (human output), `crates/cli/src/doctor.rs`, `crates/cli/src/commands.rs`

**Interfaces:**
- Consumes: `Client`, proto types, `paths`, `config::load`.
- Produces the commands of spec §7.2: `damper status [--json]`, `damper log [--json] [--since <duration>] [--session <id>]`, `damper explain <session> [--json]`, `damper config check`, `damper doctor`, `damper enable`, `damper disable`.
  ```rust
  pub fn render_status(s: &Status) -> String;
  pub fn render_events(e: &[Event]) -> String;
  pub fn render_explain(session: &SessionId, e: &[Event]) -> String;
  pub struct Check { pub name: &'static str, pub ok: bool, pub detail: String }
  pub fn doctor(paths: &Paths, path_var: &OsStr) -> Vec<Check>;
  ```
- `--json` prints the proto types as JSON, one document. `log --since 1h` converts to `since_ms` on the client side.
- `enable` and `disable` create and remove the disabled flag and work without the daemon. `config check` works without the daemon: it prints the effective values and the warnings, and exits 1 when there are warnings.
- `doctor` checks: the first `cargo` on `PATH` canonicalizes to a file named `damper-shim`; the daemon answers `Status` with the same protocol version; run dir 0700 and socket 0600, owned by the user; the config is valid; whether the disabled flag is set. It exits 1 when any check fails.
- With the daemon down, `status`, `log` and `explain` print `damper: damperd is not running (socket <path>). Start it with 'brew services start damper'.` and exit 2.

- [ ] **Step 1: Write the failing tests**: insta snapshots `status_two_sessions_one_held`, `events_mixed`, `explain_held_then_ran`; `doctor_flags_shim_not_first_on_path`; `doctor_flags_loose_socket_mode`; `enable_disable_toggle_flag`; `status_without_daemon_exits_2` (assert_cmd-style integration test in `crates/cli/tests/`).
- [ ] **Step 2: Run** `cargo nextest run -p damper` — Expected: FAIL.
- [ ] **Step 3: Implement** with clap derive.
- [ ] **Step 4: Run** — Expected: PASS. **Commit** `feat(cli): add status, log, explain, config check, doctor, enable and disable`

---

### Task 14: End-to-end tests with real processes

**Files:**
- Create: `crates/e2e/Cargo.toml` (`publish = false`, a `[[bin]] fake-cargo` and dev-deps on the other crates), `crates/e2e/src/bin/fake-cargo.rs`, `crates/e2e/tests/{harness.rs,flows.rs,fail_open.rs}`

**Interfaces:**
- `fake-cargo` behaviour, driven by its args: `jobs <n> <ms>` takes up to `n` tokens from the jobserver FIFO in `CARGO_MAKEFLAGS` (blocking reads, like cargo; one implicit job without a token), records the highest concurrency to a file named by its `--report <path>` arg, sleeps `<ms>` per job, returns every token, exits 0. `exit <code>` exits with that code. `spawn-rustc <ms>` starts a child it copied of itself named `rustc`, so the daemon sees a compiling session.
- The harness starts the real `damperd` binary with a temp `DAMPER_STATE_DIR` and the real sources, puts the real `damper-shim` first on `PATH` as `cargo`, with `fake-cargo` next as `cargo`.

- [ ] **Step 1: Write the tests**:
  - `flows.rs`: `build_gets_tokens_and_returns_them` (after the run, `damper log --json` has `SessionClosed` with `tokens_recovered` equal to the tokens fed); `two_builds_share_fairly` (two concurrent `jobs 8 2000`: neither's highest concurrency exceeds the other's by more than 1 once both run); `passthrough_never_opens_session` (`cargo metadata` → no `SessionOpened`).
  - `fail_open.rs` (spec §11 fail-open checks): `kill_damperd_mid_build` (SIGKILL the daemon during `jobs 4 3000`: the build exits 0 within its expected time plus 2 s); `sigkill_shim_frees_session_after_cargo_exits` (SIGKILL the shim: the session stays open until fake-cargo exits, then `SessionClosed`); `sigkill_cargo_closes_session`; `broken_config_uses_defaults` (garbage config: builds run, `damper status --json` shows a warning); `damperd_restart_keeps_running_build` (SIGTERM and restart the daemon mid-build: the build finishes, its FIFO isn't deleted while it runs, and is gone after); `no_daemon_runs_cargo_silently` (stderr empty, exit code passes through).
- [ ] **Step 2: Run** `cargo nextest run -p damper-e2e`
  Expected: PASS. Any failure is a bug in Tasks 7–13: fix it there, in this task's branch, with a unit test that reproduces it.
- [ ] **Step 3: Commit** `test: add end-to-end and fail-open tests`

---

### Task 15: Docs, acceptance test and tuning (controller with the owner)

**Files:**
- Create: `docs/usage.md` (build from source, put the shim first on `PATH`: `mkdir -p ~/.local/share/damper/shim && ln -sf "$(pwd)/target/release/damper-shim" ~/.local/share/damper/shim/cargo` and the profile line, run `damperd`, config reference with every key and default, the CLI, `DAMPER=off`, how aliases are handled), `scripts/acceptance/run.py` (stdlib, `python3 -I`), `scripts/acceptance/README.md`, `docs/superpowers/spikes/2026-11-xx-acceptance.md` (results)
- Modify: `README.md`, `.config/nextest.toml`, `.github/workflows/ci.yml` (`timeout-minutes`), policy constants in `crates/core/src/policy.rs` only if the run shows they need it

- [ ] **Step 1: Write `docs/usage.md` and update `README.md`.**
- [ ] **Step 2: Measure the test suite** on an idle Mac. Set nextest's `slow-timeout` to about 3× the slowest test (minimum 30 s, simple value) and CI's `timeout-minutes` to about 3× the suite's run.
- [ ] **Step 3: Write `scripts/acceptance/run.py`.** It samples every second: `kern.memorystatus_vm_pressure_level`, `vm.swapusage` and the 10 ms sleep probe from Phase 0 (p50, p99, max). It starts 5 full `--release` builds of warden in 5 worktrees with cold build dirs (`--target-dir` on the SSD per worktree, deleted afterwards), waits for them and writes a JSON summary: wall time, seconds spent at critical, the longest critical stretch, swap peak, probe latencies, exit codes.
- [x] **Step 4: Confirm the rust-analyzer [guess]**: open a Rust file in the editor with damper running, and check that `damper status` shows its check as never held (or record that the parent isn't rust-analyzer and treat it as a normal build, per the spec).
- [ ] **Step 5: Run the baseline and the damper run with the owner** (spec §11), including the memory-pressure event check left over from Phase 0. Hard gates: critical pressure only in short spikes, the Mac responsive, all 5 builds pass. Record wall time; flag it if the damper batch takes more than twice the baseline.
- [ ] **Step 6: Tune** the policy constants only where the run shows a reason, change the table in this plan and in `policy.rs` together, and re-run.
- [ ] **Step 7: Write the results** in `docs/superpowers/spikes/<date>-acceptance.md`, with the same `[call]` tags as the Phase 0 findings.
- [ ] **Step 8: Run the full gate and commit** `docs: add usage guide and acceptance results`
