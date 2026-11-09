"""Tests for the pure parts of the acceptance script."""

import tempfile
import threading
import unittest
from pathlib import Path

import run

GIB = 1024 * run.MIB


def make_plan(argv: list[str] | None = None) -> run.Plan:
    """Return a plan under a fixed run dir."""
    args = run.parse_args(["--mode", "baseline", "--ssd-dir", "/ssd", *(argv or [])])
    return run.make_plan(args, Path("/ssd/run-abc"), 4242)


class FakeProc:
    """A stand-in for a running build."""

    def __init__(self, pid: int) -> None:
        """Remember the pid."""
        self.pid = pid


class ParseArgsTests(unittest.TestCase):
    """Argument parsing and the plan."""

    def test_defaults(self) -> None:
        """Five builds, cold sccache, every path under the run dir."""
        plan = make_plan()
        self.assertEqual(plan.builds, 5)
        self.assertEqual(plan.sccache, "cold")
        self.assertEqual(plan.ssd_dir, Path("/ssd"))
        self.assertEqual(plan.target_dir(2), Path("/ssd/run-abc/target-2"))
        self.assertEqual(plan.build_dir(2), Path("/ssd/run-abc/build-2"))

    def test_mode_is_required(self) -> None:
        """No mode is an error."""
        with self.assertRaises(SystemExit):
            run.parse_args(["--ssd-dir", "/ssd"])

    def test_ssd_dir_is_required(self) -> None:
        """No default disk: the SSD dir must be given."""
        with self.assertRaises(SystemExit):
            run.parse_args(["--mode", "baseline"])

    def test_zero_builds_rejected(self) -> None:
        """At least one build."""
        with self.assertRaises(SystemExit):
            run.parse_args(["--mode", "damper", "--ssd-dir", "/ssd", "--builds", "0"])

    def test_log_dir_defaults_next_to_output(self) -> None:
        """Logs are kept with the JSON."""
        plan = make_plan(["--output", "/x/base.json"])
        self.assertEqual(plan.log_dir, Path("/x/base-logs"))
        self.assertEqual(make_plan(["--log-dir", "/l"]).log_dir, Path("/l"))

    def test_dry_run_prints_plan(self) -> None:
        """The description names the mode and every build."""
        args = run.parse_args(["--mode", "damper", "--ssd-dir", "/ssd", "--builds", "2"])
        text = run.describe(run.make_plan(args, Path("/ssd/run-abc"), 0), "ok")
        self.assertIn("mode: damper", text)
        self.assertIn("build 1:", text)
        self.assertNotIn("build 2:", text)


