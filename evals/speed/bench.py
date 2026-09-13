#!/usr/bin/env python3
"""Speed benchmark for the drip harness.

Runs each task in evals/speed/tasks/tasks.json against a fresh copy of the
fixture project, then grades the result with a hidden acceptance test the agent
never saw. Records wall time, cycles, loops, model calls, tool time, and gate
rejections from the session transcript so harness changes can be compared with
numbers rather than impressions.

  python3 evals/speed/bench.py --label baseline            # run everything
  python3 evals/speed/bench.py --label x --tasks page-bug   # one task
  python3 evals/speed/bench.py --compare baseline x         # side by side

Results accumulate in evals/speed/results/<label>.json (one record per run).
Stdlib only.
"""
import argparse
import concurrent.futures
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURE = os.path.join(HERE, "fixture")
TASKS = os.path.join(HERE, "tasks", "tasks.json")
HIDDEN = os.path.join(HERE, "tasks", "hidden")
RESULTS = os.path.join(HERE, "results")
UNITTEST = ["python3", "-m", "unittest", "discover", "-s", "tests", "-q"]


def load_tasks(selector):
    tasks = json.load(open(TASKS))
    if selector and selector != "all":
        wanted = set(selector.split(","))
        tasks = [t for t in tasks if t["id"] in wanted]
        missing = wanted - {t["id"] for t in tasks}
        if missing:
            sys.exit(f"unknown task(s): {', '.join(sorted(missing))}")
    return tasks


def make_workspace(root, task_id, repeat):
    ws = os.path.join(root, f"{task_id}-{repeat}")
    if os.path.exists(ws):
        shutil.rmtree(ws)
    shutil.copytree(FIXTURE, ws)
    git = lambda *a: subprocess.run(["git", *a], cwd=ws, check=True, capture_output=True)
    git("init", "-q")
    git("-c", "user.name=bench", "-c", "user.email=bench@example.com", "add", "-A")
    git("-c", "user.name=bench", "-c", "user.email=bench@example.com", "commit", "-q", "-m", "fixture")
    return ws


def find_transcripts(project_dir):
    found = []
    for dirpath, _, files in os.walk(project_dir):
        if "transcript.jsonl" in files:
            found.append(os.path.join(dirpath, "transcript.jsonl"))
    return found


def transcript_metrics(path):
    m = dict(inferences=0, inference_ms=0, tool_calls=0, tool_ms=0, loops=0, cycles=0,
             rejections=0, read_only_nudges=0, output_cutoffs=0, patches=0, verifies=0,
             context_expired=0, tasks_finished=0, prompt_tokens=0, completion_tokens=0,
             cache_read_tokens=0, rejection_reasons=[], bash_ms=0)
    for line in open(path, errors="ignore"):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if d.get("type") != "event":
            continue
        kind = d.get("kind")
        data = d.get("data") or {}
        detail = d.get("detail") or ""
        if kind == "inference":
            m["inferences"] += 1
            m["inference_ms"] += data.get("latencyMs") or 0
            m["prompt_tokens"] += data.get("promptTokens") or 0
            m["completion_tokens"] += data.get("completionTokens") or 0
            m["cache_read_tokens"] += data.get("cacheReadTokens") or 0
        elif kind == "tool-result":
            m["tool_calls"] += 1
            m["tool_ms"] += data.get("durationMs") or 0
            name = data.get("toolName")
            if name == "PATCH":
                m["patches"] += 1
            elif name in ("VERIFY", "CHECK"):
                m["verifies"] += 1
            elif name == "BASH":
                m["bash_ms"] += data.get("durationMs") or 0
        elif kind == "loop-start":
            m["loops"] += 1
        elif kind == "iteration-start":
            m["cycles"] += 1
        elif kind == "task-finished":
            m["tasks_finished"] += 1
        elif kind == "context-expired":
            m["context_expired"] += 1
        elif kind == "harness-op" and detail.startswith("finish_task: harness: not accepted"):
            m["rejections"] += 1
            m["rejection_reasons"].append(detail[len("finish_task: harness: not accepted yet — "):][:120])
        elif kind == "run-warning":
            if detail.startswith("read-only nudge"):
                m["read_only_nudges"] += 1
            elif "output-token cap" in detail:
                m["output_cutoffs"] += 1
    return m


