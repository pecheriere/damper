"""Acceptance run: cold release builds of warden at once, with the machine sampled.

Run with ``python3 -I scripts/acceptance/run.py``. It creates one fresh run directory on the
SSD holding one git worktree of the warden checkout per build, a target dir, a build dir and
(in the default ``--sccache cold`` mode) a dedicated sccache server and cache. It builds in
all worktrees at once, samples memory pressure, swap and a 10 ms sleep probe every second,
and prints a JSON summary. Only what the script created is deleted afterwards.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Callable, Mapping, Sequence
    from types import FrameType

CRITICAL_LEVEL = 4
PROBE_SLEEP = 0.010
MIB = 1024 * 1024
DEFAULT_SSD_DIR = Path("/Volumes/External SSD/damper-acceptance")
# Inherited variables that would change what the builds do. CARGO_HOME and the rest of
# CARGO_* are kept; the sccache variables are set explicitly per mode.
STRIPPED_ENV = frozenset(
    {
        "MAKEFLAGS",
        "MFLAGS",
        "CARGO_MAKEFLAGS",
        "RUSTC_WRAPPER",
        "SCCACHE_DIR",
        "SCCACHE_SERVER_PORT",
    }
)
# Where macOS keeps the internal disk: the sealed system volume, the data volume and home.
INTERNAL_ROOTS = (Path("/"), Path("/System/Volumes/Data"), Path.home())


@dataclass
class Sample:
    """One second of machine state."""

    pressure: int
    swap_used: int
    at: float
    lateness: list[float]


@dataclass
class Plan:
    """What a run will do. Every path lives under ``run_dir``, except the logs."""

    mode: str
    warden: Path
    builds: int
    ssd_dir: Path
    run_dir: Path
    log_dir: Path
    sccache: str
    port: int

    def worktree(self, i: int) -> Path:
        """Worktree of build ``i``."""
        return self.run_dir / f"worktree-{i}"

    def target_dir(self, i: int) -> Path:
        """Return the final-artifacts dir of build ``i``."""
        return self.run_dir / f"target-{i}"

    def build_dir(self, i: int) -> Path:
        """Intermediate build files of build ``i``."""
        return self.run_dir / f"build-{i}"

    def log_file(self, i: int) -> Path:
        """Return the log of build ``i`` (stdout and stderr), kept with the JSON."""
        return self.log_dir / f"build-{i}.log"

    @property
    def sccache_dir(self) -> Path:
        """The run's own sccache cache."""
        return self.run_dir / "sccache"


@dataclass
class Runtime:
    """Everything the run created, so cleanup removes exactly that."""

    procs: list[subprocess.Popen[bytes]] = field(default_factory=list)
    worktrees: list[Path] = field(default_factory=list)
    run_dir: Path | None = None
    created_ssd_dir: Path | None = None
    sccache_env: dict[str, str] | None = None
    stop: threading.Event = field(default_factory=threading.Event)
    thread: threading.Thread | None = None


@dataclass
class Ops:
    """The side effects of cleanup, injectable so tests can record them."""

    run: Callable[[list[str], Mapping[str, str] | None], None]
    kill: Callable[[subprocess.Popen[bytes]], None]
    rmtree: Callable[[Path], None]
    rmdir: Callable[[Path], None]