class BuildPlanTests(unittest.TestCase):
    """Per-build command lines and environment."""

    def test_cold_command_has_per_build_configs(self) -> None:
        """Build dir, wrapper and forced sccache env go in as flags."""
        cmd = run.build_command(make_plan(), 1)
        self.assertEqual(cmd[:4], ["cargo", "build", "--release", "--locked"])
        self.assertIn("/ssd/run-abc/target-1", cmd)
        self.assertIn('build.build-dir="/ssd/run-abc/build-1"', cmd)
        self.assertIn('build.rustc-wrapper="sccache"', cmd)
        self.assertIn('env.SCCACHE_DIR="/ssd/run-abc/sccache"', cmd)
        self.assertIn('env.SCCACHE_SERVER_PORT="4242"', cmd)

    def test_config_env_values_are_plain_strings(self) -> None:
        """Cargo rejects inline tables and can't merge a table onto a string key."""
        cmd = run.build_command(make_plan(), 0)
        values = [cmd[i + 1] for i, arg in enumerate(cmd) if arg == "--config"]
        env_values = [v for v in values if v.startswith("env.")]
        self.assertEqual(len(env_values), 2)
        for value in values:
            key, _, rhs = value.partition("=")
            self.assertEqual(key.count("."), 1, value)
            self.assertTrue(rhs.startswith('"'), value)

    def test_off_command_clears_wrapper(self) -> None:
        """No wrapper and no sccache env."""
        cmd = run.build_command(make_plan(["--sccache", "off"]), 0)
        self.assertIn('build.rustc-wrapper=""', cmd)
        self.assertFalse(any("SCCACHE" in part for part in cmd))

    def test_paths_with_spaces_stay_one_argument(self) -> None:
        """Quoting is by TOML string, not by a shell."""
        args = run.parse_args(["--mode", "baseline", "--ssd-dir", "/Volumes/My SSD"])
        cmd = run.build_command(run.make_plan(args, Path("/Volumes/My SSD/r"), 1), 0)
        self.assertIn('build.build-dir="/Volumes/My SSD/r/build-0"', cmd)

    def test_env_strips_and_sets(self) -> None:
        """Make flags and wrapper go; CARGO_HOME stays; sccache gets its own dir."""
        inherited = {
            "MAKEFLAGS": "-j3",
            "MFLAGS": "x",
            "CARGO_MAKEFLAGS": "y",
            "RUSTC_WRAPPER": "z",
            "SCCACHE_DIR": "/old",
            "CARGO_HOME": "/ch",
            "PATH": "/bin",
        }
        env = run.build_env(make_plan(), inherited, Path("/shim"))
        for name in ("MAKEFLAGS", "MFLAGS", "CARGO_MAKEFLAGS", "RUSTC_WRAPPER"):
            self.assertNotIn(name, env)
        self.assertEqual(env["CARGO_HOME"], "/ch")
        self.assertEqual(env["SCCACHE_DIR"], "/ssd/run-abc/sccache")
        self.assertEqual(env["SCCACHE_SERVER_PORT"], "4242")
        self.assertEqual(env["DAMPER"], "off")
        self.assertEqual(env["PATH"], "/bin")

    def test_damper_mode_puts_shim_first(self) -> None:
        """The shim dir leads PATH; DAMPER is not set."""
        args = run.parse_args(["--mode", "damper", "--ssd-dir", "/ssd", "--sccache", "off"])
        plan = run.make_plan(args, Path("/r"), 0)
        env = run.build_env(plan, {"PATH": "/bin"}, Path("/shim"))
        self.assertTrue(env["PATH"].startswith("/shim:"))
        self.assertNotIn("DAMPER", env)
        self.assertNotIn("SCCACHE_DIR", env)


class SsdTests(unittest.TestCase):
    """The internal-disk refusal."""

    def test_same_device_as_internal_root_refused(self) -> None:
        """A missing dir is judged by its nearest existing parent."""
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "new" / "dir"
            self.assertIn("internal disk", run.ssd_problem(path, lambda _p: 1, [Path(tmp)]))
            devices = {Path(tmp).resolve(): 7}
            dev_of = lambda p: devices.get(p.resolve(), 1)  # noqa: E731
            self.assertIsNone(run.ssd_problem(path, dev_of, [Path("/")]))


class CleanupTests(unittest.TestCase):
    """Teardown order and independence."""

    def make_ops(self, log: list[str], fail: str | None = None) -> run.Ops:
        """Ops that record what they are asked, and raise for ``fail``."""

        def record(name: str) -> None:
            log.append(name)
            if name == fail:
                raise OSError(name)

        return run.Ops(
            run=lambda argv, _env: record(" ".join(argv[:2]) + " " + argv[-1]),
            kill=lambda p: record(f"kill {p.pid}"),
            rmtree=lambda p: record(f"rmtree {p}"),
            rmdir=lambda p: record(f"rmdir {p}"),
        )

    def runtime(self) -> run.Runtime:
        """Return a runtime with two builds, a server, two worktrees and both dirs."""
        rt = run.Runtime()
        rt.procs = [FakeProc(1), FakeProc(2)]
        rt.worktrees = [Path("/r/w0"), Path("/r/w1")]
        rt.sccache_env = {"SCCACHE_DIR": "/r/sccache"}
        rt.run_dir = Path("/r")
        rt.created_ssd_dir = Path("/s")
        return rt

    def test_order(self) -> None:
        """Builds die first, the server stops, worktrees go, then dirs."""
        log: list[str] = []
        errors = run.teardown(self.runtime(), Path("/w"), self.make_ops(log))
        self.assertEqual(errors, [])
        self.assertEqual(
            log,
            [
                "kill 1",
                "kill 2",
                "sccache --stop-server --stop-server",
                "git -C /r/w0",
                "git -C /r/w1",
                "rmtree /r",
                "rmdir /s",
            ],
        )

    def test_failure_does_not_stop_other_steps(self) -> None:
        """A failed kill is reported and the rest still run."""
        log: list[str] = []
        errors = run.teardown(self.runtime(), Path("/w"), self.make_ops(log, fail="kill 1"))
        self.assertEqual(len(errors), 1)
        self.assertIn("kill build 1", errors[0])
        self.assertEqual(log[-2:], ["rmtree /r", "rmdir /s"])
        self.assertIn("kill 2", log)

    def test_nothing_created_nothing_removed(self) -> None:
        """An empty runtime touches nothing on disk."""
        log: list[str] = []
        self.assertEqual(run.teardown(run.Runtime(), Path("/w"), self.make_ops(log)), [])
        self.assertEqual(log, [])


