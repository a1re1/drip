---
name: probatio
description: Benchmark the skill classifier against blind operator judgment — harvest past loops from session transcripts, collect a blind multiselect of which skills should have applied, compare it with what actually matched, pin disagreements as regression cases, and tune classifiers.json questions until the match rate improves. Use when the user asks to benchmark or evaluate the skill classifier, asks why a skill did or did not fire, wants to tune classifiers.json questions, or runs /probatio.
---

Probatio (Latin: a test, trial, proof) — blind-review the classifier's own
records until its questions earn their keep.

The classifier composes skills from each skill's `classifiers.json` questions.
This skill measures that judgment against a human who did not know the answer,
and turns every disagreement into a pinned regression case.

Use `$ARGUMENTS` as an optional scope: a skill name (benchmark only that skill),
a session id, or a case id to replay.

## 1. Locate the harness

`<repo>/evals/classifier/bench.py` (stdlib only) ships in the drip checkout next
to `Cargo.toml`. If the cwd is not the drip repo, ask for the checkout path once
and use the absolute path from then on.

## 2. Harvest the classified loops

```
python3 evals/classifier/bench.py harvest
```

This walks `~/.drip/projects/*/sessions/*/transcript.jsonl`, pairs every
`loop-start` event (which records `data.skills` — what the classifier actually
matched) with the goal text in force at that moment, and writes `harvest.json`
plus `pool.json`. Add `--transcripts '<glob>'` for legacy `<project>/.drip/sessions`
roots, `--limit N` to keep the N most recent loops, or `--since ISO`.

Read the histogram it prints. Zero contexts means the classifier never ran in a
recorded session — say so and stop rather than inventing cases.

## 3. Build a blind survey

```
python3 evals/classifier/bench.py survey --n 12 --seed 0
```

`survey.json` holds the loop context and the candidate skill list, with the
classifier's picks withheld. `key.json` holds the answer. **Never show, quote, or
paraphrase `key.json` to the operator before they answer**, and never let a
context's skill list leak through another channel — the blind review is the only
reason the harness is trustworthy. Keep the survey to about six contexts per
round; long surveys get answered carelessly.

## 4. Ask for the blind multiselect

For each survey entry, present the context (goal, task, role) and the option list,
then ask with `ask_user` and `multiple: true`, one question per context, options
being the skill names in the same order as `options`. Say explicitly that the
answer key is hidden and that "none of these" is a valid answer.

Record every answer verbatim, including empty picks:

```json
{"answers": [{"caseId": "clf-...", "picks": ["tdd", "verify-before-done"]}]}
```

Save it as `evals/classifier/answers.json`. If the operator declines to judge a
context, omit that case rather than guessing for them.

## 5. Compare and pin

```
python3 evals/classifier/bench.py compare     # match rate + missed/spurious per case
python3 evals/classifier/bench.py pin         # disagreements -> cases.json
```

`missed` = the operator wanted a skill the classifier did not compose (false
negative; usually too-tight questions). `spurious` = the classifier composed a
skill the operator did not want (false positive; too-loose questions). Report
both counts and the match rate plainly before doing anything else.

## 6. Turn disagreements into regression cases

```
python3 evals/classifier/bench.py replay --cases evals/classifier/cases.json --write
```

`replay` re-runs the real classifier: for each pinned case it runs `drip --json`
in a throwaway workspace with the case's context, reads the `loop-start` skills
back out, and marks the case `pass` or `fail`. It exits non-zero when any case
fails, so it is the regression gate. Use `--id <case>` for a single case while
tuning (each replay costs one real run); `--dry-run` prints the command first.

A case is only a regression test once it reports `pass`. `open`/`fail` cases are
the tuning queue, and the pinned case must never be weakened to make it pass.

## 7. Improve the questions, not the code

For each failing case, read the skill's `classifiers.json` (`relevance.questions`,
`relevance.formula`, `relevance.threshold`) and change the smallest thing that
explains the miss:

- **false negative** — the deciding noul/choice question is too narrow: add the
  signal the operator saw (the concrete artifact, the verb, the target file type)
  or raise its weight in the formula. Re-check that the phrasing still excludes
  unrelated work.
- **false positive** — the question fires on a neighbouring context: add an
  explicit exclusion clause, demote its weight, or raise `threshold`.
- Keep questions answerable from the loop state alone; a question that needs
  knowledge the classifier never receives only moves the failure.

Then re-grade, one case at a time:

```
python3 evals/classifier/bench.py replay --cases evals/classifier/cases.json --id clf-xxxx --write
```

## 8. Report the match rate

```
python3 evals/classifier/bench.py report --json
```

Give the operator the match rate before and after, the per-skill
precision/recall table, how many cases now pass, and the exact `classifiers.json`
diff that moved it. Never claim an improvement from a formula you did not re-run,
and never edit the classifier's Rust code to satisfy one case.

## Hand-written cases as well as harvested ones

Harvesting is not the only source of cases. drip ships an **eval-case library**:
`drip --evals` lists cases (each a directory of `case.json` + `scenario.json`,
kind `prompt`, `task` or `plan`, project scope shadowing user scope, starter
cases under `<home>/evals`), and `drip --run-evals [name]` runs one through the
real classifier in a throwaway workspace and records what matched in that case's
own `verdict.json`. Use it when you want a fixed scenario instead of whatever the
sessions happen to hold:

```sh
drip --evals                        # the case library: name, kind, scope
drip --run-evals flaky-test-fixup   # one case, real classifier, recorded verdict
drip --run-evals                    # the whole pack: PASS/FAIL per case, pooled summary
drip --run-evals --runs 3           # repeat every case; unstable cases are named
```

The run prints every candidate's score (matched and below threshold) and, for
each expected or matched candidate, the answer to every question in its
`classifiers.json` — read those before touching a formula, and re-run the whole
pack after every edit so a fix for one case is never paid for with another.

Its `expected` list in `case.json` plays the part of a pinned case's `target`, and
`missed` / `spurious` between `expected` and what was matched are exactly the two
counts `compare` reports — so a scenario you keep meaning to fix can be pinned as
a case and re-run with one command, no blind survey needed. The blind survey is
still the honest instrument for judging a *new* question set, because it is the
only one where the human does not know the classifier's answer.

Done means: the survey had blind contexts, every disagreement is pinned in
`cases.json`, the cases the questions were tuned against pass `replay`, and the
match rate is stated as a number with its case count.
