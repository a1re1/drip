#!/usr/bin/env python3
"""Layer 2 of the REFERENCE benchmark: does wiring the corpus in actually help?

Runs the same gold questions through drip once per arm and compares the arms:

  control     drip with no corpus configured — answers from parametric memory
  tool        drip --reference-root <corpus> — REFERENCE is in the tool pack
  tool_skill  the above plus --skill cs-reference — the search/cite loop is primed

Each (arm, question, repeat) is one drip run writing its answer to its own JSON
file, so parallel runs never interleave. Tool use is read from the run's session
transcript (actual REFERENCE tool calls), not from the model's self-report.
Answers are then scored on citation hit rate, substring point recall, and — since
substring matching scores a correct paraphrase 0 — a second model's judgement.

Everything here is python3 stdlib. Usage:

    python3 evals/reference/agent_eval.py --dry-run        # print the plan, spend nothing
    python3 evals/reference/agent_eval.py --limit 3        # a cheap real run
    python3 evals/reference/agent_eval.py                  # the full experiment
"""

import argparse
import concurrent.futures
import datetime as dt
import hashlib
import json
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)

import corpus  # noqa: E402  (path set above)
from grade import citation_hit, mean, point_recall  # noqa: E402

OCS = corpus.ocs_root()
DEFAULT_CORPUS = corpus.default_corpus(OCS)
DEFAULT_QUESTIONS = os.path.join(OCS, "evals", "llm_eval", "questions.jsonl") if OCS else ""
RESULTS = os.path.join(HERE, "results")
ARMS = ("control", "tool", "tool_skill")
JUDGE_KEYS = ("grounded", "accurate", "complete")

ANSWER_PROMPT = """Answer one computer-science question, then write the answer to a file.

Question ({id}): {question}

{guidance}

Definition of done (all required):
1. The file {outfile} exists and contains exactly ONE compact JSON object with the keys
   id, answer, pages_cited, used_reference — write it with a BASH heredoc.
2. "answer" is 2-3 sentences answering the question. "pages_cited" is a JSON array of
   corpus page paths (e.g. "concepts/bm25.md"); never invent one.
3. Verify with: python3 -c "import json;d=json.load(open('{outfile}'));print(sorted(d))"

Scope: write ONLY {outfile}. Change no other file. Do not commit."""

ANSWER_GUIDANCE = {
    True: (
        "You have the REFERENCE tool over a computer-science corpus. Search it first"
        " (action \"search\", k 5), read the page paths it returns, and cite the paths you"
        " used in pages_cited. Set used_reference to true. Cite only paths REFERENCE"
        " actually returned."
    ),
    False: (
        "Answer from your own knowledge. Set pages_cited to [] and used_reference to false."
    ),
}

JUDGE_PROMPT = """Grade {n} answers to computer-science questions. Read-and-report task: you
only write the judgement files listed below.

For EACH pair below, in order:
1. Read the gold question file and the answer file with READ.
2. Decide three booleans about the answer, using the gold expect_points as the reference.
   Judge ONLY the answer text. Ignore whether it cites a page — arms differ in whether
   they are asked to cite, and citation is scored separately; rewarding it here would
   compare instructions rather than answer quality.
   grounded  — 1 if every claim is supported by or consistent with the expected points,
               0 if it asserts something the gold contradicts or invents specifics.
   accurate  — 1 if the substance is correct (a correct paraphrase counts; do not require
               the gold's exact wording), 0 otherwise.
   complete  — 1 if it covers the main expected points, 0 if it misses most of them.
3. IMMEDIATELY write that judgement to its output file with a BASH heredoc, as one compact
   JSON object with EXACTLY these keys: {{"id":"<id>","grounded":0|1,"accurate":0|1,
   "complete":0|1,"note":"<at most 20 words>"}}. Then move to the next pair — do not hold
   them all in memory.

Gold questions file (one JSON object per line, id/question/expect_pages/expect_points):
{gold}

Pairs (answer file -> judgement file):
{pairs}

Definition of done: every judgement file listed above exists and parses as JSON with the
four keys. Verify at the end with one python3 -c that loads them all and prints ok.

Scope: write ONLY the judgement files listed. Change no other file. Do not commit."""


# ---------------------------------------------------------------------------
# drip runs
# ---------------------------------------------------------------------------


