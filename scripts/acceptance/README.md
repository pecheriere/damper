# Acceptance run

`run.py` is the Phase 1 acceptance test (spec section 11): five cold `--release`
builds of warden at once, in five git worktrees, with the machine sampled every
second. It uses only the standard library.

```sh
# Check the plan first. Nothing is built.
python3 -I scripts/acceptance/run.py --mode baseline --dry-run

# Baseline: DAMPER=off, plain cargo.
python3 -I scripts/acceptance/run.py --mode baseline --output baseline.json

# damper: needs damperd running and the shim installed (docs/usage.md).
python3 -I scripts/acceptance/run.py --mode damper --output damper.json
```

Options: `--warden` (default `~/Code/warden`), `--builds` (5), `--ssd-dir`
(`/Volumes/External SSD/damper-acceptance`; refused if it is on the internal disk),
`--sccache {cold,off}` (cold), `--shim-dir` (`~/.local/share/damper/shim`), `--output`, `--log-dir`, `--dry-run`.
Each run makes its own fresh directory under the SSD dir and deletes only that
(and the SSD dir itself if the run created it), also when a build fails or the
run is interrupted (SIGINT, SIGTERM, SIGHUP). Close other heavy work first, and let the
Mac cool down between the two runs.

Every second it reads `kern.memorystatus_vm_pressure_level`, `vm.swapusage`
and a 10 ms sleep probe (how late the sleep wakes: p50, p99, max). The JSON
summary has the wall time, the seconds at critical pressure, the longest
critical stretch, the swap peak, the probe latencies and the exit codes. The
script exits 0 when all builds pass. At idle the probe wakes about 5 ms late;
read the numbers against that.

Hard gates (spec section 11): critical pressure only in short spikes, the Mac
responsive, all builds pass. Flag the damper run if its wall time is more than
twice the baseline's.

Test the pure parts with `python3 -I -m unittest discover -s scripts/acceptance`.

Builds are cold. Per build the script passes `--config` flags (never `CARGO_*`
variables): a fresh `build.build-dir` inside the run directory, and the sccache
mode. `--sccache cold` starts a dedicated sccache server with a fresh cache and
port for the run (so rustc still runs under sccache, as in real use) and stops
it afterwards; the cache dir and port are also forced with `env.SCCACHE_DIR` and
`env.SCCACHE_SERVER_PORT` so the `[env]` table in `~/.cargo/config.toml` cannot
override them. `--sccache off` clears the rustc wrapper. Baseline and damper runs
use separate fresh directories, so neither sees the other's files. Inherited
`MAKEFLAGS`, `MFLAGS`, `CARGO_MAKEFLAGS`, `RUSTC_WRAPPER` and the sccache
variables are removed from the builds' environment.

Each build's output goes to `build-N.log` in the log dir (next to `--output` as
`<name>-logs`, else `./acceptance-<mode>-logs`); logs are kept. The JSON also
records the warden commit and whether its checkout is dirty (only the committed
`HEAD` is built), the sccache mode, `samples_complete`, and any sampler errors.
The script exits non-zero when sampling failed or produced no samples. Probe
percentiles are computed over every lateness value pooled across the run; the
critical-time figures use the real time between samples.
