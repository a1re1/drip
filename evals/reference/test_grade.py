"""Unit tests for the benchmark's pure layers — grade.py, retrieval_eval.py's
parsing/scoring/aggregation helpers, and corpus.py's root resolution.

Pure stdlib: no corpus, no oasis, no drip, no network. Anything that shells
out lives behind a __main__ guard in the harness scripts and is not covered
here."""

import ast
import inspect
import json
import unittest

import corpus
import grade
import retrieval_eval


class RecallAtKTests(unittest.TestCase):
    def test_counts_unique_hits_in_top_k(self):
        ranked = ["a.md", "b.md", "c.md"]
        self.assertEqual(grade.recall_at_k(ranked, ["a.md", "b.md"], 3), 1.0)
        self.assertEqual(grade.recall_at_k(ranked, ["a.md", "b.md"], 1), 0.5)
        self.assertEqual(grade.recall_at_k(ranked, ["a.md", "b.md", "z.md"], 3), 2 / 3)

    def test_empty_ranking_or_relevant_is_zero(self):
        self.assertEqual(grade.recall_at_k([], ["a.md"], 5), 0.0)
        self.assertEqual(grade.recall_at_k(["a.md"], [], 5), 0.0)

    def test_k_zero_or_negative_is_zero(self):
        self.assertEqual(grade.recall_at_k(["a.md"], ["a.md"], 0), 0.0)
        self.assertEqual(grade.recall_at_k(["a.md"], ["a.md"], -1), 0.0)

    def test_duplicate_hits_deduplicate_in_numerator_without_compacting(self):
        # A duplicate hit must not inflate the numerator; position 3 is still
        # outside the top-2 even though "a.md" appears twice in it.
        ranked = ["a.md", "a.md", "b.md"]
        self.assertEqual(grade.recall_at_k(ranked, ["a.md", "b.md"], 2), 0.5)
        self.assertEqual(grade.recall_at_k(ranked, ["a.md", "b.md"], 3), 1.0)

    def test_duplicate_relevant_pages_count_once(self):
        self.assertEqual(grade.recall_at_k(["a.md"], ["a.md", "a.md"], 1), 1.0)

    def test_k_larger_than_ranking(self):
        self.assertEqual(grade.recall_at_k(["a.md"], ["a.md", "b.md"], 10), 0.5)


class ReciprocalRankTests(unittest.TestCase):
    def test_first_relevant_rank(self):
        self.assertEqual(grade.reciprocal_rank(["a.md", "b.md"], ["b.md"]), 0.5)
        self.assertEqual(grade.reciprocal_rank(["a.md", "b.md"], ["a.md"]), 1.0)
        self.assertEqual(grade.reciprocal_rank(["a.md", "b.md", "c.md"], ["c.md"]), 1 / 3)

    def test_miss_is_zero(self):
        self.assertEqual(grade.reciprocal_rank(["a.md"], ["b.md"]), 0.0)

    def test_empty_inputs_are_zero(self):
        self.assertEqual(grade.reciprocal_rank([], ["a.md"]), 0.0)
        self.assertEqual(grade.reciprocal_rank(["a.md"], []), 0.0)

    def test_duplicate_relevant_pages(self):
        self.assertEqual(grade.reciprocal_rank(["a.md", "b.md"], ["b.md", "b.md"]), 0.5)


class SlugTests(unittest.TestCase):
    def test_slug_strips_directory_and_extension(self):
        self.assertEqual(grade._slug("concepts/bm25.md"), "bm25")
        self.assertEqual(grade._slug("bm25.md"), "bm25")
        self.assertEqual(grade._slug("a/b/c/index.markdown"), "index")

    def test_slug_without_extension(self):
        self.assertEqual(grade._slug("concepts/bm25"), "bm25")

    def test_trailing_slash(self):
        self.assertEqual(grade._slug("concepts/bm25/"), "bm25")


