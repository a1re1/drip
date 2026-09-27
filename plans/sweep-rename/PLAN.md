---
name: sweep-rename
description: A mechanical sweep across many files — a rename, a scrub of every mention, a rewording of comments or test names — done by enumerating occurrences first, editing file by file with no behavior change, re-grepping to zero, and proving the build and tests are unchanged
---

# Plan: sweep or rename across files

Use this plan when the goal asks for the same mechanical change in many
places: rename X to Y everywhere, remove every mention of Z from comments,
rewrite the wording in these files. Adapt it: keep the order, and replace
step 3 with the file groups this sweep actually covers.

1. **Enumerate.** Grep for every pattern the goal names (and its obvious
   variants — casing, plural, abbreviation) within the scope the goal sets,
   and record the count per file. This list is the definition of done.
2. **Decide the rule once.** Write the replacement rule in one line per
   pattern (what becomes what, what is left alone) so every file gets the
   same treatment. A sweep that changes behavior is a refactor, not a sweep —
   if a hit is load-bearing (a serialized name, a public identifier), note it
   and leave it unless the goal covers it.
3. **Edit file by file.** Replace this step with one task per file or
   directory group, each applying the rule and re-grepping that group to
   zero. Keep edits to the hits; do not reformat or improve neighbouring
   lines.
4. **Re-grep the whole scope.** Show that every pattern from step 1 has zero
   hits in scope (and that out-of-scope files are untouched).
5. **Prove no behavior change.** Run the project's build and tests and compare
   with a baseline taken before the sweep. Commit or PR only if the goal
   asked for it.