def grade(ws, task):
    """Return (hidden_pass, original_pass, output)."""
    orig = subprocess.run(UNITTEST, cwd=ws, capture_output=True, text=True, timeout=120)
    hidden_src = os.path.join(HIDDEN, task["hidden"])
    hidden_dst = os.path.join(ws, "tests", f"test_hidden_{task['id'].replace('-', '_')}.py")
    shutil.copy(hidden_src, hidden_dst)
    res = subprocess.run(["python3", "-m", "unittest", "-q", os.path.relpath(hidden_dst, ws).replace("/", ".")[:-3]],
                         cwd=ws, capture_output=True, text=True, timeout=120)
    os.remove(hidden_dst)
    tail = (res.stdout + res.stderr).strip().splitlines()[-6:]
    return res.returncode == 0, orig.returncode == 0, "\n".join(tail)


ROLE_SECONDS = (("planner", "plan_s"), ("author", "author_s"), ("reviewer", "review_s"))


def extract_role_inference(stdout):
    """Return the roleInference map from the final run-end record of drip --json stdout.

    Legacy stdout without roleInference (or empty/broken output) yields {}.
    """
    ri = {}
    for line in stdout.splitlines():
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if isinstance(d, dict) and isinstance(d.get("roleInference"), dict):
            ri = d["roleInference"]
    return ri


def role_seconds(role_inference):
    """Map role latencyMs into per-run seconds; absent roles map to 0."""
    out = {}
    for role, field in ROLE_SECONDS:
        ms = (role_inference.get(role) or {}).get("latencyMs") or 0
        out[field] = round(ms / 1000, 1)
    return out


def run_one(task, repeat, opts, root):
    ws = make_workspace(root, task["id"], repeat)
    project_dir = os.path.join(ws, ".dripdata")
    cmd = [opts.drip, "--json", "--prompt", task["goal"], "--max-iterations", str(opts.max_iterations),
           "--project-dir", project_dir, "--no-repo-memory"] + opts.extra
    env = dict(os.environ)
    env.pop("DRIP_PROJECT_DIR", None)
    started = time.time()
    try:
        proc = subprocess.run(cmd, cwd=ws, capture_output=True, text=True, timeout=opts.timeout, env=env)
        stdout, timed_out, rc = proc.stdout, False, proc.returncode
    except subprocess.TimeoutExpired as exc:
        stdout, timed_out, rc = (exc.stdout or b"").decode("utf-8", "ignore") if isinstance(exc.stdout, bytes) else (exc.stdout or ""), True, None
    wall = time.time() - started
    result = None
    for line in reversed(stdout.splitlines()):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if d.get("type") == "result":
            result = d
            break
    metrics = {}
    for tp in find_transcripts(project_dir):
        tm = transcript_metrics(tp)
        if not metrics or tm["inferences"] > metrics.get("inferences", 0):
            metrics = tm
    hidden_pass, orig_pass, grade_out = grade(ws, task)
    record = dict(label=opts.label, task=task["id"], size=task["size"], repeat=repeat, wall_s=round(wall, 1),
                  timed_out=timed_out, exit_code=rc, reason=(result or {}).get("reason"),
                  hidden_pass=hidden_pass, original_pass=orig_pass, grade_tail=grade_out,
                  workspace=ws, drip_version=opts.drip_version, extra=opts.extra, started_at=started,
                  max_iterations=opts.max_iterations, **role_seconds(extract_role_inference(stdout)), **metrics)
    if not opts.keep:
        shutil.rmtree(ws, ignore_errors=True)
    return record


def save(records, label):
    os.makedirs(RESULTS, exist_ok=True)
    path = os.path.join(RESULTS, f"{label}.json")
    existing = json.load(open(path)) if os.path.exists(path) else []
    existing.extend(records)
    json.dump(existing, open(path, "w"), indent=1)
    return path


