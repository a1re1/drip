---
name: praeparare
description: Run an agentic pre-PR pass on the current branch — clean up, commit, merge the base, push, and open a DRAFT PR
---

Praeparare (Latin "to prepare") prepares the current branch for a pull request.
Follow the steps below in order. Never rewrite history: no squash, rebase,
amend, or force-push at any point. Everything in this pass is non-destructive
to the commit history.

1. Detect the base branch before touching anything. Run
   `git remote show origin` and check its exit status FIRST. If the command
   fails or the remote is unreachable, REFUSE (stop and explain to the
   operator) — never guess a base branch from a failed run. Only when the
   command succeeds, read the HEAD branch from its output with
   `git remote show origin | sed -n 's/.*HEAD branch: //p'`; if the command
   succeeded but printed no "HEAD branch" line, fall back to `main`. REFUSE
   (stop and explain to the operator) if the current branch IS the base
   branch — there is nothing to prepare for a PR from the base branch itself.
2. Refuse before making any change if `gh auth status` fails. Opening a pull
   request needs authenticated `gh`; report the auth problem and stop instead
   of pushing a branch the operator cannot PR from.
3. Detect the project's formatter, linter, and test commands from the repo
   itself, not from a hard-coded list. Examples of the detection heuristic:
   `Cargo.toml` -> `cargo fmt --all -- --check`, `cargo clippy --all-targets`,
   `cargo test`; a `package.json` -> its `lint`/`test`/`fmt`-style scripts; a
   `Makefile` or `justfile` -> its check/test targets. Run what you find and
   fix any failures. Rerun the relevant checks after any later step (cleanup,
   merge) changes code they examine.
4. Remove stray changes: debug prints and logging you added while exploring,
   TODO-scratch notes, and files outside the goal of the work. Honour
   `.gitignore` — never stage ignored files explicitly. Do not delete work
   that looks like the user's on purpose; when in doubt about a change, leave
   it and say so in the final report.
5. Stage and commit ALL remaining changes with a descriptive message. Never
   squash, rebase, amend, or force-push — history is non-destructive.
6. Integrate the base branch: `git fetch origin && git merge origin/<base>`.
   On non-trivial conflicts STOP and report them — resolving a conflicted
   merge that needs human judgment is out of scope for an automated pass.
7. Publish: `git push -u origin HEAD`.
8. Check for an existing PR with `gh pr view`. Distinguish "no PR for this
   branch" from other `gh` failures — only the former leads to creation. If a
   PR already exists, push only and do not create another. If none exists,
   run `gh pr create --draft` with a title naming the change and a body with
   three markdown sections: **Goal**, **Changes**, **Testing**. Never mark a
   draft ready-for-review.
9. Final message: report the branch, the PR URL, what was verified (the exact
   checks that ran and passed), and what was NOT verified.

## Refuse when

- The current branch is the base branch.
- `git remote show origin` fails or the remote is unreachable.
- `gh auth status` fails.
- A merge conflict needs human judgment.
