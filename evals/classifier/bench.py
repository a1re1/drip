#!/usr/bin/env python3
"""Benchmark drip's skill classifier against blind operator judgment.

Digs through recorded session transcripts, rebuilds the context of each loop the
classifier saw, shows that context to the operator BLIND (the skills the
classifier matched are withheld), collects a multiselect from the observed skill
surface, and compares the two. Disagreements are the interesting rows: they get
pinned into `cases.json` as regression cases, and `replay` re-runs the live
classifier on them so tuning a skill's `classifiers.json` questions has a number
to move — the match rate.

  python3 evals/classifier/bench.py harvest
  python3 evals/classifier/bench.py survey --n 12
  python3 evals/classifier/bench.py compare --survey survey.json --answers answers.json
  python3 evals/classifier/bench.py pin --disagreements disagreements.json
  python3 evals/classifier/bench.py replay --cases cases.json --write
  python3 evals/classifier/bench.py report --survey survey.json --answers answers.json

Stdlib only. Nothing here talks to the classifier except `replay`, which shells
out to `drip` so the pool rules (thresholds, caps, authored formulas) stay in
exactly one place.
"""

import argparse
import glob
import hashlib
import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time

ROLE_RE = re.compile(r"\[role:\s*([a-z][a-z-]*)\]")
SKILLS_RE = re.compile(r"\[skills:\s*([^\]]*)\]")
TASK_RE = re.compile(r"(?:^|\s)(task-[A-Za-z0-9-]+):\s*(.*)$")

DEFAULT_PROJECTS_DIR = os.path.join(os.path.expanduser("~"), ".drip", "projects")
DEFAULT_DIR = os.path.dirname(os.path.abspath(__file__))


# ---------------------------------------------------------------------------
# Transcript digging
# ---------------------------------------------------------------------------


def now_iso():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def load_records(path):
    """Parsed JSONL records plus the count of unparseable lines."""
    records, bad = [], 0
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                records.append(json.loads(line))
            except ValueError:
                bad += 1
    return records, bad


def session_id_for(path):
    return os.path.basename(os.path.dirname(os.path.abspath(path)))


def goal_at(goals, when):
    """Goal text in force at `when`: the latest goal record not after it."""
    chosen = None
    for at, text in goals:
        if when is None or at is None or (at or "") <= when:
            chosen = text
        else:
            break
    return chosen


def case_id(session_id, loop, task_id, at):
    raw = "%s|%s|%s|%s" % (session_id, loop, task_id, at)
    return "clf-" + hashlib.sha1(raw.encode("utf-8")).hexdigest()[:10]


def parse_loop_start(event, sid, transcript, goals):
    """One classified loop as a case dict, or None for any other record."""
    if event.get("type") != "event" or event.get("kind") != "loop-start":
        return None

    data = event.get("data") or {}
    detail = event.get("detail") or ""
    skills = data.get("skills")
    if skills is None:
        found = SKILLS_RE.search(detail)
        skills = (
            [item.strip() for item in found.group(1).split(",") if item.strip()]
            if found
            else []
        )

    role = ROLE_RE.search(detail)
    task = TASK_RE.search(detail)
    at = event.get("at")
    return {
        "caseId": case_id(sid, data.get("loop"), data.get("taskId"), at),
        "sessionId": sid,
        "transcript": transcript,
        "at": at,
        "iteration": event.get("iteration"),
        "loop": data.get("loop"),
        "taskId": data.get("taskId"),
        "role": role.group(1) if role else None,
        "task": task.group(2).strip() if task else None,
        "goal": goal_at(goals, at),
        "skillsMatched": sorted(skills),
        "tools": data.get("tools") or [],
    }


def find_transcripts(projects_dir, extra_globs=()):
    patterns = [
        os.path.join(projects_dir, "*", "sessions", "*", "transcript.jsonl"),
        os.path.join(projects_dir, "*", "*", "sessions", "*", "transcript.jsonl"),
    ]
    patterns.extend(extra_globs)
    found = []
    for pattern in patterns:
        found.extend(glob.glob(pattern))
    return sorted(set(found))