class SamplerTests(unittest.TestCase):
    """Sampler failures are recorded."""

    def test_error_is_recorded(self) -> None:
        """A failing read stops sampling and lands in the error list."""
        calls = []

        def take() -> run.Sample:
            calls.append(1)
            if len(calls) > 1:
                message = "boom"
                raise ValueError(message)
            return run.Sample(1, 0, 1.0, [0.001])

        out: list[run.Sample] = []
        errors: list[str] = []
        run.sampler(threading.Event(), out, errors, take)
        self.assertEqual(len(out), 1)
        self.assertEqual(errors, ["ValueError: boom"])
        summary = run.summarise(out, 0.0, 1.0, [0], errors)
        self.assertFalse(summary["samples_complete"])

    def test_no_samples_is_incomplete(self) -> None:
        """Zero samples never counts as complete."""
        self.assertFalse(run.summarise([], 0.0, 1.0, [0], [])["samples_complete"])


class MathTests(unittest.TestCase):
    """Summary maths."""

    def test_percentile_nearest_rank(self) -> None:
        """p50 and p99 of 1..100."""
        values = [float(i) for i in range(1, 101)]
        self.assertEqual(run.percentile(values, 0.50), 50.0)
        self.assertEqual(run.percentile(values, 0.99), 99.0)
        self.assertEqual(run.percentile([3.0], 0.99), 3.0)

    def test_critical_stats(self) -> None:
        """Real deltas: uneven gaps count by their length."""
        times = [1.0, 2.0, 4.0, 5.0, 6.5, 7.0]
        levels = [1, 4, 4, 2, 4, 1]
        samples = [run.Sample(lv, 0, t, []) for lv, t in zip(levels, times, strict=True)]
        self.assertEqual(run.critical_stats(samples, 0.0), (4.5, 3.0))
        self.assertEqual(run.critical_stats([], 0.0), (0.0, 0.0))

    def test_parse_swap_used(self) -> None:
        """Real vm.swapusage output."""
        text = "total = 4096.00M  used = 2461.25M  free = 1634.75M  (encrypted)"
        self.assertEqual(run.parse_swap_used(text), int(2461.25 * run.MIB))
        self.assertEqual(run.parse_swap_used("used = 1.50G"), int(1.5 * GIB))
        with self.assertRaises(ValueError):
            run.parse_swap_used("nothing")

    def test_summarise(self) -> None:
        """Peaks, critical time, pooled percentiles and pass flag."""
        samples = [
            run.Sample(1, GIB, 1.0, [0.001] * 99 + [0.002]),
            run.Sample(4, 3 * GIB, 2.0, [0.001] * 99 + [0.050]),
        ]
        out = run.summarise(samples, 0.0, 12.34, [0, 0], [])
        self.assertEqual(out["swap_peak_mib"], 3072)
        self.assertEqual(out["seconds_at_critical"], 1.0)
        self.assertEqual(out["probe_max_ms"], 50.0)
        self.assertEqual(out["probe_p50_ms"], 1.0)
        # Pooled nearest-rank p99 of 200 values is the 198th: 2 ms, not the worst second's p99.
        self.assertEqual(out["probe_p99_ms"], 1.0)
        self.assertTrue(out["all_passed"])
        self.assertTrue(out["samples_complete"])
        self.assertFalse(run.summarise(samples, 0.0, 1.0, [0, 2], [])["all_passed"])


if __name__ == "__main__":
    unittest.main()