class CitationHitTests(unittest.TestCase):
    def test_single_string_hit_across_directories(self):
        self.assertEqual(grade.citation_hit("notes/bm25.md", "concepts/bm25.md"), 1.0)

    def test_single_string_miss(self):
        self.assertEqual(grade.citation_hit("notes/bm25.md", "concepts/other.md"), 0.0)

    def test_collections_hit(self):
        self.assertEqual(grade.citation_hit(["x/a.md", "y/b.md"], ["z/b.md"]), 1.0)

    def test_collections_miss(self):
        self.assertEqual(grade.citation_hit(["x/a.md"], ["z/b.md"]), 0.0)

    def test_empty_inputs_are_zero(self):
        self.assertEqual(grade.citation_hit("", ["a.md"]), 0.0)
        self.assertEqual(grade.citation_hit("a.md", ""), 0.0)
        self.assertEqual(grade.citation_hit([], ["a.md"]), 0.0)
        self.assertEqual(grade.citation_hit("a.md", []), 0.0)

    def test_string_is_never_iterated_as_characters(self):
        # "abc" as a collection would iterate characters; it must not match
        # a slug built from a single character path.
        self.assertEqual(grade.citation_hit("abc", ["b"]), 0.0)


class PointRecallTests(unittest.TestCase):
    def test_fraction_of_matched_points_case_insensitive(self):
        self.assertEqual(grade.point_recall("BM25 ranks by TF-IDF", ["bm25", "tf-idf"]), 1.0)
        self.assertEqual(grade.point_recall("BM25 ranks by TF-IDF", ["bm25", "proximity"]), 0.5)

    def test_missed_points(self):
        self.assertEqual(grade.point_recall("nothing here", ["alpha", "beta", "gamma"]), 0.0)

    def test_empty_expectations_are_zero(self):
        self.assertEqual(grade.point_recall("anything", []), 0.0)

    def test_empty_answer_is_zero_with_expectations(self):
        self.assertEqual(grade.point_recall("", ["alpha"]), 0.0)


class MeanTests(unittest.TestCase):
    def test_mean_of_values(self):
        self.assertEqual(grade.mean([1.0, 2.0, 3.0]), 2.0)
        self.assertEqual(grade.mean([0.5]), 0.5)

    def test_mean_of_empty_is_zero(self):
        self.assertEqual(grade.mean([]), 0.0)


class ParseRawHitsTests(unittest.TestCase):
    """Strict parsing of the raw `oasis search --json` payload."""

    def test_parses_ranked_paths_in_order(self):
        stdout = json.dumps([{"path": "b.md", "score": 2}, {"path": "a.md"}])
        self.assertEqual(retrieval_eval.parse_raw_hits(stdout), ["b.md", "a.md"])

    def test_empty_array_is_a_successful_empty_ranking(self):
        self.assertEqual(retrieval_eval.parse_raw_hits("[]"), [])

    def test_malformed_json_is_a_side_error(self):
        with self.assertRaises(retrieval_eval.SideError):
            retrieval_eval.parse_raw_hits("{not json")

    def test_non_array_payload_is_a_side_error(self):
        with self.assertRaises(retrieval_eval.SideError):
            retrieval_eval.parse_raw_hits(json.dumps({"hits": []}))

    def test_non_object_hit_is_a_side_error(self):
        with self.assertRaises(retrieval_eval.SideError):
            retrieval_eval.parse_raw_hits(json.dumps(["concepts/bm25.md"]))

    def test_hit_without_string_path_is_a_side_error(self):
        with self.assertRaises(retrieval_eval.SideError):
            retrieval_eval.parse_raw_hits(json.dumps([{"path": 5}]))
        with self.assertRaises(retrieval_eval.SideError):
            retrieval_eval.parse_raw_hits(json.dumps([{"score": 1}]))


class ParseProbeLineTests(unittest.TestCase):
    """Strict parsing of the reference_probe one-line JSON output."""

    def test_parses_paths_from_a_good_line(self):
        line = json.dumps(
            {"query": "q", "failed": False, "paths": ["a.md", "b.md"], "text": "…"}
        )
        self.assertEqual(retrieval_eval.parse_probe_line(line), ["a.md", "b.md"])

    def test_empty_paths_is_a_successful_empty_ranking(self):
        line = json.dumps({"query": "q", "failed": False, "paths": [], "text": ""})
        self.assertEqual(retrieval_eval.parse_probe_line(line), [])

    def test_failed_probe_is_a_side_error(self):
        line = json.dumps({"query": "q", "failed": True, "paths": [], "text": ""})
        with self.assertRaises(retrieval_eval.SideError):
            retrieval_eval.parse_probe_line(line)

    def test_malformed_json_is_a_side_error(self):
        with self.assertRaises(retrieval_eval.SideError):
            retrieval_eval.parse_probe_line("not json at all")

    def test_multiple_lines_are_a_side_error(self):
        good = json.dumps({"query": "q", "failed": False, "paths": [], "text": ""})
        with self.assertRaises(retrieval_eval.SideError):
            retrieval_eval.parse_probe_line(good + "\n" + good)

    def test_missing_or_non_boolean_failed_is_a_side_error(self):
        with self.assertRaises(retrieval_eval.SideError):
            retrieval_eval.parse_probe_line(json.dumps({"paths": []}))
        with self.assertRaises(retrieval_eval.SideError):
            retrieval_eval.parse_probe_line(
                json.dumps({"failed": "no", "paths": []})
            )

    def test_non_string_paths_are_a_side_error(self):
        with self.assertRaises(retrieval_eval.SideError):
            retrieval_eval.parse_probe_line(
                json.dumps({"failed": False, "paths": ["a.md", 3]})
            )


