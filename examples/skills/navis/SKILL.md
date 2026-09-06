---
name: navis
description: Full ship workflow — implement, verify, review, triage findings, fix, and open a draft PR
roles:
  default: author
  planning: planner
  implementation: author
  review: reviewer
  triage: planner
  fixes: author
  shipping: author
---

Full ship loop for one goal, split into stages so each stage can run under the
role that suits it. The `roles:` block above is **advisory** — it hints how
roles should be used with this skill; it does not route anything by itself.

Run the stages in order, realizing each role change through the task list:

1. **Planning** (suggested role: planner) — break the goal into small concrete
   tasks with `plan_tasks`, including verification and an independent review.
2. **Implementation** (author) — implement every task fully; run the project's
   declared verification and keep its passing output visible.
3. **Review** (reviewer) — review the actual diff against the original goal
   with your own tools; re-run verification yourself. Save a written report.
4. **Triage** (planner) — schedule a separate planner task after the review
   produces findings. Classify every finding; queue small fix tasks for valid
   P0/P1 items; record one-line reasons for anything judged invalid.
5. **Fixes** (author) — fix, then re-verify and re-review each fix batch.
6. **Shipping** (author) — commit scoped changes, push, and open a draft PR.

Precedence: an explicit role the user assigned, or one a task already carries,
always wins over these hints. Use only roles that exist in the session's role
configuration; if a suggested role is unknown there, keep the configured
default behavior instead of inventing a role or granting extra tools. With
other skills active, take stage hints from the most relevant skill and surface
real conflicts explicitly.
