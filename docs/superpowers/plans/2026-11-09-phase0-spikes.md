# damper Phase 0 (spikes) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove or disprove the five assumptions in the design's §9 Phase 0 with measurements on the owner's Mac, and record the results in one findings document that the engine plan is written from.

**Architecture:** Throwaway scripts in the session scratchpad drive real `cargo` builds of a generated test workspace and of warden. A sampler counts concurrent `rustc` processes over time. Nothing here is product code: no tests, no crate, nothing kept except the findings document.

**Tech Stack:** zsh, Python 3 (`python3 -I`, stdlib only), cargo 1.99.1, sccache 0.19.0, `taskpolicy`, `sysctl`, `memory_pressure`.

**Spec:** `docs/superpowers/specs/2026-11-08-damper-design.md` (§5.1, §5.3, §6.2, §7.5, §9)

## Global Constraints

- Spike scripts live in the session scratchpad, never in the repo. Only `docs/superpowers/spikes/2026-11-09-phase0-findings.md` is committed.
- Build files stay on the external SSD, which the global `build-dir` handles automatically. Never set `CARGO_TARGET_DIR` or any other `CARGO_*` variable to choose a location. Use `--target-dir` on the SSD only if a one-off dir is unavoidable.
- When done, delete the SSD build dirs the spikes created (`/Volumes/External SSD/cargo-build/<hash>` for the generated workspace and any scratch warden worktree).
- Don't run `sccache --stop-server` while other builds are running. Check `procs cargo` first. `sccache --zero-stats` is fine.
- Run the measurements only while no other heavy builds are running. Check `procs rustc` before each measured run.
- Never print process environments: use anchored exact-name checks only.
- Commit messages carry no AI attribution (public repo).

## Review Focus

Measurement pitfalls that would give a wrong answer:

1. **sccache hits hiding concurrency.** A warm cache makes rustc processes too short to sample. S1 runs without sccache (`RUSTC_WRAPPER=` set to empty for the command only), and S2 checks concurrency on a cold cache first.
2. **Sampling too slowly.** At 1 s, short rustc processes are missed. Sample every 50 ms and report the maximum and a time series.
3. **The implicit slot.** A pool seeded with N tokens should show up to N+1 concurrent rustc. Read results against N+1, not N.
4. **The FIFO path differs per run in production.** S2 must compare cache hits across two *different* FIFO paths, not the same one twice.
5. **Thermal or background noise in the QoS measurement.** Alternate the configurations (A, B, C, D, A, B, C, D) rather than repeating one, and record the spread.

---

### Task 1: Test workspace and concurrency sampler

**Files:**
- Create (scratchpad): `$S/gen_ws.py`, `$S/sample.py`, `$S/ws/` (generated)

`$S` = `/private/tmp/claude-501/-Users-pecheriere/e5ddcaf7-f3a9-4bb2-9167-a8e632dbda74/scratchpad/phase0`

- [ ] **Step 1:** Write `gen_ws.py <dir> <n>`. It generates a cargo workspace of `n` independent library crates (`c00`…`cNN`) plus one binary `top` depending on all of them.
  - Each library holds about 300 generic functions instantiated with several types, so each crate takes at least 1–2 s to compile.
  - Generate with `n = 24`.
- [ ] **Step 2:** Write `sample.py <out.csv> [--interval 0.05]`. It writes `t,count` rows, where `count` is the number of processes whose executable name is exactly `rustc`, until it is killed.
  - Use `ps -A -o comm=` through `subprocess`, with an exact basename match.
  - Print the maximum count on SIGINT or SIGTERM.
- [ ] **Step 3:** Sanity check. Run `cargo build` in `ws` with `RUSTC_WRAPPER=` (no sccache), with the sampler running. Expected: the maximum concurrent rustc is about 10, cargo's default -j on this Mac. Then run `cargo clean`.

### Task 2: S1, cargo with an inherited FIFO jobserver

- [ ] **Step 1:** Write `pool.py <fifo> <tokens> [--add-after SECONDS --add N]`.
  - It creates the FIFO (mode 0600) and opens it read-write, so it never sees EOF.
  - It writes `<tokens>` bytes of `+`, then sleeps. It writes N more after the delay, if given.
  - It keeps the FIFO open until killed.
- [ ] **Step 2:** For N in 0, 1, 3: start the pool, start the sampler, then run `cargo build` in `ws` from clean with `RUSTC_WRAPPER=` and `CARGO_MAKEFLAGS="-j --jobserver-auth=fifo:<fifo>"`. Record the maximum rustc count and wall time. Expected: a maximum of N+1.
- [ ] **Step 3:** Live growth. Start with N=1 and `--add-after 5 --add 6`. Expected: the time series shows the count rising from at most 2 to at most 8 after about 5 s.
- [ ] **Step 4:** Check that tokens come back. After the build, read the FIFO's fill level with `fionread.py` (write it first, from Task 4 Step 1) and confirm it equals the tokens seeded plus added. Cargo returned everything.
- [ ] **Step 5:** Also check the `MAKEFLAGS` variable alone, without `CARGO_MAKEFLAGS`, and record which variable cargo honours.