class ScorePathsTests(unittest.TestCase):
    def test_metrics_match_grade_functions(self):
        scores = retrieval_eval.score_paths(["b.md", "c.md", "a.md"], ["a.md"], 3)
        self.assertEqual(scores["recall@1"], 0.0)
        self.assertEqual(scores["recall@3"], 1.0)
        self.assertEqual(scores["recall@5"], 1.0)
        self.assertEqual(scores["recall@k"], 1.0)
        self.assertEqual(scores["mrr@k"], 1 / 3)

    def test_mrr_is_truncated_at_k(self):
        scores = retrieval_eval.score_paths(["x", "y", "a"], ["a"], 2)
        self.assertEqual(scores["mrr@k"], 0.0)  # relevant at rank 3, outside k=2
        self.assertEqual(scores["recall@5"], 1.0)  # recall@5 looks past k

    def test_order_and_duplicates_are_preserved(self):
        scores = retrieval_eval.score_paths(["a", "a", "b"], ["a", "b"], 2)
        self.assertEqual(scores["recall@k"], 0.5)  # duplicate does not compact

    def test_empty_ranking_scores_zero(self):
        scores = retrieval_eval.score_paths([], ["a"], 3)
        for key in retrieval_eval.METRIC_KEYS:
            self.assertEqual(scores[key], 0.0)


class RecordTests(unittest.TestCase):
    def test_failed_side_scores_zero_and_keeps_error(self):
        raw = retrieval_eval.make_side(False, error="boom")
        tool = retrieval_eval.make_side(True, paths=["a.md"], seconds=1.0)
        record = retrieval_eval.build_record("id1", "q", ["a.md"], raw, tool, 3)
        self.assertFalse(record["raw"]["ok"])
        self.assertEqual(record["raw"]["error"], "boom")
        self.assertEqual(record["raw"]["paths"], [])
        self.assertEqual(
            record["raw"]["scores"],
            {key: 0.0 for key in retrieval_eval.METRIC_KEYS},
        )
        self.assertEqual(record["tool"]["scores"]["recall@1"], 1.0)
        self.assertIsNone(record["tool"]["error"])

    def test_successful_empty_ranking_is_success_with_zero_scores(self):
        side = retrieval_eval.make_side(True, paths=[], seconds=0.5)
        record = retrieval_eval.build_record("id2", "q", ["a.md"], side, side, 3)
        self.assertTrue(record["raw"]["ok"])
        self.assertEqual(record["raw"]["scores"]["recall@1"], 0.0)

    def test_success_error_paths_and_seconds_are_stored_apart(self):
        side = retrieval_eval.make_side(True, paths=["a.md"], seconds=1.25)
        self.assertIsNone(side["error"])
        self.assertEqual(side["seconds"], 1.25)
        self.assertEqual(side["paths"], ["a.md"])
        self.assertTrue(side["ok"])


