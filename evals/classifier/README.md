# Classifier benchmark

`bench.py` measures drip's skill classifier against blind operator judgment.
A skill's `classifiers.json` sidecar decides when the classifier composes that
skill into a loop; this harness tells you whether it *should* have — with a
match rate you can move by editing the questions, and pinned cases that keep a
fixed miss from coming back.

## Why

A `classifiers.json` question set is a small model someone has to tune. The only
honest signal is a context the classifier actually saw plus a human who did not
know the answer. Transcripts already hold both halves: every classified loop is
recorded as a `loop-start` event carrying `data.skills` (what matched) with the
goal and task text in the same session.

## How a pass works

1. **Harvest** — `harvest` scans `~/.drip/projects/*/sessions/*/transcript.jsonl`
   (plus any extra globs you pass) for `loop-start` events, pairs each with the
   goal text in force at that moment, and writes `harvest.json` plus the
   observed skill surface in `pool.json`.
2. **Blind survey** — `survey` samples N loops and writes `survey.json`, which
   carries the loop context and the candidate skill list but **not** what the
   classifier matched. The answer key goes to a separate file (`key.json`) that
   the skill's workflow tells the agent never to show the operator.
3. **Operator multiselect** — the agent shows one context at a time and asks
   with `ask_user` (`multiple: true`) which skills should be active for it,
   recording `{"answers": [{"caseId": ..., "picks": [...]}]}`.
4. **Compare** — `compare` scores picks against the key: agreement, missed
   skills (operator picked, classifier did not), spurious skills (classifier
   picked, operator did not), and the overall match rate.
5. **Pin** — `pin` appends every disagreement to `cases.json` as a regression
   case with `target` = the operator's picks and `targetSkill` = the skill whose
   questions should change.
6. **Replay** — `replay` shells out to `drip` once per pinned case in a throwaway
   workspace and reads the `loop-start` event out of the run's `--json` output.
   The pool rules (thresholds, the unauthored cap, authored formulas) stay in the
   product, so the harness never reimplements them. Each case reports
   `pass`/`fail` and updates `status`/`lastRun` in the cases file when `--write`.
7. **Improve** — edit the failing skill's `classifiers.json` questions or formula,
   re-run `replay --id <case>`, and keep the case pinned once it passes. The
   match rate before/after comes from `report`.

## CLI

```
python3 evals/classifier/bench.py harvest [--projects-dir DIR] [--transcripts GLOB]
                                        [--since ISO] [--limit N] [--out FILE] [--pool-out FILE]
python3 evals/classifier/bench.py survey  [--harvest FILE] [--pool-file FILE] [--n N]
                                        [--seed N] [--out FILE] [--key FILE]
python3 evals/classifier/bench.py compare [--survey FILE] [--key FILE] [--answers FILE] [--out FILE]
python3 evals/classifier/bench.py pin     [--disagreements FILE] [--cases FILE] [--out FILE]
python3 evals/classifier/bench.py replay  [--cases FILE] [--id ID] [--drip PATH] [--dry-run]
                                        [--max-iterations N] [--timeout S] [--write] [--keep]
python3 evals/classifier/bench.py report  [--survey FILE] [--key FILE] [--answers FILE] [--json]
```

`replay` exits non-zero when any case fails, so it works as a regression gate.

## Case schema

```json
{
  "id": "clf-1a2b3c4d5e",
  "pinnedAt": "2026-09-23T02:00:00Z",
  "kind": "false-negative",
  "targetSkill": "tdd",
  "target": ["tdd", "verify-before-done"],
  "matched": ["verify-before-done"],
  "missed": ["tdd"],
  "spurious": [],
  "context": {"goal": "...", "task": "...", "taskId": "task-1", "role": "author"},
  "source": {"sessionId": "...", "loop": 4, "at": "..."},
  "status": "open",
  "lastRun": null
}
```

`target` is the operator's blind pick — the thing the classifier should have
matched. A pinned case is a regression test only after `replay` reports `pass`
for it; `open`/`fail` cases are the tuning queue.

## Limits

- The harvested goal + task text is what the transcript records; the classifier
  also sees the loop's notes, memory and tool surface, so a case can look
  under-specified when replayed. That under-specification is part of the miss.
- `replay` performs a real run: it calls the classifier and burns one iteration
  of model time per case. Use `--id` to replay one case while tuning; batch runs
  are for the report.
- `--no-ask`/`--no-review` are pinned in the replay command so a case never
  stalls waiting for operator input.
