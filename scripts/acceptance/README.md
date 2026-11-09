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
(`/Volumes/SSD/damper-acceptance`; each build's `--target-dir` and worktree go
there), `--shim-dir` (`~/.local/share/damper/shim`), `--output`, `--dry-run`.
Keep the SSD dir on the external SSD. Worktrees and target dirs are deleted
afterwards, also when a build fails. Close other heavy work first, and let the
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