def arm_flags(arm, corpus):
    """The drip flags that define an arm."""
    if arm == "control":
        return []
    if arm == "tool":
        return ["--reference-root", corpus]
    if arm == "tool_skill":
        return ["--reference-root", corpus, "--skill", "cs-reference"]
    raise ValueError(f"unknown arm {arm!r}")


def resolve_drip(explicit=None):
    """The drip binary under evaluation.

    Defaults to this checkout's build, never the globally installed drip: the
    experiment is about the REFERENCE wiring in THIS tree, and an installed
    binary without --reference-root would fail every tool arm with a usage
    error that looks like a model failure.
    """
    if explicit:
        return explicit
    # debug first: the preflight below rebuilds it, so it is the only build
    # guaranteed to match the working tree (a stale release build would fail
    # every tool arm on an unknown --reference-root).
    for build in ("debug", "release"):
        candidate = os.path.join(REPO, "target", build, "drip")
        if os.path.exists(candidate):
            return candidate
    return shutil.which("drip") or "drip"


def drip_command(prompt, arm, corpus, profile, max_iterations, drip_bin="drip"):
    return (
        [drip_bin, "--json", "--max-iterations", str(max_iterations), "--profile", profile]
        + arm_flags(arm, corpus)
        + [prompt]
    )


def child_env(arm):
    """A control run must not inherit a corpus from the operator's shell."""
    env = dict(os.environ)
    if arm == "control":
        for name in ("DRIP_REFERENCE_ROOTS", "OASIS_ROOTS"):
            env.pop(name, None)
    return env


def run_drip(command, env, timeout):
    """Run one drip goal; return (result_line_dict_or_None, exit_code)."""
    try:
        completed = subprocess.run(
            command, cwd=REPO, env=env, capture_output=True, text=True, timeout=timeout
        )
    except subprocess.TimeoutExpired:
        return None, 124

    result = None
    for line in completed.stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        if payload.get("type") == "result":
            result = payload
    return result, completed.returncode


def transcript_path(session_id, cwd=REPO):
    """~/.drip/projects/<cwd with '/' replaced by '-'>/sessions/<id>/transcript.jsonl."""
    slug = os.path.abspath(cwd).replace("/", "-")
    return os.path.expanduser(
        os.path.join("~/.drip/projects", slug, "sessions", session_id, "transcript.jsonl")
    )


def count_reference_calls(result):
    """Actual REFERENCE tool calls in a finished run's transcript.

    Returns None when the transcript cannot be located — the caller reports that
    as unknown rather than substituting the model's self-report.
    """
    if not result:
        return None
    path = result.get("transcriptPath")
    session_id = result.get("sessionId")
    if not path and session_id:
        path = transcript_path(session_id)
    if not path or not os.path.exists(path):
        return None

    calls = 0
    with open(path) as handle:
        for line in handle:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("kind") == "tool-call" and (event.get("data") or {}).get("toolName") == "REFERENCE":
                calls += 1
    return calls


# ---------------------------------------------------------------------------
# One answer
# ---------------------------------------------------------------------------


def answer_one(question, arm, repeat, options):
    """Produce (or reuse) one arm's answer to one question."""
    outfile = os.path.join(RESULTS, "answers", arm, f"{question['id']}-{repeat}.json")

    record = {"id": question["id"], "arm": arm, "repeat": repeat, "outfile": outfile}
    if options.dry_run:
        command = drip_command(
            "<answer prompt>", arm, options.corpus, options.answer_profile, options.max_iterations, options.drip_bin
        )
        record["command"] = " ".join(command)
        return record
    os.makedirs(os.path.dirname(outfile), exist_ok=True)
    if os.path.exists(outfile) and not options.refresh:
        meta = load_meta(outfile)
        # A cached answer counts only when it was produced under the same
        # configuration; otherwise it would silently mix two experiments.
        if meta.get("config") == run_config(options):
            record.update(load_answer(outfile), cached=True)
            # The measured tool-use count lives beside the answer, so a cache
            # hit keeps the metric instead of reporting it as unknown.
            record.update(reference_calls=meta.get("reference_calls"), exit_code=meta.get("exit_code"))
            return record

    prompt = ANSWER_PROMPT.format(
        id=question["id"],
        question=question["question"],
        guidance=ANSWER_GUIDANCE[arm != "control"],
        outfile=outfile,
    )
    command = drip_command(
        prompt, arm, options.corpus, options.answer_profile, options.max_iterations, options.drip_bin
    )
    result, exit_code = run_drip(command, child_env(arm), options.timeout)
    record["exit_code"] = exit_code
    record["reference_calls"] = count_reference_calls(result)
    record["cached"] = False
    record.update(load_answer(outfile))
    save_meta(outfile, record, options)
    return record


