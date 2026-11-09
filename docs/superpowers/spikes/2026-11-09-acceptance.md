# damper Phase 1: acceptance results

Date: 2026-11-09. Plan: `docs/superpowers/plans/2026-11-09-phase1-engine.md`, Task 15. Design: `docs/superpowers/specs/2026-11-08-damper-design.md` §11.

Machine: Mac mini M4 (10 cores, 32 GB), macOS Darwin 27.2, cargo 1.99.1. damper at `651a915` (release build), default config, daemon started about 10 minutes before the damper run.

Workload: `scripts/acceptance/run.py`, five cold `cargo build --release --locked` of warden at `30449bb` at once, each in its own worktree, target dir and build dir on the external SSD, with one dedicated cold sccache server per run (R21). Baseline: `DAMPER=off`, the owner's dotfiles shim (`nice -n 15`). damper run: damper's shim first on `PATH`, utility QoS, in front of the dotfiles shim.

Tags: **[call]** is the author's proposal, open to the owner's override. **[owner]** is the owner's decision.

## Results

| | Baseline | damper |
|---|---|---|
| Builds passed | 5 / 5 | 5 / 5 |
| Wall time | 329.3 s | 311.8 s (0.95×) |
| Time at critical pressure | 0 s | 0 s |
| Swap peak | 2443 MiB (no growth) | 2443 MiB (no growth) |
| Sleep probe p50 / p99 / max | 3.81 / 8.57 / 29.35 ms | 4.92 / 8.58 / 39.48 ms |
| Holds | n/a | 0 |
| Pool limit | n/a | 1 → 10 in about 18 s, then 6–10 (idle decay and regrowth near the end) |
| Peak build footprint | not measured | 4.5 GiB |

The idle probe wakes about 5 ms late, so both runs stayed responsive.

## Gates (spec §11)

- **All builds pass:** met in both runs.
- **Critical pressure only in short spikes:** met in both, but **not exercised**. The baseline never left normal pressure, so this workload doesn't show what damper is for. **[owner]** Accepted as an overhead check, with the memory test done later in a separate run that drives the baseline into critical (for example `--sccache off` and more builds). The S4 check carried over from Phase 0 (memory-pressure events firing in a user agent) waits for that run too.
- **Mac responsive:** met. Probe latencies are close to idle in both runs.
- **Wall time (record-only, flag above 2×):** 0.95× the baseline, not flagged.

## Observations

- **The limit tops out at the core count and costs no wall time here.** The damper batch capped five builds at 10 tokens in total, where the baseline let each build run up to 10 jobs, and finished slightly faster. Five warden builds through one sccache server don't gain from oversubscription on this machine.
- **No reason to tune the policy constants.** Nothing in the run pushed against a constant: no holds, growth reached the core count within seconds, and idle decay only acted while the builds drained. `INITIAL_RESERVE` didn't matter because the daemon had already measured footprints by then. **[call]** Leave the table as it is until the memory run gives evidence.
- **Not verified: priority under sccache.** Phase 0 measured utility QoS without sccache (about 25–35% slower). Here the damper run was no slower than the baseline. A likely reason: with sccache, rustc runs as a child of the sccache server, so it takes the server's priority, not the shim's. In this run the script started the server itself, at default priority. If that is right, damper's priority only reaches rustc when a damper build happens to start the sccache server. **[call]** Check it in the memory run by reading the QoS and nice value of live rustc processes, and track it as an issue.
- **Two script bugs, found and fixed on the first live run.** Cargo refuses inline tables in `--config` (`36b8370`) and can't merge a CLI table onto the plain-string `SCCACHE_DIR` in `~/.cargo/config.toml` (`651a915`). The script now passes plain strings, which take precedence over the file. Cleanup after the failed runs worked: no run directory or worktree was left behind.

## rust-analyzer parent check (Step 4)

Pending: needs the owner's editor with the shim first on its `PATH`.
