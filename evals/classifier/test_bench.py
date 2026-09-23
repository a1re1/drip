#!/usr/bin/env python3
"""Unit tests for evals/classifier/bench.py. Stdlib only, no network."""
import importlib.util
import json
import os
import shutil
import tempfile
import unittest

_spec = importlib.util.spec_from_file_location(
    "clfbench", os.path.join(os.path.dirname(os.path.abspath(__file__)), "bench.py")
)
bench = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bench)


def loop_start(at, loop, task_id, skills, role="author", task=None, tools=None):
    detail = "loop %d" % loop
    if skills:
        detail += " [skills: %s]" % ", ".join(skills)
    detail += " [role: %s] \u2014 %s: %s" % (role, task_id, task or "a task")
    data = {"loop": loop, "taskId": task_id}
    if skills:
        data["skills"] = list(skills)
    if tools:
        data["tools"] = list(tools)
    return {"at": at, "data": data, "detail": detail, "kind": "loop-start", "type": "event"}


class TranscriptFixture(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp(prefix="clfbench-test-")
        self.projects = os.path.join(self.root, "projects")
        session = os.path.join(self.projects, "proj-a", "sessions", "sess-1")
        os.makedirs(session)
        self.transcript = os.path.join(session, "transcript.jsonl")
        records = [
            {"at": "2026-09-20T00:00:00Z", "text": "first goal", "type": "goal"},
            loop_start("2026-09-20T00:01:00Z", 1, "task-1", ["tdd"], task="write a failing test"),
            {"at": "2026-09-20T00:02:00Z", "kind": "tool-call", "type": "event"},
            {"at": "2026-09-20T00:05:00Z", "text": "second goal", "type": "goal"},
            loop_start("2026-09-20T00:06:00Z", 1, "task-1", [], role="reviewer", task="review it"),
        ]
        with open(self.transcript, "w", encoding="utf-8") as fh:
            for record in records:
                fh.write(json.dumps(record) + "\n")
            fh.write("not json at all\n")

    def tearDown(self):
        shutil.rmtree(self.root, ignore_errors=True)

    def payload(self, **kwargs):
        return bench.harvest([self.transcript], **kwargs)


class ParseLoopStartTest(TranscriptFixture):
    def test_reads_skills_and_role_from_data(self):
        payload = self.payload()
        case = payload["cases"][0]
        self.assertEqual(case["skillsMatched"], ["tdd"])
        self.assertEqual(case["role"], "author")
        self.assertEqual(case["taskId"], "task-1")
        self.assertEqual(case["task"], "write a failing test")
        self.assertEqual(case["sessionId"], "sess-1")

    def test_falls_back_to_the_detail_skills_line_when_data_omits_it(self):
        event = loop_start("2026-09-20T00:09:00Z", 2, "task-2", ["navis", "tdd"])
        event["data"].pop("skills")
        case = bench.parse_loop_start(event, "sess-1", self.transcript, [])
        self.assertEqual(case["skillsMatched"], ["navis", "tdd"])

    def test_empty_skills_line_is_an_empty_match(self):
        event = loop_start("2026-09-20T00:09:00Z", 2, "task-2", [])
        case = bench.parse_loop_start(event, "sess-1", self.transcript, [])
        self.assertEqual(case["skillsMatched"], [])

    def test_ignores_non_loop_start_records(self):
        self.assertIsNone(bench.parse_loop_start({"type": "goal"}, "s", "t", []))
        self.assertIsNone(
            bench.parse_loop_start({"type": "event", "kind": "tool-call"}, "s", "t", [])
        )

    def test_goal_is_the_one_in_force_at_that_moment(self):
        cases = self.payload()["cases"]
        self.assertEqual(cases[0]["goal"], "first goal")
        self.assertEqual(cases[1]["goal"], "second goal")

    def test_malformed_lines_are_counted_not_fatal(self):
        payload = self.payload()
        self.assertEqual(payload["badLines"], 1)
        self.assertEqual(len(payload["cases"]), 2)

    def test_limit_keeps_the_most_recent_loops(self):
        payload = self.payload(limit=1)
        self.assertEqual(len(payload["cases"]), 1)
        self.assertEqual(payload["cases"][0]["goal"], "second goal")

    def test_since_filters_by_timestamp(self):
        payload = self.payload(since="2026-09-20T00:03:00Z")
        self.assertEqual(len(payload["cases"]), 1)
        self.assertEqual(payload["cases"][0]["role"], "reviewer")

    def test_find_transcripts_walks_the_projects_layout(self):
        found = bench.find_transcripts(self.projects)
        self.assertIn(self.transcript, found)

    def test_skill_histogram_and_observed_pool(self):
        cases = self.payload()["cases"]
        self.assertEqual(bench.skill_histogram(cases), {"tdd": 1})
        self.assertEqual(bench.observed_pool(cases), ["tdd"])


class SurveyTest(TranscriptFixture):
    def test_survey_withholds_what_the_classifier_matched(self):
        cases = self.payload()["cases"]
        survey, key = bench.build_survey(cases, ["tdd", "navis"], n=0, seed=1)
        for entry in survey["cases"]:
            self.assertNotIn("skillsMatched", entry)
            self.assertNotIn("matched", entry)
        self.assertEqual(key["key"][cases[0]["caseId"]]["skillsMatched"], ["tdd"])

    def test_options_always_contain_the_classifier_pick(self):
        cases = [self.payload()["cases"][0]]
        survey, _ = bench.build_survey(cases, ["navis"], n=0, seed=1)
        self.assertIn("tdd", survey["cases"][0]["options"])
        self.assertIn("navis", survey["cases"][0]["options"])

    def test_survey_is_deterministic_for_a_seed(self):
        cases = self.payload()["cases"]
        first, _ = bench.build_survey(cases, ["tdd"], n=1, seed=7)
        second, _ = bench.build_survey(cases, ["tdd"], n=1, seed=7)
        self.assertEqual(first, second)

    def test_survey_honours_the_case_limit(self):
        cases = self.payload()["cases"]
        survey, key = bench.build_survey(cases, ["tdd"], n=1, seed=0)
        self.assertEqual(len(survey["cases"]), 1)
        self.assertEqual(len(key["key"]), 1)


class CompareTest(unittest.TestCase):
    def case(self, cid, matched, picks, context=None):
        meta = {"caseId": cid, "skillsMatched": matched}
        if context:
            meta["context"] = context
        return bench.compare_picks(meta, picks)

    def test_equal_sets_agree(self):
        row = self.case("a", ["tdd", "navis"], ["navis", "tdd"])
        self.assertEqual(row["kind"], "agree")
        self.assertEqual(row["missed"], [])
        self.assertEqual(row["spurious"], [])
        self.assertEqual(row["agreed"], ["navis", "tdd"])

    def test_split_missed_and_spurious(self):
        row = self.case("b", ["verify-before-done"], ["tdd"])
        self.assertEqual(row["kind"], "disagree")
        self.assertEqual(row["missed"], ["tdd"])
        self.assertEqual(row["spurious"], ["verify-before-done"])

    def test_answers_file_shapes_both_accepted(self):
        wrapped = bench.answers_by_case({"answers": [{"caseId": "x", "picks": ["tdd"]}]})
        bare = bench.answers_by_case([{"caseId": "x", "picks": ["tdd"]}])
        self.assertEqual(wrapped, bare)
        self.assertEqual(wrapped["x"], ["tdd"])

    def test_compare_survey_skips_unanswered_cases(self):
        survey = {"cases": [{"caseId": "a", "context": {}}, {"caseId": "b", "context": {}}]}
        key = {"key": {"a": {"skillsMatched": ["tdd"]}, "b": {"skillsMatched": []}}}
        results = bench.compare_survey(survey, key, {"answers": [{"caseId": "a", "picks": ["tdd"]}]})
        self.assertEqual([row["caseId"] for row in results], ["a"])


class PinTest(unittest.TestCase):
    def rows(self):
        return [
            {
                "caseId": "clf-1",
                "kind": "disagree",
                "picks": ["tdd"],
                "matched": ["verify-before-done"],
                "missed": ["tdd"],
                "spurious": ["verify-before-done"],
                "context": {"task": "write a test"},
                "source": {"sessionId": "s"},
            },
            {
                "caseId": "clf-2",
                "kind": "agree",
                "picks": ["tdd"],
                "matched": ["tdd"],
                "missed": [],
                "spurious": [],
                "context": {},
                "source": {},
            },
        ]

    def test_pins_only_disagreements(self):
        cases, added = bench.pin_cases(self.rows(), {"version": 1, "cases": []}, pinned_at="2026-09-23T00:00:00Z")
        self.assertEqual(added, 1)
        self.assertEqual([case["id"] for case in cases["cases"]], ["clf-1"])
        self.assertEqual(cases["cases"][0]["kind"], "false-negative")
        self.assertEqual(cases["cases"][0]["targetSkill"], "tdd")
        self.assertEqual(cases["cases"][0]["status"], "open")

    def test_pinning_twice_is_idempotent(self):
        first, _ = bench.pin_cases(self.rows(), {"version": 1, "cases": []})
        second, added = bench.pin_cases(self.rows(), first)
        self.assertEqual(added, 0)
        self.assertEqual(len(second["cases"]), 1)

    def test_false_positive_targets_the_spurious_skill(self):
        rows = [
            {
                "caseId": "clf-3",
                "kind": "disagree",
                "picks": [],
                "matched": ["navis"],
                "missed": [],
                "spurious": ["navis"],
            }
        ]
        cases, _ = bench.pin_cases(rows, {"version": 1, "cases": []})
        self.assertEqual(cases["cases"][0]["kind"], "false-positive")
        self.assertEqual(cases["cases"][0]["targetSkill"], "navis")
        self.assertEqual(cases["cases"][0]["target"], [])


class ReplayTest(unittest.TestCase):
    def case(self):
        return {
            "id": "clf-9",
            "target": ["tdd"],
            "context": {"goal": "add a skill", "task": "write a failing test", "role": "author"},
        }

    def test_argv_carries_context_and_sandbox(self):
        argv = bench.replay_argv(self.case(), drip="drip", project_dir="/tmp/pd", max_iterations=2)
        self.assertEqual(argv[0], "drip")
        self.assertIn("--json", argv)
        self.assertEqual(argv[argv.index("--prompt") + 1], "goal: add a skill\ncurrent task: write a failing test\nrole: author")
        self.assertEqual(argv[argv.index("--project-dir") + 1], "/tmp/pd")
        self.assertEqual(argv[argv.index("--max-iterations") + 1], "2")
        for flag in ("--no-review", "--no-ask", "--no-repo-memory"):
            self.assertIn(flag, argv)

    def test_prompt_falls_back_to_the_case_id(self):
        self.assertEqual(bench.context_prompt({"id": "clf-9", "context": {}}), "clf-9")

    def test_extracts_skills_from_ndjson(self):
        lines = [
            json.dumps({"type": "info", "text": "classifier: jev \u00b7 12 skills in pool"}),
            "not json",
            json.dumps(
                {
                    "type": "event",
                    "kind": "loop-start",
                    "data": {"loop": 1, "skills": ["tdd", "navis"]},
                }
            ),
        ]
        self.assertEqual(bench.extract_loop_start_skills(lines), ["navis", "tdd"])
        self.assertIsNone(bench.extract_loop_start_skills(lines[:2]))


class ReportTest(unittest.TestCase):
    def results(self):
        return [
            {"caseId": "a", "kind": "agree", "agreed": ["tdd"], "missed": [], "spurious": []},
            {"caseId": "b", "kind": "disagree", "agreed": [], "missed": ["navis"], "spurious": ["tdd"]},
        ]

    def test_match_rate_arithmetic(self):
        summary = bench.summarize_results(self.results())
        self.assertEqual(summary["cases"], 2)
        self.assertEqual(summary["agree"], 1)
        self.assertEqual(summary["matchRate"], 0.5)

    def test_per_skill_precision_and_recall(self):
        skills = bench.summarize_results(self.results())["skills"]
        self.assertEqual(skills["tdd"]["precision"], 0.5)
        self.assertEqual(skills["tdd"]["recall"], 1.0)
        self.assertEqual(skills["navis"]["recall"], 0.0)
        self.assertEqual(skills["navis"]["precision"], None)

    def test_empty_report_has_no_match_rate(self):
        summary = bench.summarize_results([])
        self.assertEqual(summary["cases"], 0)
        self.assertIsNone(summary["matchRate"])


if __name__ == "__main__":
    unittest.main()
