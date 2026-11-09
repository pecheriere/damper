# damper Phase 0: spike findings

Date: 2026-11-09. Plan: `docs/superpowers/plans/2026-11-09-phase0-spikes.md`. Design: `docs/superpowers/specs/2026-11-08-damper-design.md`.

Machine: Mac mini M4 (4 performance and 6 efficiency cores, 32 GB), macOS Darwin 27.2, cargo 1.99.1, sccache 0.19.0.

Test workspace: 24 generated library crates with 1,200 generic functions each, plus one binary depending on all of them. At default `-j` it builds in 11.8 s, with 10 concurrent rustc. Real project: warden at `30449bb`, a full `--release` build of the workspace.

Tags: **[call]** is the author's proposal, open to the owner's override.

## Summary

| Spike | Question (design §9) | Result |
|---|---|---|
| S1 | Does cargo honour an inherited FIFO jobserver, live? | **Confirmed** |
| S2 | Does sccache stay correct under per-session FIFOs? | **Confirmed**, with one nuance |
| S3 | Does `FIONREAD` accounting on a macOS FIFO work? | **Confirmed, with a constraint**: it needs a read-only fd |
| S4 | Are memory-pressure signals readable without privileges? | **Confirmed**, except that simulating events needs root |
| S5 | Utility QoS against `nice`? | **Utility wins on responsiveness**, at about 25–35% longer builds |

Nothing refutes the design. Five changes follow, listed at the end.

## S1: cargo with an inherited FIFO jobserver

- **Job count follows the tokens.** The pool seeded N tokens, `CARGO_MAKEFLAGS="-j --jobserver-auth=fifo:<path>"`, no sccache:

  | N | max concurrent rustc | wall |
  |---|---|---|
  | 0 | 1 | 53.7 s |
  | 1 | 2 | 28.1 s |
  | 3 | 4 | 16.1 s |

  The maximum is always N+1. The extra one is cargo's implicit slot, as design §5.1 expects.
- **Growth works live.** Starting from 1 token and adding 6 more after 5 s, concurrency per second went 2, 2, 2, 2, 7, 8, 8 … 8, then tailed off. Cargo picks up new tokens within a second.
- **Tokens all come back.** After each build the FIFO held exactly the tokens seeded plus added (3 after N=3, 7 after the growth run).
- **Variables.** Cargo honours `CARGO_MAKEFLAGS`, `MAKEFLAGS` and `MFLAGS` alike (N=1 gives a maximum of 2 with each).

## S2: sccache under per-session FIFOs

- **Concurrency follows the tokens on cache misses.** With a cold cache and N=3, the maximum was 4 concurrent rustc, spawned by the sccache server.
- **Cache keys ignore the FIFO path.** The test workspace has `incremental = false`, see the note below.

  | Build | hits | misses |
  |---|---|---|
  | FIFO path `a.fifo`, cold | 0 | 24 |
  | different path `b.fifo` | **24** | 0 |
  | no jobserver | 24 | 0 |

  The source confirms it. `src/compiler/rust.rs:1867` in sccache 0.19.0 skips `CARGO_MAKEFLAGS` when hashing the `CARGO_*` env vars, with the comment "CARGO_MAKEFLAGS will have jobserver info which is extremely non-cacheable".
- **The nuance.** The sccache server creates its own jobserver, sized to `num_cpus` (10 here), in `src/jobserver.rs:48` and `commands.rs:959`. When it spawns rustc, it removes `MAKEFLAGS`, `MFLAGS` and `CARGO_MAKEFLAGS` and hands rustc its own pool instead (`src/mock_command.rs:266-268`, `share_jobserver: true` at `rust.rs:2095`). So:
  - damper's session tokens limit how many rustc *processes* run, because cargo holds a token for each job while the sccache client waits. That's the part that matters for memory.
  - rustc's *internal* thread parallelism draws on sccache's machine-wide pool of 10, not damper's.
  - That's compatible. sccache's pool is itself machine-wide, so it already stops thread oversubscription.
- **Note.** sccache never caches incremental compiles. In the dev profile, workspace members are incremental, so on real projects sccache caches dependencies only. That's sccache behaviour, unrelated to damper. The test needed `incremental = false` to exercise cache keys at all.

## S3: `FIONREAD` on a macOS FIFO

- **The constraint.** `FIONREAD` returns 0 on an `O_RDWR` fd of a FIFO, whatever the fill level. On an `O_RDONLY | O_NONBLOCK` fd it returns the right byte count: 3 after writing 5 and reading 2. damper must open a separate read-only fd for accounting.
- **It reads without disturbing cargo.** While cargo was blocked waiting on an empty FIFO, a read-only `FIONREAD` read 0, didn't block and consumed nothing. Writing 2 tokens moved concurrency from 1 to 3 at once.

## S4: memory signals without privileges