def load(label):
    path = os.path.join(RESULTS, f"{label}.json")
    if not os.path.exists(path):
        sys.exit(f"no results for label {label!r} at {path}")
    return json.load(open(path))


def summarize(records):
    """Aggregate a label: pass rate, and medians of the cost metrics."""
    by_task = {}
    for r in records:
        by_task.setdefault(r["task"], []).append(r)
    out = {}
    for task, rs in sorted(by_task.items()):
        med = lambda k: statistics.median([r.get(k) or 0 for r in rs])
        out[task] = dict(n=len(rs), pass_rate=sum(1 for r in rs if r["hidden_pass"]) / len(rs),
                         wall_s=med("wall_s"), cycles=med("cycles"), loops=med("loops"), inferences=med("inferences"),
                         inference_min=round(med("inference_ms") / 60000, 1), tool_calls=med("tool_calls"),
                         rejections=med("rejections"), completed=sum(1 for r in rs if r["reason"] == "completed") / len(rs))
    total = dict(n=len(records), pass_rate=sum(1 for r in records if r["hidden_pass"]) / max(1, len(records)),
                 wall_s=statistics.median([r["wall_s"] for r in records]) if records else 0,
                 cycles=statistics.median([r.get("cycles") or 0 for r in records]) if records else 0,
                 inferences=statistics.median([r.get("inferences") or 0 for r in records]) if records else 0,
                 rejections=statistics.median([r.get("rejections") or 0 for r in records]) if records else 0,
                 sum_wall_min=round(sum(r["wall_s"] for r in records) / 60, 1))
    return out, total


def print_table(label, records):
    per, total = summarize(records)
    print(f"\n== {label}: {total['n']} runs, pass {total['pass_rate']:.0%}, median wall {total['wall_s']:.0f}s, "
          f"median cycles {total['cycles']:.0f}, median inferences {total['inferences']:.0f}, "
          f"median rejections {total['rejections']:.0f}, total wall {total['sum_wall_min']} min")
    print(f"{'task':18} {'n':>2} {'pass':>5} {'done':>5} {'wall_s':>7} {'cyc':>4} {'loops':>5} {'inf':>4} {'inf_min':>7} {'tools':>5} {'rej':>4}")
    for task, s in per.items():
        print(f"{task:18} {s['n']:>2} {s['pass_rate']:>5.0%} {s['completed']:>5.0%} {s['wall_s']:>7.0f} {s['cycles']:>4.0f} "
              f"{s['loops']:>5.0f} {s['inferences']:>4.0f} {s['inference_min']:>7} {s['tool_calls']:>5.0f} {s['rejections']:>4.0f}")


def compare(a, b):
    ra, rb = load(a), load(b)
    print_table(a, ra)
    print_table(b, rb)
    pa, ta = summarize(ra)
    pb, tb = summarize(rb)
    print(f"\n== delta {b} vs {a}")
    for key in ("pass_rate", "wall_s", "cycles", "inferences", "rejections", "sum_wall_min"):
        va, vb = ta[key], tb[key]
        rel = f"{(vb - va) / va:+.0%}" if va else "n/a"
        print(f"  {key:14} {va:>8.2f} -> {vb:>8.2f}  ({rel})")


def summarize_runs(runs):
    """Aggregate raw run dicts into one summary row per task.

    Pure: does not mutate or retain the input. Each row has task, runs,
    wall_s_median/min/max, inferences_median, hidden_pass_rate, rejections.
    """
    by_task = {}
    for r in runs:
        by_task.setdefault(r["task"], []).append(r)
    rows = []
    for task in sorted(by_task):
        rs = by_task[task]
        walls = [r["wall_s"] for r in rs]
        rows.append(dict(
            task=task,
            runs=len(rs),
            wall_s_median=statistics.median(walls),
            wall_s_min=min(walls),
            wall_s_max=max(walls),
            inferences_median=statistics.median([r.get("inferences") or 0 for r in rs]),
            hidden_pass_rate=sum(1 for r in rs if r["hidden_pass"]) / len(rs),
            rejections=sum(r.get("rejections") or 0 for r in rs),
            plan_s_median=statistics.median([r.get("plan_s") or 0 for r in rs]),
            author_s_median=statistics.median([r.get("author_s") or 0 for r in rs]),
            review_s_median=statistics.median([r.get("review_s") or 0 for r in rs]),
        ))
    return rows


