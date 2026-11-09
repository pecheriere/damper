"""Tests for the pure parts of the acceptance script."""

import unittest

import run

GIB = 1024 * run.MIB


class ParseArgsTests(unittest.TestCase):
    """Argument parsing and the plan."""

    def test_defaults(self) -> None:
        """Five builds, paths derived from the SSD dir."""
        args = run.parse_args(["--mode", "baseline"])
        plan = run.make_plan(args)
        self.assertEqual(plan.builds, 5)
        self.assertEqual(len(plan.worktrees), 5)
        self.assertEqual(plan.target_dirs[2], plan.ssd_dir / "target-2")

    def test_mode_is_required(self) -> None:
        """No mode is an error."""
        with self.assertRaises(SystemExit):
            run.parse_args([])

    def test_zero_builds_rejected(self) -> None:
        """At least one build."""
        with self.assertRaises(SystemExit):
            run.parse_args(["--mode", "damper", "--builds", "0"])

    def test_dry_run_prints_plan(self) -> None:
        """The description names the mode and every build."""
        args = run.parse_args(["--mode", "damper", "--builds", "2", "--dry-run"])
        text = run.describe(run.make_plan(args))
        self.assertIn("mode: damper", text)
        self.assertIn("build 1:", text)
        self.assertNotIn("build 2:", text)


class MathTests(unittest.TestCase):
    """Summary maths."""

    def test_percentile_nearest_rank(self) -> None:
        """p50 and p99 of 1..100."""
        values = [float(i) for i in range(1, 101)]
        self.assertEqual(run.percentile(values, 0.50), 50.0)
        self.assertEqual(run.percentile(values, 0.99), 99.0)
        self.assertEqual(run.percentile([3.0], 0.99), 3.0)

    def test_critical_stats(self) -> None:
        """Total and longest consecutive stretch at level 4."""
        levels = [1, 4, 4, 2, 4, 4, 4, 1]
        self.assertEqual(run.critical_stats(levels), (5.0, 3.0))
        self.assertEqual(run.critical_stats([]), (0.0, 0.0))

    def test_parse_swap_used(self) -> None:
        """Real vm.swapusage output."""
        text = "total = 4096.00M  used = 2461.25M  free = 1634.75M  (encrypted)"
        self.assertEqual(run.parse_swap_used(text), int(2461.25 * run.MIB))
        self.assertEqual(run.parse_swap_used("used = 1.50G"), int(1.5 * GIB))
        with self.assertRaises(ValueError):
            run.parse_swap_used("nothing")

    def test_summarise(self) -> None:
        """Peaks, critical time and pass flag."""
        samples = [
            run.Sample(1, GIB, 0.005, 0.006, 0.007),
            run.Sample(4, 3 * GIB, 0.005, 0.020, 0.050),
        ]
        out = run.summarise(samples, 12.34, [0, 0])
        self.assertEqual(out["swap_peak_mib"], 3072)
        self.assertEqual(out["seconds_at_critical"], 1.0)
        self.assertEqual(out["probe_max_ms"], 50.0)
        self.assertTrue(out["all_passed"])
        self.assertFalse(run.summarise(samples, 1.0, [0, 2])["all_passed"])


if __name__ == "__main__":
    unittest.main()