class AggregateTests(unittest.TestCase):
    def _record(
        self,
        rid,
        raw_paths,
        tool_paths,
        raw_ok=True,
        tool_ok=True,
        raw_secs=1.0,
        tool_secs=1.0,
    ):
        return retrieval_eval.build_record(
            rid,
            f"q{rid}",
            ["a.md"],
            retrieval_eval.make_side(
                raw_ok, paths=raw_paths, seconds=raw_secs,
                error=None if raw_ok else "raw boom",
            ),
            retrieval_eval.make_side(
                tool_ok, paths=tool_paths, seconds=tool_secs,
                error=None if tool_ok else "tool boom",
            ),
            3,
        )

    def test_averages_over_all_records_with_failed_sides_scored_zero(self):
        records = [
            self._record("1", ["a.md"], ["a.md"]),
            self._record("2", ["a.md"], [], tool_ok=False),
        ]
        summary = retrieval_eval.aggregate(records, 3)
        self.assertEqual(summary["raw"]["metrics"]["recall@1"], 1.0)
        self.assertEqual(summary["tool"]["metrics"]["recall@1"], 0.5)
        self.assertEqual(summary["delta"]["recall@1"], -0.5)
        self.assertEqual(summary["queries"], 2)
        self.assertEqual(summary["raw"]["ok"], 2)
        self.assertEqual(summary["tool"]["ok"], 1)
        self.assertEqual(summary["tool"]["failed"], 1)

    def test_empty_records_give_zero_metrics_and_null_parity(self):
        summary = retrieval_eval.aggregate([], 3)
        self.assertEqual(summary["queries"], 0)
        for key in retrieval_eval.METRIC_KEYS:
            self.assertEqual(summary["raw"]["metrics"][key], 0.0)
            self.assertEqual(summary["delta"][key], 0.0)
        self.assertIsNone(summary["parity"])
        self.assertEqual(summary["parity_comparable"], 0)
        self.assertIsNone(summary["raw"]["latency"]["median"])
        self.assertIsNone(summary["raw"]["latency"]["p95"])

    def test_parity_uses_full_ordered_lists_including_duplicates(self):
        records = [
            self._record("1", ["a", "a", "b"], ["a", "b"]),  # same hits, diverged
            self._record("2", [], []),  # both-successful empties count as equal
            self._record("3", ["a"], ["a"]),
        ]
        summary = retrieval_eval.aggregate(records, 3)
        self.assertAlmostEqual(summary["parity"], 2 / 3)
        self.assertEqual(summary["diverging_ids"], ["1"])

    def test_parity_skips_queries_where_a_side_failed(self):
        summary = retrieval_eval.aggregate([self._record("1", ["a"], [], tool_ok=False)], 3)
        self.assertIsNone(summary["parity"])
        self.assertEqual(summary["parity_comparable"], 0)

    def test_latency_uses_successful_sides_only(self):
        records = [
            self._record("1", ["a"], ["a"], raw_secs=1.0, tool_secs=3.0),
            self._record("2", ["a"], ["a"], raw_secs=3.0, tool_secs=1.0),
            self._record("3", ["a"], ["a"], tool_ok=False, tool_secs=None),
        ]
        summary = retrieval_eval.aggregate(records, 3)
        # raw latencies [1.0, 3.0, 1.0] -> median 1.0; tool latencies [3.0, 1.0]
        # (the failed side's timing is excluded) -> median 2.0.
        self.assertEqual(summary["raw"]["latency"]["median"], 1.0)
        self.assertEqual(summary["tool"]["latency"]["median"], 2.0)
        self.assertEqual(summary["tool"]["ok"], 2)
        self.assertEqual(summary["tool"]["failed"], 1)


class PercentileTests(unittest.TestCase):
    def test_empty_input_is_none(self):
        self.assertIsNone(retrieval_eval.percentile([], 0.5))
        self.assertIsNone(retrieval_eval.median([]))

    def test_single_value_is_returned_for_any_fraction(self):
        self.assertEqual(retrieval_eval.percentile([7.0], 0.95), 7.0)

    def test_median_interpolates_deterministically(self):
        self.assertEqual(retrieval_eval.median([1.0, 2.0, 3.0, 4.0]), 2.5)
        self.assertEqual(retrieval_eval.median([3.0, 1.0, 2.0]), 2.0)

    def test_p95_is_deterministic(self):
        values = [float(i) for i in range(1, 21)]  # 1..20
        # position = 0.95 * 19 = 18.05 → 19 + 0.05 * (20 - 19) = 19.05
        self.assertAlmostEqual(retrieval_eval.percentile(values, 0.95), 19.05)