- **Readable as a normal user:**
  - `kern.memorystatus_vm_pressure_level` (1 at rest);
  - `kern.memorystatus_level` (57, the percentage of memory available);
  - `vm.swapusage` (2.4 GB of 4 GB used at rest);
  - `vm_stat` compressor pages.
- **The event source registers.** `DispatchSource.makeMemoryPressureSource` registers fine from a plain Swift binary.
- **Not verified: events firing.** `memory_pressure -S` (simulation) needs root, failing with `kern.memorypressure_manual_trigger: Operation not permitted`. I didn't escalate. Real events should occur during the Phase 1 acceptance test (§11), which drives the Mac into pressure. The sysctl level is the fallback, polled every second.
- **`proc_pid_rusage` works on the user's own processes** (`ri_phys_footprint`). Peak footprint per rustc:

  | Build | rustc runs | median | p90 | max | peak sum | linker max |
  |---|---|---|---|---|---|---|
  | test workspace (debug) | 25 | 319 MB | 385 MB | 391 MB | 3.1 GB | 335 MB |
  | warden (release) | 472 | 41 MB | 120 MB | **1.3 GB** | 1.7 GB | 98 MB |

  On a real project, memory per job is heavy-tailed. Most jobs are small, and one crate (the final binary's) dominates.

## S5: utility QoS against `nice`

Warden, a clean `--release` build of the workspace, no sccache. Configurations alternated, two rounds. The responsiveness probe is a default-priority process that sleeps 10 ms in a loop and records how late it wakes. At idle it wakes a steady 5 ms late (p50 5.01, p99 5.05, max 5.1 ms), a timer-coalescing offset, so read the numbers against that.

| Config | wall, round 1 / 2 | probe p99, round 1 / 2 | probe max, round 1 / 2 |
|---|---|---|---|
| A: plain | 62.0 / 75.7 s | 9.28 / 8.71 ms | 13.2 / 20.4 ms |
| B: `nice -n 15` (today's shim) | 60.1 / 79.6 s | 9.19 / 8.39 ms | 13.9 / 21.3 ms |
| C: `taskpolicy -c utility` | 76.8 / 103.5 s | **5.05 / 5.04 ms** | 7.5 / 12.1 ms |
| D: `taskpolicy -c background` | 401.9 / 340.5 s | 5.11 / 5.11 ms | 10.9 / 14.8 ms |

- **`nice -n 15` barely helps.** Its latency is the same as plain cargo, adding about 4 ms at p99 over idle. Today's shim protects responsiveness much less than intended.
- **Utility QoS keeps the probe at idle-level latency** (p99 equal to idle), at a cost of about 25–35% longer wall time.
- **Background QoS is 5–6 times slower**, as expected from its limit to the efficiency cores. It's ruled out.
- **Caveats.** Two rounds only, and round 2 was slower across the board (thermals, or other activity). The probe stands in for interactive responsiveness, it doesn't measure the UI directly.
- **Recommendation: default `priority = "utility"`.** **[call]** It fits the owner's decision that responsiveness beats throughput. `"nice"` and `"none"` stay available.

## Other finding: the shim must use rustup's proxy

During S5 setup, running the toolchain's cargo binary directly (`rustup which cargo` → `~/.rustup/toolchains/1.99.1-…/bin/cargo`) broke warden's build with `E0514`. Cargo then calls `rustc` through rustup's proxy without a toolchain set. The proxy resolves the toolchain from each crate's directory, and one registry crate compiled with rustc 1.85.0. Going through `~/.cargo/bin/cargo`, the proxy, the build succeeds.

Design §4 already says the shim finds "the next `cargo` on `PATH`", which is the proxy, and that's correct. It needs to become an explicit requirement with a test, because "find the real cargo" invites exactly this mistake.

## Design changes for the owner to rule on

1. **Default priority is utility QoS**, from S5 (§7.4, §7.5). Today's `nice -n 15` shim turned out to protect responsiveness much less than assumed. **[call]**
2. **Size the pool from measured total memory, not an average per job** (§6.2). Warden's jobs range from 41 MB (median) to 1.3 GB.
   - Replace `limit = min(cores, headroom ÷ estimated memory per job)` with a closed loop on the sessions' measured total footprint against the memory budget and live pressure.
   - Keep a margin for one large job, such as the largest footprint seen recently.
   - **[call]**
3. **Account through a separate read-only fd**, from S3 (§5.3). `FIONREAD` on the broker's read-write fd always reads 0. **[call]**, an implementation constraint.
4. **The shim must exec the rustup proxy**, never a toolchain's cargo binary (§4), with a test for it. **[call]**
5. **Record the sccache nuance in §5.1 and §6.2.** Session tokens limit rustc processes. rustc's internal threads use sccache's own pool of 10 for the whole Mac. No change in behaviour, but the policy should know that thread parallelism isn't damper's to limit when sccache is the wrapper. **[call]**

Not verified, carried to Phase 1: memory-pressure events firing in a user agent under real pressure (S4).
