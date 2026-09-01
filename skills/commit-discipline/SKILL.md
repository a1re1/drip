---
name: commit-discipline
description: Safe git hygiene when a goal involves committing or pushing work
---

When the goal asks for commits, branches, or pushes:

1. Run git status before staging: know what is dirty and why. Never use
   git add -A blindly — stage the files your work changed, by name, and
   leave unrelated dirt alone.
2. One logical change per commit, with a message that says what and why in
   plain sentences (no "WIP", no "fixes").
3. After committing (and especially after pushing), run git status again:
   a dirty tree after a push means local and published code have diverged —
   reconcile or report it in the summary, never leave it silent.
4. Never force-push, amend published history, or delete branches unless the
   goal says so explicitly.
5. Do not commit generated artifacts, secrets, .env files, or the .drip
   session directory. Check the diff before the commit, not after.