def meta_path(answer_path):
    return answer_path[: -len(".json")] + ".meta.json"


def run_config(options):
    """The knobs an answer depends on; a cached answer from a different one is stale."""
    return {
        "answer_profile": options.answer_profile,
        "corpus": options.corpus,
        "max_iterations": options.max_iterations,
        "questions": options.questions,
    }


def save_meta(answer_path, record, options):
    """Persist what only the run itself knows, next to the answer it cached."""
    try:
        with open(meta_path(answer_path), "w") as handle:
            json.dump(
                {
                    "config": run_config(options),
                    "exit_code": record.get("exit_code"),
                    "reference_calls": record.get("reference_calls"),
                },
                handle,
            )
    except OSError:
        pass


def load_meta(answer_path):
    """The cached run's measured facts; unknown tool use stays None."""
    try:
        with open(meta_path(answer_path)) as handle:
            payload = json.load(handle)
    except (OSError, json.JSONDecodeError):
        return {"reference_calls": None}
    if not isinstance(payload, dict):
        return {"reference_calls": None}
    return {
        "config": payload.get("config"),
        "exit_code": payload.get("exit_code"),
        "reference_calls": payload.get("reference_calls"),
    }


def load_answer(path):
    """The model's answer object, or an empty answer when it never wrote one."""
    try:
        with open(path) as handle:
            payload = json.load(handle)
    except (OSError, json.JSONDecodeError):
        return {"answer": "", "pages_cited": [], "used_reference_reported": None, "wrote_answer": False}
    return {
        "answer": payload.get("answer") or "",
        "pages_cited": payload.get("pages_cited") or [],
        "used_reference_reported": payload.get("used_reference"),
        "wrote_answer": True,
    }


# ---------------------------------------------------------------------------
# The judge
# ---------------------------------------------------------------------------


def judge_arm(arm, records, options):
    """One drip run grades every answer of one arm; returns {id-repeat: score}."""
    pairs = []
    for record in records:
        if not options.dry_run and not record.get("wrote_answer"):
            continue
        judgement = os.path.join(RESULTS, "judgments", arm, f"{record['id']}-{record['repeat']}.json")
        if not options.dry_run:
            os.makedirs(os.path.dirname(judgement), exist_ok=True)
        pairs.append((record, judgement))
    if not pairs:
        if options.dry_run:
            print(f"  judge[{arm}]: no answers to grade")
        return {}, 0

    prompt = JUDGE_PROMPT.format(
        n=len(pairs),
        gold=options.questions,
        pairs="\n".join(f"  {record['outfile']} -> {judgement}" for record, judgement in pairs),
    )
    # Verdicts are read back from fixed paths, so clear the arm's previous ones:
    # a judge run that dies half way must not have an older sweep averaged in.
    if not options.dry_run:
        for _, judgement in pairs:
            try:
                os.remove(judgement)
            except FileNotFoundError:
                pass

    command = drip_command(
        prompt, "control", options.corpus, options.judge_profile, options.judge_iterations, options.drip_bin
    )
    if options.dry_run:
        print(f"  judge[{arm}]: " + " ".join(command[:-1]) + f" '<judge prompt for {len(pairs)} answers>'")
        return {}, 0

    _, judge_exit = run_drip(command, child_env("control"), options.judge_timeout)
    if judge_exit != 0:
        print(f"  judge[{arm}]: drip exited {judge_exit}; only the verdicts it wrote are scored")

    scores, failures = {}, 0
    for record, judgement in pairs:
        try:
            with open(judgement) as handle:
                payload = json.load(handle)
            if not isinstance(payload, dict):
                raise TypeError("judgement is not a JSON object")
            scores[key_of(record)] = mean([float(bool(payload[k])) for k in JUDGE_KEYS])
        except (OSError, json.JSONDecodeError, KeyError, TypeError, ValueError):
            failures += 1
    return scores, failures


def key_of(record):
    return f"{record['id']}-{record['repeat']}"


# ---------------------------------------------------------------------------
# Aggregation and reporting
# ---------------------------------------------------------------------------


