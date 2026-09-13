# Speed benchmark

`evals/speed/bench.py` measures how fast (and how well) the drip harness
completes a fixed set of coding tasks. For each task it runs `drip` against a
fresh copy of `evals/speed/fixture` (a small `kvstore` Python project), grades
the result with a hidden acceptance test the agent never saw, and records wall
time, cycles, loops, model calls, tool time, and gate rejections from the
session transcript. Results accumulate in `evals/speed/results/<label>.json`
(one JSON record per run), so harness changes can be compared with numbers
rather than impressions. Stdlib only.

## How a run works

1. **Fresh workspace** — `make_workspace()` copies `fixture/` to
   `<root>/<task>-<repeat>`, then `git init` + an initial commit so the agent
   starts from a clean tree. A new temp root is created per invocation unless
   `--root` is given; workspaces are deleted after grading unless `--keep`.
2. **Run** — `drip --json --prompt <goal> --max-iterations N --project-dir
   <ws>/.dripdata --no-repo-memory` (plus `--extra` flags) runs in the
   workspace, with a per-run timeout. `DRIP_PROJECT_DIR` is stripped from the
   environment.
3. **Metrics** — the run's `transcript.jsonl` under `.dripdata` is parsed for
   inference counts/latency, tool calls (PATCH/VERIFY/CHECK/BASH timings),
   loops, cycles, finish_task rejections, hedges, token counts, and role
   timings (planner/author/reviewer seconds from the final `roleInference`).
4. **Grade** — the pre-existing suite is run first
   (`python3 -m unittest discover -s tests -q`); then the hidden test file is
   copied into the workspace as `tests/test_hidden_<task_id>.py` and run via
   `python3 -m unittest tests.test_hidden_<task_id>` (grading times out after
   120s, which counts as a failure). The hidden test is removed afterwards.
   `hidden_pass` = hidden suite exit 0; `original_pass` = the fixture's own
   suite still passes (i.e. the agent didn't break existing behaviour).
5. **Save** — records are appended to `results/<label>.json` and a per-task
   table is printed.

## CLI flags

```
python3 evals/speed/bench.py --label NAME [options]
```

| Flag | Default | Meaning |
|---|---|---|
| `--label` | required to run | Results label; appends to `results/<label>.json`. |
| `--tasks` | `all` | Comma-separated task ids to run (unknown ids abort); `all` runs everything. |
| `--repeats` | `1` | Repetitions per task. |
| `--jobs` | `3` | Concurrent drip runs (thread pool). |
| `--max-iterations` | `30` | Passed to `drip --max-iterations`. |
| `--timeout` | `1800` | Seconds per run before it is killed (recorded as a timeout). |
| `--drip` | `drip` on PATH | Path to the drip binary. |
| `--extra` | `""` | Extra flags forwarded to drip, e.g. `--extra "--roles reviewed"`. |
| `--root` | temp dir | Workspace root directory. |
| `--keep` | off | Keep workspaces after grading (for debugging). |
| `--compare A B` | — | Print a two-label comparison table and exit. |
| `--show LABEL` | — | Print a label's per-run table and exit. |
| `--summary LABEL` | — | Print a per-task median/min/max summary and exit. |

A broken run (exception) is logged as `CRASHED` and skipped; the rest of the
batch and its results are still saved.

## Report columns

`--show` (per task, medians unless noted): `n` runs, `pass` (hidden pass
rate), `done` (runs that finished with reason `completed`), `wall_s` (median
wall-clock seconds), `cyc` (median iterations), `loops` (median task loops),
`inf` (median model inferences), `inf_min` (median total inference time, in
minutes), `tools` (median tool calls), `rej` (median finish_task rejections).
The header line adds overall pass rate, median wall/cycles/inferences/
rejections, and total wall time in minutes.

`--summary` (per task): `runs`, `wall_med`/`wall_min`/`wall_max` (seconds),
`inf_med` (median inferences), `pass` (hidden pass rate), `rej` (total
finish_task rejections), `plan_med`/`auth_med`/`rev_med` (median
planner/author/reviewer inference seconds from `roleInference`), `acache`/`rcache`
(median share of the author's / reviewer's prompt tokens served from the
prompt cache, from `cacheReadTokens` / `promptTokens`; drip 0.121+), `hedges`
(total hedged requests), `won` (hedges the second request won), `waived`
(runs whose review was waived).

`--compare A B` (per task, tasks present in either label): `A_wall`,
`B_wall` (median wall seconds; `-` if the task is missing from that label),
`delta%` (B wall relative to A wall), `A_inf`, `B_inf` (median inferences),
`A_pass`, `B_pass` (hidden pass rates).

## Adding a task

1. Append an entry to `evals/speed/tasks/tasks.json`:

```json
{
  "id": "my-task",          // unique id, used in --tasks and results
  "size": "S",              // S/M/L/XL, recorded but not enforced
  "goal": "Full prompt given to the agent. State the exact behaviour to
            implement, where the tests go, and to run
            `python3 -m unittest discover -s tests -q`.",
  "hidden": "my_task_test.py"  // file name under tasks/hidden/
}
```

2. Write `evals/speed/tasks/hidden/my_task_test.py`: a normal `unittest`
   module (see `page_bug_test.py` for an example) that imports from `kvstore`
   and asserts only the acceptance criteria in the goal. It must pass on a
   correct implementation and fail on the unfixed fixture, because it is
   copied into the workspace's `tests/` directory and run there. Keep the
   agent's own suite passing too — `original_pass` is recorded separately.

## Examples

```sh
python3 evals/speed/bench.py --label baseline                     # all tasks
python3 evals/speed/bench.py --label page --tasks page-bug        # one task
python3 evals/speed/bench.py --label v2 --repeats 3 --jobs 4 --extra "--roles reviewed"
python3 evals/speed/bench.py --show baseline                      # per-run table
python3 evals/speed/bench.py --summary baseline                   # medians/min/max
python3 evals/speed/bench.py --compare baseline v2                # side by side
```
