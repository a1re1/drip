#!/usr/bin/env python3
"""Unit tests for evals/speed/bench.py summarize_runs. Stdlib only."""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
import importlib.util

_spec = importlib.util.spec_from_file_location(
    "bench", os.path.join(os.path.dirname(os.path.abspath(__file__)), "bench.py"))
bench = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bench)


def run(task, wall_s, inferences, hidden_pass, rejections):
    return dict(task=task, wall_s=wall_s, inferences=inferences,
                hidden_pass=hidden_pass, rejections=rejections)


class SummarizeRunsTest(unittest.TestCase):
    def test_empty_input_returns_empty_list(self):
        self.assertEqual(bench.summarize_runs([]), [])

    def test_odd_count_median(self):
        rows = bench.summarize_runs([run("a", 10.0, 5, True, 1),
                                     run("a", 20.0, 7, True, 2),
                                     run("a", 30.0, 9, False, 0)])
        self.assertEqual(len(rows), 1)
        s = rows[0]
        self.assertEqual(s["task"], "a")
        self.assertEqual(s["runs"], 3)
        self.assertEqual(s["wall_s_median"], 20.0)
        self.assertEqual(s["wall_s_min"], 10.0)
        self.assertEqual(s["wall_s_max"], 30.0)
        self.assertEqual(s["inferences_median"], 7)
        self.assertAlmostEqual(s["hidden_pass_rate"], 2 / 3)
        self.assertEqual(s["rejections"], 3)

    def test_even_count_median_is_fractional(self):
        s = bench.summarize_runs([run("a", 10.0, 4, True, 0),
                                  run("a", 30.0, 8, True, 0)])[0]
        self.assertEqual(s["wall_s_median"], 20.0)
        self.assertEqual(s["inferences_median"], 6)
        self.assertEqual(s["hidden_pass_rate"], 1.0)

    def test_multiple_tasks_sorted_with_failed_hidden_test(self):
        rows = bench.summarize_runs([
            run("b", 5.0, 3, False, 2),
            run("a", 7.0, 4, True, 0),
            run("a", 9.0, 6, False, 3),
        ])
        self.assertEqual([s["task"] for s in rows], ["a", "b"])
        a, b = rows
        self.assertEqual(a["runs"], 2)
        self.assertEqual(a["hidden_pass_rate"], 0.5)
        self.assertEqual(a["rejections"], 3)
        self.assertEqual(a["inferences_median"], 5)
        self.assertEqual(b["hidden_pass_rate"], 0.0)
        self.assertEqual(b["rejections"], 2)

    def test_input_not_mutated(self):
        runs = [run("a", 10.0, 5, True, 1), run("a", 20.0, 7, False, 2)]
        snapshot = [dict(r) for r in runs]
        bench.summarize_runs(runs)
        self.assertEqual(runs, snapshot)


if __name__ == "__main__":
    unittest.main()
