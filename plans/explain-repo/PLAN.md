---
name: explain-repo
description: Orient the operator in a codebase — read the manifest, README, and top-level tree, say what the project is, how it is built, run, and tested, and where the important code lives, without editing anything
---

# Plan: explain the repo

Use this plan when the goal asks what a repository is or does, what is in it,
or for an overview of the codebase. Adapt it: keep it short, and read only
what the answer needs.

1. **Read the front matter.** The README, the package manifest (Cargo.toml,
   package.json, pyproject), and the top-level directory listing. Note the
   declared build, run, and test commands.
2. **Map the important code.** Skim the entry point and the two or three
   largest or most-referenced modules; note what each is for in one line.
   Read a file only far enough to name its purpose.
3. **Answer.** In a short message: what the project is and who it is for, how
   to build, run, and test it, and a list of the key directories or modules
   with one line each. Mention anything surprising (a second binary, a
   vendored dependency, a missing README). Edit nothing.
