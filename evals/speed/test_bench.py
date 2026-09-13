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


def wall_delta_pct(a_row, b_row):
    """Delta the compare() table prints for a task present on both sides."""
    return (b_row["wall_s_median"] - a_row["wall_s_median"]) / a_row["wall_s_median"]


class CompareDeltaTest(unittest.TestCase):
    def test_compare_delta_between_two_labels(self):
        runs_a = [run("t1", 10.0, 4, True, 0), run("t2", 40.0, 8, True, 1)]
        runs_b = [run("t1", 15.0, 6, True, 0), run("t2", 30.0, 6, False, 0)]
        ra = {r["task"]: r for r in bench.summarize_runs(runs_a)}
        rb = {r["task"]: r for r in bench.summarize_runs(runs_b)}
        self.assertAlmostEqual(wall_delta_pct(ra["t1"], rb["t1"]), 0.5)   # 10 -> 15
        self.assertAlmostEqual(wall_delta_pct(ra["t2"], rb["t2"]), -0.25) # 40 -> 30

    def test_compare_missing_task_is_dash(self):
        rows_a = bench.summarize_runs([run("t1", 10.0, 4, True, 0)])
        rows_b = bench.summarize_runs([run("t2", 20.0, 5, True, 0)])
        tasks = sorted({r["task"] for r in rows_a} | {r["task"] for r in rows_b})
        self.assertEqual(tasks, ["t1", "t2"])
        self.assertIsNone({r["task"]: r for r in rows_a}.get("t2"))
        self.assertIsNone({r["task"]: r for r in rows_b}.get("t1"))


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

class RoleTimingTest(unittest.TestCase):
    @staticmethod
    def role_run(task, plan_s, author_s, review_s, **kw):
        return dict(run(task, 10.0, 5, True, 1), plan_s=plan_s, author_s=author_s, review_s=review_s, **kw)

    def test_summarize_runs_with_role_timings(self):
        rows = bench.summarize_runs([self.role_run("a", 1.0, 4.0, 2.0),
                                     self.role_run("a", 3.0, 6.0, 4.0)])
        s = rows[0]
        self.assertEqual(s["plan_s_median"], 2.0)
        self.assertEqual(s["author_s_median"], 5.0)
        self.assertEqual(s["review_s_median"], 3.0)

    def test_summarize_runs_legacy_without_role_fields_yields_zero(self):
        rows = bench.summarize_runs([run("a", 10.0, 5, True, 1), run("a", 20.0, 7, True, 2)])
        s = rows[0]
        self.assertEqual(s["plan_s_median"], 0)
        self.assertEqual(s["author_s_median"], 0)
        self.assertEqual(s["review_s_median"], 0)

    def test_extract_role_inference_last_run_end_wins(self):
        stdout = ('{"type":"x"}\n'
                  '{"type":"run-end","roleInference":{"planner":{"calls":1,"latencyMs":1500},'
                  '"author":{"calls":3,"latencyMs":2300}}}\n')
        self.assertEqual(bench.extract_role_inference(stdout),
                         {"planner": {"calls": 1, "latencyMs": 1500},
                          "author": {"calls": 3, "latencyMs": 2300}})

    def test_extract_role_inference_absent_or_garbage(self):
        self.assertEqual(bench.extract_role_inference('{"type":"result"}\nnot json\n'), {})
        self.assertEqual(bench.extract_role_inference(""), {})

    def test_role_seconds_mapping_and_defaults(self):
        self.assertEqual(bench.role_seconds({"planner": {"latencyMs": 1500},
                                             "author": {"latencyMs": 2300},
                                             "reviewer": {"latencyMs": 900}}),
                         dict(plan_s=1.5, author_s=2.3, review_s=0.9))
        self.assertEqual(bench.role_seconds({}), dict(plan_s=0, author_s=0, review_s=0))

    def test_input_not_mutated(self):
        runs = [run("a", 10.0, 5, True, 1), run("a", 20.0, 7, False, 2)]
        snapshot = [dict(r) for r in runs]
        bench.summarize_runs(runs)
        self.assertEqual(runs, snapshot)


class ReviewWaivedTest(unittest.TestCase):
    def test_transcript_metrics_counts_review_waived_events(self):
        import tempfile
        lines = [
            '{"type":"event","kind":"harness-op","detail":"review waived: cosmetic nit"}',
            '{"type":"event","kind":"harness-op","detail":"review waived: deferred P2"}',
            '{"type":"event","kind":"harness-op","detail":"review waiver: not a waived event"}',
            '{"type":"event","kind":"harness-op","detail":"hedge resolved: the second request"}',
           ]
        with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False) as f:
            f.write("\n".join(lines) + "\n")
            path = f.name
        try:
            m = bench.transcript_metrics(path)
        finally:
            os.unlink(path)
        self.assertEqual(m["review_waived"], 2)

    def test_summarize_runs_sums_review_waived(self):
        runs = [run("a", 10.0, 5, True, 0), dict(run("a", 20.0, 7, True, 1), review_waived=3),
                dict(run("a", 30.0, 9, False, 0), review_waived=4)]
        s = bench.summarize_runs(runs)[0]
        self.assertEqual(s["review_waived"], 7)

    def test_summarize_runs_legacy_without_review_waived_yields_zero(self):
        s = bench.summarize_runs([run("a", 10.0, 5, True, 1)])[0]
        self.assertEqual(s["review_waived"], 0)


if __name__ == "__main__":
    unittest.main()
