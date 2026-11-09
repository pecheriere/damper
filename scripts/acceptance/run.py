"""Acceptance run: five cold release builds of warden at once, with the machine sampled.

Run with ``python3 -I scripts/acceptance/run.py``. It creates one git worktree of the
warden checkout per build, builds each with its own cold ``--target-dir`` on the SSD,
samples memory pressure, swap and a 10 ms sleep probe every second, and prints a JSON
summary. Worktrees and target directories are deleted afterwards.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

CRITICAL_LEVEL = 4
PROBE_SLEEP = 0.010
MIB = 1024 * 1024


@dataclass
class Sample:
    """One second of machine state."""

    pressure: int
    swap_used: int
    probe_p50: float
    probe_p99: float
    probe_max: float


@dataclass
class Plan:
    """What a run will do."""

    mode: str
    warden: Path
    builds: int
    ssd_dir: Path
    worktrees: list[Path] = field(default_factory=list)
    target_dirs: list[Path] = field(default_factory=list)


def parse_args(argv: list[str]) -> argparse.Namespace:
    """Parse the command line."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--mode", choices=["baseline", "damper"], required=True)
    parser.add_argument("--warden", type=Path, default=Path.home() / "Code" / "warden")
    parser.add_argument("--builds", type=positive_int, default=5)
    parser.add_argument(
        "--ssd-dir",
        type=Path,
        default=Path("/Volumes/SSD/damper-acceptance"),
        help="where worktrees' target dirs go; must be on the external SSD",
    )
    parser.add_argument(
        "--shim-dir",
        type=Path,
        default=Path.home() / ".local" / "share" / "damper" / "shim",
        help="directory holding the damper shim named cargo (damper mode)",
    )
    parser.add_argument("--output", type=Path, help="write the JSON summary here too")
    parser.add_argument("--dry-run", action="store_true", help="print the plan, build nothing")
    return parser.parse_args(argv)


def positive_int(text: str) -> int:
    """Argparse type for an integer of at least one."""
    value = int(text)
    if value < 1:
        message = "must be at least 1"
        raise argparse.ArgumentTypeError(message)
    return value


def make_plan(args: argparse.Namespace) -> Plan:
    """Work out the paths a run uses."""
    plan = Plan(args.mode, args.warden, args.builds, args.ssd_dir)
    for i in range(args.builds):
        plan.worktrees.append(args.ssd_dir / f"worktree-{i}")
        plan.target_dirs.append(args.ssd_dir / f"target-{i}")
    return plan


def describe(plan: Plan) -> str:
    """Human-readable plan, printed by ``--dry-run``."""
    lines = [
        f"mode: {plan.mode}",
        f"warden: {plan.warden}",
        f"builds: {plan.builds} x cargo build --release --locked, cold target dirs",
    ]
    lines += [
        f"  build {i}: worktree {w}, target {t}"
        for i, (w, t) in enumerate(zip(plan.worktrees, plan.target_dirs, strict=True))
    ]
    lines.append("sampling: 1 s, pressure level, swap, 10 ms sleep probe")
    lines.append("cleanup: worktrees and target dirs are deleted afterwards")
    return "\n".join(lines)


def percentile(values: list[float], fraction: float) -> float:
    """Nearest-rank percentile of a non-empty list."""
    ordered = sorted(values)
    rank = max(1, math.ceil(fraction * len(ordered)))
    return ordered[rank - 1]


def critical_stats(levels: list[int], interval: float = 1.0) -> tuple[float, float]:
    """Total seconds at critical, and the longest consecutive stretch."""
    total = 0.0
    longest = 0.0
    run = 0.0
    for level in levels:
        if level >= CRITICAL_LEVEL:
            total += interval
            run += interval
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


def summarise(samples: list[Sample], wall: float, exit_codes: list[int]) -> dict[str, object]:
    """Build the JSON summary of a run."""
    total, longest = critical_stats([s.pressure for s in samples])
    return {
        "wall_seconds": round(wall, 1),
        "exit_codes": exit_codes,
        "all_passed": all(code == 0 for code in exit_codes),
        "samples": len(samples),
        "seconds_at_critical": total,
        "longest_critical_stretch_seconds": longest,
        "swap_peak_mib": max((s.swap_used for s in samples), default=0) // MIB,
        "probe_p50_ms": round(statistics.median(s.probe_p50 for s in samples) * 1000, 2)
        if samples
        else None,
        "probe_p99_ms": round(max((s.probe_p99 for s in samples), default=0.0) * 1000, 2),
        "probe_max_ms": round(max((s.probe_max for s in samples), default=0.0) * 1000, 2),
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
        probe_p50=percentile(lateness, 0.50),
        probe_p99=percentile(lateness, 0.99),
        probe_max=max(lateness),
    )


def sampler(stop: threading.Event, out: list[Sample]) -> None:
    """Sample every second until ``stop`` is set."""
    while not stop.is_set():
        out.append(sample_once())


def run_git(warden: Path, *args: str) -> None:
    """Run git in the warden checkout."""
    subprocess.run(["git", "-C", str(warden), *args], check=True, capture_output=True)


def build_env(plan: Plan) -> dict[str, str]:
    """Build the environment for a build: damper off in baseline mode."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("CARGO_")}
    if plan.mode == "baseline":
        env["DAMPER"] = "off"
    return env


def run_builds(plan: Plan, shim_dir: Path) -> tuple[float, list[int], list[Sample]]:
    """Create worktrees, build in all of them at once and return wall time, codes, samples."""
    env = build_env(plan)
    if plan.mode == "damper":
        env["PATH"] = f"{shim_dir}{os.pathsep}{env['PATH']}"
    samples: list[Sample] = []
    stop = threading.Event()
    thread = threading.Thread(target=sampler, args=(stop, samples), daemon=True)
    for worktree in plan.worktrees:
        run_git(plan.warden, "worktree", "add", "--detach", str(worktree), "HEAD")
    thread.start()
    start = time.monotonic()
    procs = [
        subprocess.Popen(
            ["cargo", "build", "--release", "--locked", "--target-dir", str(target)],
            cwd=worktree,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        for worktree, target in zip(plan.worktrees, plan.target_dirs, strict=True)
    ]
    codes = [proc.wait() for proc in procs]
    wall = time.monotonic() - start
    stop.set()
    thread.join()
    return wall, codes, samples


def cleanup(plan: Plan) -> None:
    """Delete the worktrees and target dirs."""
    for worktree in plan.worktrees:
        if worktree.exists():
            run_git(plan.warden, "worktree", "remove", "--force", str(worktree))
    for target in plan.target_dirs:
        shutil.rmtree(target, ignore_errors=True)


def main(argv: list[str]) -> int:
    """Entry point."""
    args = parse_args(argv)
    plan = make_plan(args)
    if args.dry_run:
        sys.stdout.write(describe(plan) + "\n")
        return 0
    if not plan.ssd_dir.parent.is_dir():
        sys.stderr.write(f"{plan.ssd_dir.parent} does not exist; is the SSD mounted?\n")
        return 2
    plan.ssd_dir.mkdir(exist_ok=True)
    try:
        wall, codes, samples = run_builds(plan, args.shim_dir)
    finally:
        cleanup(plan)
    summary = {"mode": plan.mode, "builds": plan.builds, **summarise(samples, wall, codes)}
    text = json.dumps(summary, indent=2)
    sys.stdout.write(text + "\n")
    if args.output:
        args.output.write_text(text + "\n")
    return 0 if summary["all_passed"] else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