def score_arm(records, gold, judge_scores):
    """Aggregate one arm's records into the reported metrics.

    Runs that never wrote an answer (drip crashed, timed out, or refused) are
    counted as failures and excluded from the answer-quality metrics: scoring
    them 0 would blame the model for a harness problem.
    """
    answered = [record for record in records if record.get("wrote_answer")]
    tool_use, citations, recalls, judged = [], [], [], []
    for record in answered:
        question = gold[record["id"]]
        calls = record.get("reference_calls")
        if calls is not None:
            tool_use.append(1.0 if calls > 0 else 0.0)
        citations.append(1.0 if citation_hit(record["pages_cited"], question["expect_pages"]) else 0.0)
        recalls.append(point_recall(record["answer"], question.get("expect_points") or []))
        score = judge_scores.get(key_of(record))
        if score is not None:
            judged.append(score)

    return {
        "n": len(records),
        "answered": len(answered),
        "failed_runs": len(records) - len(answered),
        "tool_use_rate": mean(tool_use) if tool_use else None,
        "tool_use_known": len(tool_use),
        "citation_hit_rate": mean(citations) if citations else None,
        "point_recall": mean(recalls) if recalls else None,
        "judge_score": mean(judged) if judged else None,
        "judged": len(judged),
    }


METRICS = (
    ("tool_use_rate", "tool-use rate"),
    ("citation_hit_rate", "citation hit"),
    ("point_recall", "point recall"),
    ("judge_score", "judge score"),
)


def cell(value):
    return "  n/a" if value is None else f"{value:5.2f}"


def print_report(summary, arms):
    print(f"\n{'arm':<12}{'n':>4}  " + "  ".join(f"{label:>14}" for _, label in METRICS))
    print("-" * (16 + 16 * len(METRICS)))
    for arm in arms:
        row = summary[arm]
        cells = "  ".join(f"{cell(row[key]):>14}" for key, _ in METRICS)
        print(f"{arm:<12}{row['n']:>4}  {cells}")
    for arm in arms:
        row = summary[arm]
        notes = [f"{row['answered']}/{row['n']} answered"]
        if row["failed_runs"]:
            notes.append(f"{row['failed_runs']} run(s) produced no answer")
        notes.append(f"tool use known for {row['tool_use_known']}")
        notes.append(f"judged {row['judged']}")
        print(f"  {arm}: " + ", ".join(notes))

    print("\nlift")
    for later, earlier in (("tool", "control"), ("tool_skill", "tool")):
        if later not in summary or earlier not in summary:
            continue
        deltas = []
        for key, label in METRICS:
            after, before = summary[later][key], summary[earlier][key]
            deltas.append(f"{label} {after - before:+.2f}" if after is not None and before is not None else f"{label} n/a")
        print(f"  {later} - {earlier}: " + ", ".join(deltas))


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def load_questions(path, limit):
    questions = []
    with open(path) as handle:
        for line in handle:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            questions.append(json.loads(line))
    return questions[:limit] if limit else questions


def sha1_of(path):
    with open(path, "rb") as handle:
        return hashlib.sha1(handle.read()).hexdigest()


def command_output(command):
    try:
        return subprocess.run(command, cwd=REPO, capture_output=True, text=True, timeout=30).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--arms", default=",".join(ARMS), help="comma-separated arms to run")
    parser.add_argument("--questions", default=DEFAULT_QUESTIONS)
    parser.add_argument("--corpus", default=DEFAULT_CORPUS)
    parser.add_argument("--limit", type=int, default=0, help="first N questions (0 = all)")
    parser.add_argument("--repeats", type=int, default=1, help="runs per question, for variance")
    parser.add_argument("--jobs", type=int, default=3, help="concurrent drip runs")
    parser.add_argument("--answer-profile", default="glm-5-3-flash")
    parser.add_argument("--judge-profile", default="kimi-k3", help="a different model from the answerer")
    parser.add_argument("--max-iterations", type=int, default=8)
    parser.add_argument("--judge-iterations", type=int, default=30)
    parser.add_argument("--timeout", type=int, default=900, help="seconds per answer run")
    parser.add_argument("--judge-timeout", type=int, default=1800)
    parser.add_argument("--skip-judge", action="store_true")
    parser.add_argument("--refresh", action="store_true", help="re-run cached answers")
    parser.add_argument("--dry-run", action="store_true", help="print the plan and exit without spending tokens")
    parser.add_argument("--drip-bin", default=None, help="drip binary to evaluate (default: this checkout's build)")
    parser.add_argument("--json", action="store_true", dest="as_json")
    return parser.parse_args(argv)