def harvest(transcripts, limit=0, since=None):
    cases, bad_lines = [], 0
    for path in transcripts:
        records, bad = load_records(path)
        bad_lines += bad
        goals = sorted(
            ((r.get("at"), r.get("text") or "") for r in records if r.get("type") == "goal"),
            key=lambda item: item[0] or "",
        )
        sid = session_id_for(path)
        for record in records:
            case = parse_loop_start(record, sid, path, goals)
            if case is None:
                continue
            if since and (case["at"] or "") < since:
                continue
            cases.append(case)
    cases.sort(key=lambda c: (c["sessionId"], c["at"] or "", c["loop"] or 0))
    if limit and limit > 0 and len(cases) > limit:
        cases = cases[-limit:]
    return {"version": 1, "scanned": len(transcripts), "badLines": bad_lines, "cases": cases}


def skill_histogram(cases):
    counts = {}
    for case in cases:
        for name in case["skillsMatched"]:
            counts[name] = counts.get(name, 0) + 1
    return dict(sorted(counts.items(), key=lambda item: (-item[1], item[0])))


def observed_pool(cases):
    names = set()
    for case in cases:
        names.update(case["skillsMatched"])
    return sorted(names)


# ---------------------------------------------------------------------------
# Blind survey
# ---------------------------------------------------------------------------


def context_of(case):
    return {
        "goal": case.get("goal"),
        "task": case.get("task"),
        "taskId": case.get("taskId"),
        "role": case.get("role"),
    }


def options_for(case, pool):
    """The multiselect the operator sees. The classifier's own picks are always
    on the list, so a miss can be reported as a miss rather than as an absent
    option."""
    return sorted(set(pool) | set(case["skillsMatched"]))


def build_survey(cases, pool, n=0, seed=0):
    """(survey, key). The survey never carries what the classifier matched."""
    rng = random.Random(seed)
    eligible = [c for c in cases if c.get("task") or c.get("goal")]
    picked = rng.sample(eligible, n) if n and 0 < n < len(eligible) else eligible

    survey, key = [], {}
    for case in picked:
        survey.append(
            {
                "caseId": case["caseId"],
                "context": context_of(case),
                "options": options_for(case, pool),
            }
        )
        key[case["caseId"]] = {
            "skillsMatched": case["skillsMatched"],
            "role": case.get("role"),
            "taskId": case.get("taskId"),
            "loop": case.get("loop"),
            "at": case.get("at"),
            "sessionId": case.get("sessionId"),
            "transcript": case.get("transcript"),
            "context": context_of(case),
        }
    return (
        {"version": 1, "createdAt": now_iso(), "cases": survey},
        {"version": 1, "key": key},
    )


# ---------------------------------------------------------------------------
# Compare / pin
# ---------------------------------------------------------------------------


def compare_picks(case, picks):
    matched = set(case.get("skillsMatched") or [])
    picked = set(picks or [])
    return {
        "caseId": case["caseId"],
        "context": case.get("context") or context_of(case),
        "source": {
            key: case.get(key)
            for key in ("sessionId", "transcript", "loop", "taskId", "at", "role")
        },
        "picks": sorted(picked),
        "matched": sorted(matched),
        "missed": sorted(picked - matched),
        "spurious": sorted(matched - picked),
        "agreed": sorted(picked & matched),
        "kind": "agree" if picked == matched else "disagree",
    }


def answers_by_case(answers):
    rows = answers.get("answers") if isinstance(answers, dict) else answers
    out = {}
    for row in rows or []:
        if "caseId" in row:
            out[row["caseId"]] = row.get("picks") or []
    return out


def compare_survey(survey, key, answers):
    picks = answers_by_case(answers)
    results = []
    for entry in survey.get("cases") or []:
        cid = entry["caseId"]
        meta = dict(key.get("key", {}).get(cid) or {})
        meta["caseId"] = cid
        meta.setdefault("context", entry.get("context"))
        if cid not in picks:
            continue
        results.append(compare_picks(meta, picks[cid]))
    return results


