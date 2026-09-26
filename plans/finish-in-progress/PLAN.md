---
name: finish-in-progress
description: Complete work that is already partly done in the tree — inventory what exists and what is missing before touching anything, never reimplement what is there, close the remaining gaps, get the build and checks green, and hand it back in the state the goal asked for
---

# Plan: finish in-progress work

Use this plan when the goal says work is already underway — uncommitted
changes in the working tree, a partial patch, a branch someone else started,
a previous session that stopped — and asks to finish it. Adapt it: keep the
order, and replace step 3 with the gaps this work actually has.

1. **Inventory the state.** Read `git status`, `git diff --stat`, and the
   goal's list of what exists. Build a two-column list: done (with the file
   that proves it) and missing. Read the existing partial code before
   deciding anything is missing — reimplementing finished work is the failure
   mode this plan exists to prevent.
2. **Get it compiling.** If the tree does not build, fix compile errors first
   with the smallest edits, preserving the previous author's intent.
3. **Close the gaps.** Replace this step with one task per missing item from
   step 1, each naming the file and the check that proves it. Follow the
   patterns the existing partial code established.
4. **Verify.** Run the project's declared checks and any command the goal names
   as the acceptance check. Distinguish failures that were already there from
   ones this work introduced.
5. **Hand back as asked.** Commit only if the goal says to (and with the
   message it gives); never rebase, amend, or stash unless told. Report what
   was already done, what this pass added, and what remains open.