def main(argv=None):
    options = parse_args(argv)
    arms = [arm.strip() for arm in options.arms.split(",") if arm.strip()]
    for arm in arms:
        if arm not in ARMS:
            print(f"unknown arm {arm!r}; expected any of {', '.join(ARMS)}", file=sys.stderr)
            return 2
    for path, label in ((options.questions, "questions file"), (options.corpus, "corpus")):
        if not path:
            print(corpus.missing_message(label), file=sys.stderr)
            return 2
        if not os.path.exists(path):
            print(f"{label} not found: {path}", file=sys.stderr)
            return 2
    if not options.dry_run:
        # Build the tree under evaluation, so the tool arms actually have
        # --reference-root rather than failing with a usage error.
        build = subprocess.run(["cargo", "build", "--quiet", "--bin", "drip"], cwd=REPO, capture_output=True, text=True)
        if build.returncode != 0:
            print(f"cargo build --bin drip failed:\n{build.stderr.strip()[:800]}", file=sys.stderr)
            return 2
    options.drip_bin = resolve_drip(options.drip_bin)
    if not options.dry_run:
        if not os.path.exists(options.drip_bin) and shutil.which(options.drip_bin) is None:
            print(f"drip binary not found: {options.drip_bin}", file=sys.stderr)
            return 2
    print(f"drip binary: {options.drip_bin}")

    questions = load_questions(options.questions, options.limit)
    for question in questions:
        missing = [field for field in ("id", "question", "expect_pages") if field not in question]
        if missing:
            print(f"gold row {question.get('id', '?')} is missing {', '.join(missing)}", file=sys.stderr)
            return 2
    gold = {question["id"]: question for question in questions}
    plan = [
        (question, arm, repeat)
        for arm in arms
        for question in questions
        for repeat in range(1, options.repeats + 1)
    ]
    print(
        f"{len(plan)} answer runs ({len(arms)} arms x {len(questions)} questions x {options.repeats} repeats)"
        + ("" if options.skip_judge else f" + {len(arms)} judge runs")
    )

    if options.dry_run:
        for question, arm, repeat in plan:
            record = answer_one(question, arm, repeat, options)
            print(f"  {arm:<11} {question['id']}#{repeat}: {record['command']}")
        if not options.skip_judge:
            for arm in arms:
                planned = [
                    answer_one(question, arm, repeat, options)
                    for question in questions
                    for repeat in range(1, options.repeats + 1)
                ]
                judge_arm(arm, planned, options)
        return 0

    records = {arm: [] for arm in arms}
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, options.jobs)) as pool:
        futures = {
            pool.submit(answer_one, question, arm, repeat, options): arm
            for question, arm, repeat in plan
        }
        for future in concurrent.futures.as_completed(futures):
            record = future.result()
            records[record["arm"]].append(record)
            print(f"  {record['arm']:<11} {record['id']}#{record['repeat']} done", flush=True)

    summary, judge_failures = {}, 0
    for arm in arms:
        records[arm].sort(key=lambda record: (record["id"], record["repeat"]))
        scores, failures = ({}, 0) if options.skip_judge else judge_arm(arm, records[arm], options)
        judge_failures += failures
        summary[arm] = score_arm(records[arm], gold, scores)

    run = {
        "at": dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
        "layer": "agent",
        "git_sha": command_output(["git", "rev-parse", "HEAD"]),
        "drip_bin": options.drip_bin,
        "drip_version": command_output([options.drip_bin, "--version"]),
        "corpus": options.corpus,
        "questions_file": options.questions,
        "questions_file_sha": sha1_of(options.questions),
        "arms": arms,
        "answer_profile": options.answer_profile,
        "judge_profile": None if options.skip_judge else options.judge_profile,
        "repeats": options.repeats,
        "judge_parse_failures": judge_failures,
        "summary": summary,
    }

    os.makedirs(RESULTS, exist_ok=True)
    with open(os.path.join(RESULTS, "latest-agent.json"), "w") as handle:
        json.dump({**run, "records": records}, handle, indent=2, sort_keys=True)
    with open(os.path.join(RESULTS, "history.jsonl"), "a") as handle:
        handle.write(json.dumps(run, sort_keys=True) + "\n")

    if options.as_json:
        print(json.dumps(run, indent=2, sort_keys=True))
    else:
        print_report(summary, arms)
        if judge_failures:
            print(f"\n{judge_failures} judgement(s) could not be parsed and were dropped")
    return 0


if __name__ == "__main__":
    sys.exit(main())