def pin_cases(disagreements, existing, pinned_at=None):
    """Append the disagreements that are not already pinned. Idempotent."""
    pinned_at = pinned_at or now_iso()
    cases = list((existing or {}).get("cases") or [])
    seen = {case["id"] for case in cases}
    added = 0

    for row in disagreements:
        if row.get("kind") != "disagree" or row["caseId"] in seen:
            continue
        targets = row.get("missed") or row.get("spurious") or []
        cases.append(
            {
                "id": row["caseId"],
                "pinnedAt": pinned_at,
                "kind": "false-negative" if row.get("missed") else "false-positive",
                "targetSkill": targets[0] if targets else None,
                "target": row.get("picks") or [],
                "matched": row.get("matched") or [],
                "missed": row.get("missed") or [],
                "spurious": row.get("spurious") or [],
                "context": row.get("context"),
                "source": row.get("source"),
                "status": "open",
                "lastRun": None,
            }
        )
        seen.add(row["caseId"])
        added += 1

    return {"version": (existing or {}).get("version", 1), "cases": cases}, added


# ---------------------------------------------------------------------------
# Replay (the only part that calls the classifier)
# ---------------------------------------------------------------------------


def context_prompt(case):
    context = case.get("context") or {}
    lines = []
    if context.get("goal"):
        lines.append("goal: %s" % context["goal"])
    if context.get("task"):
        lines.append("current task: %s" % context["task"])
    if context.get("role"):
        lines.append("role: %s" % context["role"])
    return "\n".join(lines) or (case.get("id") or "context")


def replay_argv(case, drip="drip", project_dir=None, max_iterations=1, home=None):
    argv = [
        drip,
        "--json",
        "--prompt",
        context_prompt(case),
        "--max-iterations",
        str(max_iterations),
        "--no-review",
        "--no-ask",
        "--no-repo-memory",
    ]
    if project_dir:
        argv += ["--project-dir", project_dir]
    if home:
        argv += ["--home", home]
    return argv


def extract_loop_start_skills(lines):
    """The skills the harness composed for the first classified loop, or None."""
    for line in lines:
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            record = json.loads(line)
        except ValueError:
            continue
        if record.get("kind") != "loop-start":
            continue
        data = record.get("data") or {}
        return sorted(data.get("skills") or [])
    return None


def replay_case(case, drip="drip", timeout=900, max_iterations=1, home=None, keep=False):
    root = tempfile.mkdtemp(prefix="probatio-")
    project = os.path.join(root, "project")
    data_dir = os.path.join(root, "project-data")
    os.makedirs(project)

    env = dict(os.environ)
    env.pop("DRIP_PROJECT_DIR", None)
    argv = replay_argv(case, drip=drip, project_dir=data_dir, max_iterations=max_iterations, home=home)
    record = {"id": case["id"], "at": now_iso()}

    try:
        proc = subprocess.run(
            argv, cwd=project, env=env, capture_output=True, text=True, timeout=timeout
        )
        record["exitCode"] = proc.returncode
        skills = extract_loop_start_skills(proc.stdout.splitlines())
        if skills is None:
            for path in glob.glob(os.path.join(data_dir, "sessions", "*", "transcript.jsonl")):
                skills = extract_loop_start_skills(open(path, encoding="utf-8", errors="replace"))
                if skills is not None:
                    break
    except subprocess.TimeoutExpired:
        skills = None
        record["exitCode"] = None
        record["timeout"] = True
    finally:
        if not keep:
            shutil.rmtree(root, ignore_errors=True)
        else:
            record["workspace"] = root

    if skills is None:
        record["error"] = "no loop-start event in the run output"
        record["status"] = "error"
        return record

    target = set(case.get("target") or [])
    matched = set(skills)
    record.update(
        {
            "skills": sorted(matched),
            "missing": sorted(target - matched),
            "extra": sorted(matched - target),
            "status": "pass" if matched == target else "fail",
        }
    )
    return record


# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------


def _ratio(numerator, denominator):
    return None if not denominator else round(numerator / float(denominator), 4)