def parse_args(argv: list[str]) -> argparse.Namespace:
    """Parse the command line."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--mode", choices=["baseline", "damper"], required=True)
    parser.add_argument("--warden", type=Path, default=Path.home() / "Code" / "warden")
    parser.add_argument("--builds", type=positive_int, default=5)
    parser.add_argument(
        "--ssd-dir",
        type=Path,
        default=DEFAULT_SSD_DIR,
        help="parent of this run's fresh directory; must be on the external SSD",
    )
    parser.add_argument(
        "--sccache",
        choices=["cold", "off"],
        default="cold",
        help="cold: dedicated sccache server and fresh cache; off: no rustc wrapper",
    )
    parser.add_argument(
        "--shim-dir",
        type=Path,
        default=Path.home() / ".local" / "share" / "damper" / "shim",
        help="directory holding the damper shim named cargo (damper mode)",
    )
    parser.add_argument("--output", type=Path, help="write the JSON summary here too")
    parser.add_argument(
        "--log-dir",
        type=Path,
        help="build logs go here (default: next to --output, else ./acceptance-<mode>-logs)",
    )
    parser.add_argument("--dry-run", action="store_true", help="print the plan, build nothing")
    return parser.parse_args(argv)


def positive_int(text: str) -> int:
    """Argparse type for an integer of at least one."""
    value = int(text)
    if value < 1:
        message = "must be at least 1"
        raise argparse.ArgumentTypeError(message)
    return value


def default_log_dir(args: argparse.Namespace) -> Path:
    """Where build logs go when ``--log-dir`` is not given."""
    if args.log_dir:
        return args.log_dir
    if args.output:
        return args.output.parent / f"{args.output.stem}-logs"
    return Path.cwd() / f"acceptance-{args.mode}-logs"


def make_plan(args: argparse.Namespace, run_dir: Path, port: int) -> Plan:
    """Work out the paths a run uses."""
    return Plan(
        mode=args.mode,
        warden=args.warden,
        builds=args.builds,
        ssd_dir=args.ssd_dir,
        run_dir=run_dir,
        log_dir=default_log_dir(args),
        sccache=args.sccache,
        port=port,
    )


def existing_ancestor(path: Path) -> Path:
    """Return the path itself, or the nearest parent that exists."""
    current = path
    while not current.exists():
        current = current.parent
    return current


def ssd_problem(
    path: Path,
    dev_of: Callable[[Path], int],
    internal_roots: Sequence[Path] = INTERNAL_ROOTS,
) -> str | None:
    """Why ``path`` is not on the external SSD (it shares a device with the internal disk)."""
    device = dev_of(existing_ancestor(path.absolute()))
    for root in internal_roots:
        if root.exists() and dev_of(root) == device:
            return f"{path} is on the internal disk (same device as {root}); mount the SSD"
    return None


def stat_device(path: Path) -> int:
    """``st_dev`` of a path."""
    return path.stat().st_dev


def sccache_args(plan: Plan) -> list[str]:
    """Return the per-build ``--config`` flags for the sccache mode (no CARGO_* env)."""
    if plan.sccache == "off":
        return ["--config", 'build.rustc-wrapper=""']
    return [
        "--config",
        'build.rustc-wrapper="sccache"',
        "--config",
        f"env.SCCACHE_DIR={{value={json.dumps(str(plan.sccache_dir))}, force=true}}",
        "--config",
        f'env.SCCACHE_SERVER_PORT={{value="{plan.port or "<free port>"}", force=true}}',
    ]


def build_command(plan: Plan, i: int) -> list[str]:
    """Return the cargo command line of build ``i``."""
    return [
        "cargo",
        "build",
        "--release",
        "--locked",
        "--target-dir",
        str(plan.target_dir(i)),
        "--config",
        f"build.build-dir={json.dumps(str(plan.build_dir(i)))}",
        *sccache_args(plan),
    ]


def sccache_env(plan: Plan, base: Mapping[str, str]) -> dict[str, str]:
    """Return the environment of the sccache server and its clients: own dir and port."""
    return {**base, "SCCACHE_DIR": str(plan.sccache_dir), "SCCACHE_SERVER_PORT": str(plan.port)}


def clean_env(inherited: Mapping[str, str]) -> dict[str, str]:
    """Return the inherited environment minus the variables that would change the builds."""
    return {k: v for k, v in inherited.items() if k not in STRIPPED_ENV}


def build_env(plan: Plan, inherited: Mapping[str, str], shim_dir: Path) -> dict[str, str]:
    """Return the build environment: damper off in baseline, the shim first on PATH otherwise."""
    env = clean_env(inherited)
    if plan.sccache == "cold":
        env = sccache_env(plan, env)
    if plan.mode == "baseline":
        env["DAMPER"] = "off"
    else:
        env["PATH"] = f"{shim_dir}{os.pathsep}{env.get('PATH', '')}"
    return env


def describe(plan: Plan, ssd_note: str) -> str:
    """Human-readable plan, printed by ``--dry-run``."""
    sccache = f"sccache: {plan.sccache}"
    if plan.sccache == "cold":
        sccache += f" (own server on a free port chosen at run time, cache {plan.sccache_dir})"
    lines = [
        f"mode: {plan.mode}",
        f"warden: {plan.warden}",
        f"run dir: {plan.run_dir} (a fresh directory is created under {plan.ssd_dir})",
        f"ssd check: {ssd_note}",
        sccache,
        f"builds: {plan.builds} at once; logs in {plan.log_dir}",
    ]
    for i in range(plan.builds):
        lines.append(f"  build {i}: worktree {plan.worktree(i)}")
        lines.append(f"    {' '.join(build_command(plan, i))}")
    lines.append("sampling: 1 s, pressure level, swap, 10 ms sleep probe")
    lines.append("cleanup: only the run dir (and the SSD dir if the run created it) is deleted")
    return "\n".join(lines)


def percentile(values: list[float], fraction: float) -> float:
    """Nearest-rank percentile of a non-empty list."""
    ordered = sorted(values)
    rank = max(1, math.ceil(fraction * len(ordered)))
    return ordered[rank - 1]


def critical_stats(samples: Sequence[Sample], start: float) -> tuple[float, float]:
    """Seconds at critical and the longest consecutive stretch, from real time deltas.

    Each sample covers the time since the previous one (the first, since ``start``).
    """
    total = 0.0
    longest = 0.0
    run = 0.0
    previous = start
    for sample in samples:
        delta = sample.at - previous
        previous = sample.at
        if sample.pressure >= CRITICAL_LEVEL:
            total += delta
            run += delta
            longest = max(longest, run)
        else:
            run = 0.0
    return total, longest


def parse_swap_used(text: str) -> int:
    """Bytes used, from ``sysctl -n vm.swapusage`` output."""
    match = re.search(r"used = ([0-9.]+)([KMGT])", text)
    if match is None:
        message = f"unrecognised vm.swapusage output: {text!r}"
        raise ValueError(message)
    scale = {"K": 1024, "M": MIB, "G": 1024 * MIB, "T": 1024 * 1024 * MIB}
    return int(float(match.group(1)) * scale[match.group(2)])


def summarise(
    samples: list[Sample],
    start: float,
    wall: float,
    exit_codes: list[int],
    sampler_errors: list[str],
) -> dict[str, object]:
    """Build the JSON summary of a run. Probe percentiles pool every lateness value."""
    total, longest = critical_stats(samples, start)
    pooled = [value for s in samples for value in s.lateness]
    return {
        "wall_seconds": round(wall, 1),
        "exit_codes": exit_codes,
        "all_passed": all(code == 0 for code in exit_codes),
        "samples": len(samples),
        "samples_complete": bool(samples) and not sampler_errors,
        "sampler_errors": sampler_errors,
        "seconds_at_critical": round(total, 1),
        "longest_critical_stretch_seconds": round(longest, 1),
        "swap_peak_mib": max((s.swap_used for s in samples), default=0) // MIB,
        "probe_p50_ms": round(percentile(pooled, 0.50) * 1000, 2) if pooled else None,
        "probe_p99_ms": round(percentile(pooled, 0.99) * 1000, 2) if pooled else None,
        "probe_max_ms": round(max(pooled) * 1000, 2) if pooled else None,
    }


def sysctl(name: str) -> str:
    """Read one sysctl value."""
    return subprocess.run(
        ["/usr/sbin/sysctl", "-n", name],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()


def probe(duration: float) -> list[float]:
    """Lateness in seconds of repeated 10 ms sleeps over ``duration``."""
    lateness = []
    end = time.monotonic() + duration
    while time.monotonic() < end:
        start = time.monotonic()
        time.sleep(PROBE_SLEEP)
        lateness.append(time.monotonic() - start - PROBE_SLEEP)
    return lateness


def sample_once() -> Sample:
    """One second: the probe runs for it, then pressure and swap are read."""
    lateness = probe(1.0)
    return Sample(
        pressure=int(sysctl("kern.memorystatus_vm_pressure_level")),
        swap_used=parse_swap_used(sysctl("vm.swapusage")),
        at=time.monotonic(),
        lateness=lateness,
    )


def sampler(
    stop: threading.Event,
    out: list[Sample],
    errors: list[str],
    take: Callable[[], Sample] = sample_once,
) -> None:
    """Sample every second until ``stop`` is set. A failure is recorded, not lost."""
    while not stop.is_set():
        try:
            out.append(take())
        except Exception as error:  # noqa: BLE001 - any sampling failure must reach the summary
            errors.append(f"{type(error).__name__}: {error}")
            return


def git(warden: Path, *args: str) -> str:
    """Run git in the warden checkout and return its stdout."""
    return subprocess.run(
        ["git", "-C", str(warden), *args],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def default_run(argv: list[str], env: Mapping[str, str] | None) -> None:
    """Run a command, raising if it fails."""
    subprocess.run(argv, check=True, capture_output=True, env=env)


def default_kill(proc: subprocess.Popen[bytes]) -> None:
    """Kill a still-running build (its whole process group) and reap it."""
    if proc.poll() is None:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            proc.kill()
    proc.wait()


def real_ops() -> Ops:
    """Return the real side effects."""
    return Ops(run=default_run, kill=default_kill, rmtree=shutil.rmtree, rmdir=Path.rmdir)


def stop_sampler(rt: Runtime) -> None:
    """Stop the sampler thread and wait for it."""
    rt.stop.set()
    if rt.thread is not None:
        rt.thread.join()


def teardown(rt: Runtime, warden: Path, ops: Ops) -> list[str]:
    """Undo a run, step by step. Each step runs even if an earlier one failed.

    Order: builds are killed first, then the sampler stops, then the sccache server stops,
    then the worktrees are unregistered, then the run dir goes, then the SSD dir if the
    run created it (and only if it is empty). Returns one message per failed step.
    """
    steps: list[tuple[str, Callable[[], None]]] = []
    steps += [(f"kill build {p.pid}", lambda p=p: ops.kill(p)) for p in rt.procs]
    steps.append(("stop sampler", lambda: stop_sampler(rt)))
    if rt.sccache_env is not None:
        env = rt.sccache_env
        steps.append(("stop sccache server", lambda: ops.run(["sccache", "--stop-server"], env)))
    steps += [
        (
            f"remove worktree {w}",
            lambda w=w: ops.run(
                ["git", "-C", str(warden), "worktree", "remove", "--force", str(w)], None
            ),
        )
        for w in rt.worktrees
    ]
    if rt.run_dir is not None:
        run_dir = rt.run_dir
        steps.append((f"remove run dir {run_dir}", lambda: ops.rmtree(run_dir)))
    if rt.created_ssd_dir is not None:
        ssd_dir = rt.created_ssd_dir
        steps.append((f"remove ssd dir {ssd_dir}", lambda: ops.rmdir(ssd_dir)))
    errors = []
    for name, step in steps:
        try:
            step()
        except Exception as error:  # noqa: BLE001 - report and carry on with the other steps
            errors.append(f"cleanup: {name} failed: {type(error).__name__}: {error}")
    return errors


class Interrupted(SystemExit):
    """Raised from SIGTERM and SIGHUP so ``finally`` blocks run."""


def raise_interrupted(signum: int, _frame: FrameType | None) -> None:
    """Signal handler: turn the signal into an exception."""
    raise Interrupted(128 + signum)


def free_port() -> int:
    """Return a currently free local TCP port, for the run's own sccache server."""
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def run_builds(
    plan: Plan, rt: Runtime, shim_dir: Path, ops: Ops
) -> tuple[float, float, list[int], list[Sample], list[str]]:
    """Create worktrees and build in all of them at once.

    Returns the start, the wall time, the exit codes, the samples and the sampler errors.
    Everything created is recorded in ``rt`` as it is created.
    """
    env = build_env(plan, os.environ, shim_dir)
    plan.log_dir.mkdir(parents=True, exist_ok=True)
    if plan.sccache == "cold":
        rt.sccache_env = sccache_env(plan, clean_env(os.environ))
        ops.run(["sccache", "--start-server"], rt.sccache_env)
    for i in range(plan.builds):
        worktree = plan.worktree(i)
        add = ["git", "-C", str(plan.warden), "worktree", "add", "--detach", str(worktree), "HEAD"]
        ops.run(add, None)
        rt.worktrees.append(worktree)
    samples: list[Sample] = []
    errors: list[str] = []
    rt.thread = threading.Thread(target=sampler, args=(rt.stop, samples, errors), daemon=True)
    rt.thread.start()
    start = time.monotonic()
    for i in range(plan.builds):
        with plan.log_file(i).open("wb") as log:
            rt.procs.append(
                subprocess.Popen(
                    build_command(plan, i),
                    cwd=plan.worktree(i),
                    env=env,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
            )
    codes = [proc.wait() for proc in rt.procs]
    wall = time.monotonic() - start
    stop_sampler(rt)
    return start, wall, codes, samples, errors


def warden_state(warden: Path) -> dict[str, object]:
    """Return the commit that gets built and whether the checkout is dirty."""
    return {
        "warden_commit": git(warden, "rev-parse", "HEAD"),
        "warden_dirty": bool(git(warden, "status", "--porcelain")),
    }


def execute(args: argparse.Namespace, rt: Runtime, ops: Ops) -> int:
    """Prepare the directories, run the builds, write the summary. Returns the exit code."""
    ssd_dir = args.ssd_dir
    if not ssd_dir.parent.is_dir():
        sys.stderr.write(f"{ssd_dir.parent} does not exist; is the SSD mounted?\n")
        return 2
    problem = ssd_problem(ssd_dir, stat_device)
    if problem:
        sys.stderr.write(problem + "\n")
        return 2
    if not ssd_dir.exists():
        ssd_dir.mkdir()
        rt.created_ssd_dir = ssd_dir
    rt.run_dir = Path(tempfile.mkdtemp(prefix="run-", dir=ssd_dir))
    plan = make_plan(args, rt.run_dir, free_port() if args.sccache == "cold" else 0)
    state = warden_state(plan.warden)
    start, wall, codes, samples, errors = run_builds(plan, rt, args.shim_dir, ops)
    summary = {
        "mode": plan.mode,
        "builds": plan.builds,
        "sccache": plan.sccache,
        "log_dir": str(plan.log_dir),
        **state,
        **summarise(samples, start, wall, codes, errors),
    }
    text = json.dumps(summary, indent=2)
    sys.stdout.write(text + "\n")
    if args.output:
        args.output.write_text(text + "\n")
    return 0 if summary["all_passed"] and summary["samples_complete"] else 1


def main(argv: list[str]) -> int:
    """Entry point."""
    args = parse_args(argv)
    if args.dry_run:
        plan = make_plan(args, args.ssd_dir / "run-XXXXXXXX", 0)
        note = ssd_problem(args.ssd_dir, stat_device) or "ok, not on the internal disk"
        sys.stdout.write(describe(plan, note) + "\n")
        return 0
    signal.signal(signal.SIGTERM, raise_interrupted)
    signal.signal(signal.SIGHUP, raise_interrupted)
    rt = Runtime()
    ops = real_ops()
    code = 1
    try:
        code = execute(args, rt, ops)
    finally:
        errors = teardown(rt, args.warden, ops)
        for message in errors:
            sys.stderr.write(message + "\n")
    if errors and code == 0:
        return 3
    return code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