class RenderTableTests(unittest.TestCase):
    def _summary(self):
        records = [
            retrieval_eval.build_record(
                "1", "q1", ["a.md"],
                retrieval_eval.make_side(True, ["a.md"], 1.0),
                retrieval_eval.make_side(True, ["a.md"], 1.5), 3,
            ),
            retrieval_eval.build_record(
                "2", "q2", ["a.md"],
                retrieval_eval.make_side(True, ["a.md", "b.md"], 2.0),
                retrieval_eval.make_side(False, error="probe failed"), 3,
            ),
        ]
        return retrieval_eval.aggregate(records, 3)

    def test_table_has_metric_rows_parity_and_latency_lines(self):
        table = retrieval_eval.render_table(self._summary())
        for key in retrieval_eval.METRIC_KEYS:
            self.assertIn(key, table)
        self.assertIn("parity:", table)
        self.assertIn("raw latency:", table)
        self.assertIn("tool latency:", table)
        self.assertIn("p95", table)
        self.assertIn("1 failed", table)

    def test_diverging_ids_capped_at_five_in_sample_order(self):
        records = [
            retrieval_eval.build_record(
                f"q{index}", f"query {index}", ["a.md"],
                retrieval_eval.make_side(True, [f"a{index}.md"], 1.0),
                retrieval_eval.make_side(True, [f"b{index}.md"], 1.0), 3,
            )
            for index in range(7)
        ]
        table = retrieval_eval.render_table(retrieval_eval.aggregate(records, 3))
        line = next(l for l in table.splitlines() if l.startswith("diverging ids:"))
        listed = [item.strip() for item in line.split(":", 1)[1].split(",")]
        self.assertEqual(listed, [f"q{i}" for i in range(5)])

    def test_null_parity_prints_n_a(self):
        records = [
            retrieval_eval.build_record(
                "1", "q", ["a.md"],
                retrieval_eval.make_side(True, ["a.md"], 1.0),
                retrieval_eval.make_side(False, error="x"), 3,
            )
        ]
        table = retrieval_eval.render_table(retrieval_eval.aggregate(records, 3))
        self.assertIn("parity: n/a", table)


class ImportSafetyTests(unittest.TestCase):
    def test_retrieval_eval_module_body_has_no_top_level_execution(self):
        tree = ast.parse(inspect.getsource(retrieval_eval))
        for node in tree.body:
            if isinstance(node, ast.Expr):
                # Only the module docstring may appear as a bare expression.
                self.assertIsInstance(node.value, ast.Constant)
            elif isinstance(node, (ast.Assign, ast.AnnAssign)):
                self.assertIsNotNone(node.value)
                self.assertNotIsInstance(node.value, ast.Call)
            elif isinstance(node, ast.If):
                # The one allowed conditional: `if __name__ == "__main__":`,
                # the guard the CLI orchestration lives behind.
                self.assertEqual(ast.dump(node.test).count("'__main__'"), 1)
                self.assertEqual(
                    getattr(node.test, "left", None) and node.test.left.id, "__name__"
                )
            else:
                self.assertIsInstance(
                    node,
                    (ast.Import, ast.ImportFrom, ast.FunctionDef, ast.ClassDef),
                )




class CorpusRootTests(unittest.TestCase):
    """corpus.py resolves the gold checkout instead of hardcoding one path."""

    def test_ocs_root_env_var_wins(self):
        roots = corpus.candidate_roots(home="/home/x", environ={"OCS_ROOT": "/gold"})
        self.assertEqual(roots[0], "/gold")

    def test_conventional_location_follows(self):
        roots = corpus.candidate_roots(home="/home/x", environ={})
        self.assertEqual(roots[0], "/home/x/src/o-cs")

    def test_pick_root_takes_the_first_with_gold_data(self):
        self.assertEqual(corpus.pick_root(["/a", "/b"], lambda root: root == "/b"), "/b")

    def test_pick_root_is_none_when_nothing_has_gold_data(self):
        self.assertIsNone(corpus.pick_root(["/a", "/b"], lambda root: False))

    def test_default_corpus_is_the_wiki_directory(self):
        self.assertTrue(corpus.default_corpus("/gold", environ={}).endswith("/gold/wiki"))

    def test_configured_reference_root_overrides_the_checkout(self):
        self.assertEqual(
            corpus.default_corpus("/gold", environ={"DRIP_REFERENCE_ROOTS": "/other/wiki:/second"}),
            "/other/wiki",
        )

    def test_blank_reference_roots_falls_back_to_the_checkout(self):
        self.assertTrue(
            corpus.default_corpus("/gold", environ={"DRIP_REFERENCE_ROOTS": " "}).endswith("/gold/wiki")
        )

    def test_default_corpus_of_no_root_is_empty(self):
        self.assertEqual(corpus.default_corpus(None, environ={}), "")

    def test_missing_message_says_how_to_fix_it(self):
        message = corpus.missing_message("corpus")
        self.assertIn("OCS_ROOT", message)
        self.assertIn(corpus.GOLD_MARKER, message)


if __name__ == "__main__":
    unittest.main()