def summarize_results(results):
    per_skill = {}
    for row in results:
        for name, field in (
            [(n, "agreed") for n in row.get("agreed", [])]
            + [(n, "missed") for n in row.get("missed", [])]
            + [(n, "spurious") for n in row.get("spurious", [])]
        ):
            stats = per_skill.setdefault(name, {"agreed": 0, "missed": 0, "spurious": 0})
            stats[field] += 1

    for stats in per_skill.values():
        stats["classifierPicks"] = stats["agreed"] + stats["spurious"]
        stats["operatorPicks"] = stats["agreed"] + stats["missed"]
        stats["precision"] = _ratio(stats["agreed"], stats["classifierPicks"])
        stats["recall"] = _ratio(stats["agreed"], stats["operatorPicks"])

    agreed = sum(1 for row in results if row.get("kind") == "agree")
    return {
        "cases": len(results),
        "agree": agreed,
        "matchRate": _ratio(agreed, len(results)),
        "skills": dict(sorted(per_skill.items())),
    }


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def read_json(path, default=None):
    if not os.path.exists(path):
        if default is not None:
            return default
        raise SystemExit("missing file: %s" % path)
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def write_json(path, payload):
    directory = os.path.dirname(os.path.abspath(path))
    if directory:
        os.makedirs(directory, exist_ok=True)
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(payload, fh, indent=2, sort_keys=False)
        fh.write("\n")


def cmd_harvest(args):
    transcripts = find_transcripts(args.projects_dir, args.transcripts or [])
    payload = harvest(transcripts, limit=args.limit, since=args.since)
    write_json(args.out, payload)
    cases = payload["cases"]
    print(
        "harvest: %d transcripts, %d loop contexts, %d unparseable lines -> %s"
        % (payload["scanned"], len(cases), payload["badLines"], args.out)
    )
    histogram = skill_histogram(cases)
    for name, count in list(histogram.items())[: args.top]:
        print("  %-28s %d" % (name, count))
    write_json(
        args.pool_out,
        {"version": 1, "observed": observed_pool(cases), "histogram": histogram},
    )
    print("observed skill surface (%d) -> %s" % (len(observed_pool(cases)), args.pool_out))
    return 0


def cmd_survey(args):
    cases = read_json(args.harvest)["cases"]
    pool = read_json(args.pool_file)["observed"] if args.pool_file else observed_pool(cases)
    if args.since:
        cases = [c for c in cases if (c["at"] or "") >= args.since]
    survey, key = build_survey(cases, pool, n=args.n, seed=args.seed)
    write_json(args.out, survey)
    write_json(args.key, key)
    print("survey: %d blind cases -> %s (answer key: %s)" % (len(survey["cases"]), args.out, args.key))
    print("show ONLY %s to the operator; %s is the answer key" % (args.out, args.key))
    return 0


def cmd_compare(args):
    survey = read_json(args.survey)
    key = read_json(args.key)
    answers = read_json(args.answers)
    results = compare_survey(survey, key, answers)
    write_json(args.out, {"version": 1, "results": results})
    summary = summarize_results(results)
    print(
        "compare: %d answered, %d agree, match rate %s -> %s"
        % (summary["cases"], summary["agree"], summary["matchRate"], args.out)
    )
    for row in results:
        if row["kind"] == "disagree":
            print(
                "  %s missed=%s spurious=%s"
                % (row["caseId"], ",".join(row["missed"]) or "-", ",".join(row["spurious"]) or "-")
            )
    return 0


def cmd_pin(args):
    disagreements = read_json(args.disagreements)
    rows = disagreements.get("results") if isinstance(disagreements, dict) else disagreements
    cases, added = pin_cases(rows or [], read_json(args.cases, {"version": 1, "cases": []}))
    write_json(args.out or args.cases, cases)
    print("pin: %d new regression cases (%d total) -> %s" % (added, len(cases["cases"]), args.out or args.cases))
    return 0


def cmd_replay(args):
    payload = read_json(args.cases)
    cases = payload.get("cases") or []
    if args.id:
        wanted = set(args.id)
        cases = [case for case in cases if case["id"] in wanted]
    if not cases:
        print("replay: no cases selected")
        return 0

    failed = 0
    for case in cases:
        if args.dry_run:
            print(" ".join(replay_argv(case, drip=args.drip, project_dir="<tmp>/project-data", max_iterations=args.max_iterations, home=args.home)))
            continue
        result = replay_case(
            case,
            drip=args.drip,
            timeout=args.timeout,
            max_iterations=args.max_iterations,
            home=args.home,
            keep=args.keep,
        )
        print(
            "%-14s %-5s target=%s got=%s%s"
            % (
                case["id"],
                result["status"],
                ",".join(case.get("target") or []) or "-",
                ",".join(result.get("skills") or []) or (result.get("error") or "-"),
                " missing=%s" % ",".join(result["missing"]) if result.get("missing") else "",
            )
        )
        case["status"] = result["status"]
        case["lastRun"] = result
        if result["status"] != "pass":
            failed += 1

    if args.write:
        write_json(args.cases, payload)
        print("replay: results written back to %s" % args.cases)
    return 1 if failed else 0