### Task 3: S2, sccache under the session FIFO

- [ ] **Step 1:** Concurrency with sccache on a cold cache. Use a fresh copy of `ws` with a changed seed, so no cache entries exist. Repeat Task 2 Step 2 with N=3 and sccache as the wrapper (the global default). Expected: a maximum of 4 concurrent rustc, spawned by the sccache server. Record what you observe whether or not it matches.
- [ ] **Step 2:** Cache keys. Run `sccache --zero-stats`, build `ws` from clean with FIFO path `/…/a.fifo` and record `sccache --show-stats` hits. Then `cargo clean`, zero the stats again, and build with a *different* FIFO path `/…/b.fifo`. Expected: hits on the second build roughly equal the crate count. Zero hits means the FIFO path is in sccache's keys, and the design must change. Stop and report.
- [ ] **Step 3:** Baseline. Repeat Step 2 without any jobserver variable, and record the hits for comparison.
- [ ] **Step 4:** Compare against the sccache 0.19.0 source. Find where sccache filters `CARGO_*` env vars for its hash (`rg -n 'CARGO_MAKEFLAGS|starts_with\("CARGO_' ` in a fresh clone in the scratchpad), and quote the relevant lines in the findings.

### Task 4: S3, FIONREAD on a macOS FIFO

- [ ] **Step 1:** Write `fionread.py <fifo>`. It prints the byte count from `fcntl.ioctl(fd, termios.FIONREAD, …)` on a read-write fd of the FIFO.
- [ ] **Step 2:** Check that it reads 0 on an empty FIFO, 5 after writing 5 bytes, and 3 after another process reads 2.
- [ ] **Step 3:** Check what it reports while a cargo build blocks reading the FIFO. Expected: 0. Confirm it never blocks and never consumes a token.

### Task 5: S4, memory-pressure signals without privileges

- [ ] **Step 1:** As the normal user, read and record:
  - `sysctl kern.memorystatus_vm_pressure_level`, `vm.swapusage` and `hw.memsize`;
  - `vm_stat`, for compressor pages, as a stand-in for `host_statistics64`.
- [ ] **Step 2:** Write a short Swift script (`swift pressure.swift`, run from the scratchpad) using `DispatchSource.makeMemoryPressureSource(eventMask: [.warning, .critical, .normal])`. It prints events with timestamps.
  - Run it.
  - In another shell, run `memory_pressure -S -l warn`, then `-l critical`. If that needs sudo, say so and ask the owner rather than escalating.
  - Record whether events arrive, and whether the sysctl level changes too.
- [ ] **Step 3:** As the normal user, run `proc_pid_rusage` on the user's own rustc processes. Take resident and physical footprint from a short Swift or Python `ctypes` call against `libproc`, during a build. Record the typical footprint per rustc job for `ws` and for warden.

### Task 6: S5, utility QoS against nice

- [ ] **Step 1:** Write `latency.py <seconds>`, a responsiveness probe at default priority.
  - It loops `time.sleep(0.010)` and records how late each wakeup is.
  - It prints p50, p99 and the maximum lateness in ms.
- [ ] **Step 2:** Set up a warden checkout in a scratch git worktree (`git -C ~/Code/warden worktree add $S/warden-wt origin/main`).
  - Each configuration builds from clean, `cargo build --workspace --release`, with `RUSTC_WRAPPER=` so the compile load is real.
  - Run the four configurations in alternating order, twice each:
    - A: plain `cargo`;
    - B: `nice -n 15 cargo`;
    - C: `taskpolicy -c utility cargo`;
    - D: `taskpolicy -c background cargo`.
  - Clean between runs.
  - Run `latency.py` for the duration of each build, and record wall time.
  - Call the real cargo binary (`rustup which cargo`), so the dotfiles `nice` shim doesn't stack on top.
- [ ] **Step 3:** Record a table of configuration, wall time and latency p50, p99 and max, both runs each. Expected: C is close to A on wall time and close to B or better on latency, and D is much slower. Recommend the default `priority` from the data.
- [ ] **Step 4:** Remove the scratch worktree (`git -C ~/Code/warden worktree remove $S/warden-wt`) and its SSD build dir.

### Task 7: Findings document and cleanup

**Files:**
- Create: `docs/superpowers/spikes/2026-11-09-phase0-findings.md`

- [ ] **Step 1:** Write the findings. For each of S1–S5:
  - the question, from design §9;
  - the result: confirmed, refuted or partly confirmed;
  - the key numbers;
  - the exact commands;
  - what it changes in the design, if anything.
  - At the end, a list of design changes for the owner to rule on. If S2 refuted the design, that list leads.

- [ ] **Step 2:** Delete the spike SSD build dirs. Find them by the workspace paths under `$S`. Confirm with `dust -d 1 "/Volumes/External SSD/cargo-build"` before and after.
- [ ] **Step 3:** Commit.

```bash
git add docs/superpowers/spikes/2026-11-09-phase0-findings.md
git commit -m "docs: record phase 0 spike findings"
```

- [ ] **Step 4:** Render the findings with presenting-designs, and publish them as an artifact for the owner on mobile.