def print_summary_runs(label, rows):
    print(f"\n== summary {label}")
    print(f"{'task':18} {'runs':>4} {'wall_med':>9} {'wall_min':>9} {'wall_max':>9} {'inf_med':>8} {'pass':>6} {'rej':>4} "
          f"{'plan_med':>9} {'auth_med':>9} {'rev_med':>9}")
    for s in rows:
        print(f"{s['task']:18} {s['runs']:>4} {s['wall_s_median']:>9.1f} {s['wall_s_min']:>9.1f} "
              f"{s['wall_s_max']:>9.1f} {s['inferences_median']:>8.1f} {s['hidden_pass_rate']:>6.0%} {s['rejections']:>4} "
              f"{s['plan_s_median']:>9.1f} {s['author_s_median']:>9.1f} {s['review_s_median']:>9.1f}")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--label", help="results label (results/<label>.json)")
    ap.add_argument("--tasks", default="all", help="comma-separated task ids, or all")
    ap.add_argument("--repeats", type=int, default=1)
    ap.add_argument("--jobs", type=int, default=3, help="concurrent drip runs")
    ap.add_argument("--max-iterations", type=int, default=30)
    ap.add_argument("--timeout", type=int, default=1800, help="seconds per run")
    ap.add_argument("--drip", default=shutil.which("drip") or "drip", help="drip binary")
    ap.add_argument("--extra", default="", help="extra drip flags, e.g. '--roles reviewed'")
    ap.add_argument("--root", default=None, help="workspace root (default: temp dir)")
    ap.add_argument("--keep", action="store_true", help="keep workspaces after grading")
    ap.add_argument("--compare", nargs=2, metavar=("A", "B"), help="compare two labels and exit")
    ap.add_argument("--show", metavar="LABEL", help="print a label's table and exit")
    ap.add_argument("--summary", metavar="LABEL", help="print per-task median/min/max summary for a label and exit")
    opts = ap.parse_args(argv)
    if opts.compare:
        return compare(*opts.compare)
    if opts.summary:
        return print_summary_runs(opts.summary, summarize_runs(load(opts.summary)))
    if opts.show:
        return print_table(opts.show, load(opts.show))
    if not opts.label:
        ap.error("--label is required to run")
    opts.extra = opts.extra.split() if opts.extra else []
    opts.drip_version = subprocess.run([opts.drip, "--version"], capture_output=True, text=True).stdout.strip()
    tasks = load_tasks(opts.tasks)
    root = opts.root or tempfile.mkdtemp(prefix="drip-speed-")
    os.makedirs(root, exist_ok=True)
    jobs = [(t, r) for r in range(opts.repeats) for t in tasks]
    print(f"{opts.drip_version} · {len(jobs)} runs · jobs={opts.jobs} · root={root} · extra={opts.extra}", flush=True)
    records = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=opts.jobs) as pool:
        futures = {pool.submit(run_one, t, r, opts, root): (t["id"], r) for t, r in jobs}
        for fut in concurrent.futures.as_completed(futures):
            rec = fut.result()
            records.append(rec)
            print(f"  {rec['task']:18} r{rec['repeat']} {rec['reason'] or 'timeout':14} wall={rec['wall_s']:.0f}s cycles={rec.get('cycles')} "
                  f"plan={rec.get('plan_s', 0):.1f}s author={rec.get('author_s', 0):.1f}s review={rec.get('review_s', 0):.1f}s "
            f"inf={rec.get('inferences')} rej={rec.get('rejections')} hidden={'PASS' if rec['hidden_pass'] else 'FAIL'} "
                  f"orig={'ok' if rec['original_pass'] else 'BROKEN'}", flush=True)
    path = save(records, opts.label)
    print_table(opts.label, records)
    print(f"\nsaved {path}")


if __name__ == "__main__":
    main()