def cmd_report(args):
    results = compare_survey(read_json(args.survey), read_json(args.key), read_json(args.answers))
    summary = summarize_results(results)
    print(json.dumps(summary, indent=2) if args.json else "")
    print(
        "report: %d cases, %d agree, match rate %s"
        % (summary["cases"], summary["agree"], summary["matchRate"])
    )
    print("  %-28s %6s %6s %9s %7s" % ("skill", "agree", "miss", "precision", "recall"))
    for name, stats in summary["skills"].items():
        if stats["missed"] or stats["spurious"]:
            print(
                "  %-28s %6d %6d %9s %7s"
                % (name, stats["agreed"], stats["missed"], stats["precision"], stats["recall"])
            )
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("harvest", help="scan session transcripts for classified loops")
    p.add_argument("--projects-dir", default=DEFAULT_PROJECTS_DIR)
    p.add_argument("--transcripts", action="append", help="extra glob(s) of transcript.jsonl")
    p.add_argument("--since", help="ISO timestamp lower bound")
    p.add_argument("--limit", type=int, default=0, help="keep only the N most recent loops")
    p.add_argument("--top", type=int, default=15)
    p.add_argument("--out", default=os.path.join(DEFAULT_DIR, "harvest.json"))
    p.add_argument("--pool-out", default=os.path.join(DEFAULT_DIR, "pool.json"))
    p.set_defaults(func=cmd_harvest)

    p = sub.add_parser("survey", help="build the blind multiselect survey + answer key")
    p.add_argument("--harvest", default=os.path.join(DEFAULT_DIR, "harvest.json"))
    p.add_argument("--pool-file", default=None)
    p.add_argument("--n", type=int, default=12)
    p.add_argument("--seed", type=int, default=0)
    p.add_argument("--since")
    p.add_argument("--out", default=os.path.join(DEFAULT_DIR, "survey.json"))
    p.add_argument("--key", default=os.path.join(DEFAULT_DIR, "key.json"))
    p.set_defaults(func=cmd_survey)

    p = sub.add_parser("compare", help="score operator picks against what the classifier matched")
    p.add_argument("--survey", default=os.path.join(DEFAULT_DIR, "survey.json"))
    p.add_argument("--key", default=os.path.join(DEFAULT_DIR, "key.json"))
    p.add_argument("--answers", default=os.path.join(DEFAULT_DIR, "answers.json"))
    p.add_argument("--out", default=os.path.join(DEFAULT_DIR, "disagreements.json"))
    p.set_defaults(func=cmd_compare)

    p = sub.add_parser("pin", help="pin disagreements as regression cases")
    p.add_argument("--disagreements", default=os.path.join(DEFAULT_DIR, "disagreements.json"))
    p.add_argument("--cases", default=os.path.join(DEFAULT_DIR, "cases.json"))
    p.add_argument("--out", default=None)
    p.set_defaults(func=cmd_pin)

    p = sub.add_parser("replay", help="re-run the live classifier on pinned cases")
    p.add_argument("--cases", default=os.path.join(DEFAULT_DIR, "cases.json"))
    p.add_argument("--id", action="append")
    p.add_argument("--drip", default="drip")
    p.add_argument("--max-iterations", type=int, default=1)
    p.add_argument("--timeout", type=int, default=900)
    p.add_argument("--home", default=None)
    p.add_argument("--keep", action="store_true")
    p.add_argument("--write", action="store_true", help="write status/lastRun back into the cases file")
    p.add_argument("--dry-run", action="store_true", help="print the command each case would run")
    p.set_defaults(func=cmd_replay)

    p = sub.add_parser("report", help="match rate and per-skill precision/recall")
    p.add_argument("--survey", default=os.path.join(DEFAULT_DIR, "survey.json"))
    p.add_argument("--key", default=os.path.join(DEFAULT_DIR, "key.json"))
    p.add_argument("--answers", default=os.path.join(DEFAULT_DIR, "answers.json"))
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=cmd_report)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
