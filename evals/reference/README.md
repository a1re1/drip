# REFERENCE benchmark

Two layers that measure the REFERENCE tool — drip's hybrid search over an
[oasis](https://github.com/a1re1/o-cs)-indexed markdown corpus — so the wiring is
judged by numbers rather than by how good the tool description reads.

| layer | script | question it answers | model cost |
| --- | --- | --- | --- |
| retrieval fidelity | `retrieval_eval.py` | does searching *through drip's tool* retrieve as well as calling `oasis` directly? | none |
| agent effect | `agent_eval.py` | does having the tool actually change what the model answers? | one drip run per (arm, question) + one judge run per arm |

Both read their gold data from the [o-cs](https://github.com/a1re1/o-cs) checkout
(read-only) and are python3 stdlib only. Paths default to
`~/src/o-cs/.worktrees/9528de83`; override with `--corpus` / `--queries` /
`--questions`. Missing corpus, missing gold file or a missing `oasis`/`drip`
binary exits 2 with a message, never a traceback.

## Layer 1 — retrieval fidelity (free, deterministic)

```sh
python3 evals/reference/retrieval_eval.py --limit 100 --mode lexical --jobs 4
```

For each sampled gold query it retrieves twice — once with `oasis … search --json`
directly, once through `cargo run --example reference_probe`, which calls the real
`reference::execute` the tool pack registers — and reports recall@1/3/5/k and MRR@k
for both sides plus the **parity rate**: the fraction of queries whose ranked path
lists are identical.

Read it this way: the recall columns describe the *corpus and engine*, the delta and
parity columns describe *drip's wiring*. A parity below 1.0 means the tool path lost
or reordered hits the engine returned — argument marshalling, `k` handling, snippet
shaping — and is the finding worth chasing. Identical recall with parity 1.0 means
the harness adds no retrieval loss, which is the intended result.

Sampling is `random.Random(--seed)` over the query file, so a run is reproducible;
`--limit 0` runs all 1312 queries.

## Layer 2 — does the wiring help the model? (spends tokens)

```sh
python3 evals/reference/agent_eval.py --dry-run      # print the plan, spend nothing
python3 evals/reference/agent_eval.py --limit 3      # a cheap real run
python3 evals/reference/agent_eval.py                # the full experiment
```

Three arms over the same questions:

| arm | drip flags | what it isolates |
| --- | --- | --- |
| `control` | *(none)* | the model answering from memory |
| `tool` | `--reference-root <corpus>` | the effect of the tool being in the pack |
| `tool_skill` | `+ --skill cs-reference` | the extra effect of priming the search/cite loop |

Each (arm, question, repeat) is one drip run that writes its answer to its own file
under `results/answers/<arm>/`, so parallel runs (`--jobs`) never interleave and a
re-run reuses cached answers unless `--refresh` is passed. Each answer keeps a
`.meta.json` sidecar with the run's exit code and its measured REFERENCE call count,
so a cached run still reports tool use instead of "unknown".

The arms run against **this checkout's** `drip` build (`cargo build --bin drip`, then
`target/debug/drip`), never a globally installed one — an installed binary without
`--reference-root` would fail every tool arm with a usage error that reads like a model
failure. Override with `--drip-bin`.

Four metrics per arm, and the **lift** between arms is the actual result:

- **tool-use rate** — counted from the run's session transcript (`tool-call` events
  named `REFERENCE`), not from the model's self-report. When a transcript cannot be
  located the run is reported as unknown rather than assumed.
- **citation hit rate** — did it cite one of the gold pages (compared by page slug)?
- **point recall** — fraction of the gold's expected points present as substrings.
  A strict floor: a correct paraphrase scores 0, so read it as a lower bound.
- **judge score** — a second model (`--judge-profile`, default `kimi-k3`, deliberately
  a different model from the answering `--answer-profile`) scores each answer on
  grounded / accurate / complete and writes one JSON verdict per answer under
  `results/judgments/<arm>/`. Unparseable verdicts are dropped and counted in
  `judge_parse_failures` rather than silently scored 0.

Cost is `arms × questions × repeats` answer runs plus one judge run per arm — with
the defaults (3 arms, 12 questions, 1 repeat) that is 36 short GLM-Flash runs and 3
judge runs. `--dry-run` prints every command first.

## Results

Both layers write `results/latest-<layer>.json` (full per-query / per-answer detail,
git-ignored — it is regenerated per run and reaches megabytes on the full query set)
and append one line to the tracked `results/history.jsonl`, carrying the provenance that
makes runs comparable: timestamp, git sha, `drip --version`, corpus path, sha1 of the gold
file, and the run's knobs. Editing the gold file changes its sha1, so a number is
always traceable to the query set that produced it.

## What the first full run found (12 questions, GLM-5.3-Flash, judge kimi-k3)

| arm | tool use | citation hit | point recall | judge |
| --- | --- | --- | --- | --- |
| control | 0.00 | 0.00 | 0.49 | 1.00 |
| tool | 1.00 | 1.00 | 0.54 | 1.00 |
| tool_skill | 1.00 | 1.00 | 0.54 | 1.00 |

Read it honestly: the tool changes **grounding**, not correctness. Given the corpus the
model searches it every time and cites a gold page every time (0 → 1.00 on both), and
point recall rises slightly (+0.05) — but the judge scores every arm 1.00, because this
model already answers these twelve textbook questions correctly from memory. The judge
metric is at its ceiling here and cannot discriminate; a harder question set (or
questions whose answers are not in a general model's memory) is what would make it
informative. The `cs-reference` skill adds nothing over the bare tool on this set.

An earlier version of the judge rubric awarded `grounded` partly for citing a page —
which only the tool arms are asked to do — and reported a +0.33 lift that was an
artifact of the rubric, not of the tool. The rubric now judges the answer text alone.

## Caveats

- Point recall is exact-substring matching — a floor, not an answer-quality score.
  That is exactly why the judge exists; when the two disagree, trust the judge and
  read the answers.
- The judge is a model. It is a different model from the answerer to avoid
  self-preference, but it is not ground truth — and on an easy question set it
  saturates at 1.00 for every arm, which is a property of the questions, not of the
  tool. Never grade an arm on something only that arm is instructed to do.
- Layer 1 needs the o-cs checkout and the `oasis` binary; layer 2 additionally needs
  `drip` on PATH and API credentials for both profiles.
- The corpus is a moving target: re-run after ingesting pages, and compare against
  `history.jsonl` rather than against remembered numbers.
