---
name: investigate-report
description: A read-only investigation, audit, or research pass whose deliverable is a report or a plan — pin the question, gather evidence with reads, greps, commands, and transcripts, write findings with exact paths and numbers, and change no source files
---

# Plan: investigate and report

Use this plan when the goal asks to find out, research, audit, reconnoitre,
or plan something and explicitly or implicitly wants no code changed: the
answer is the deliverable. Adapt it: keep the order, and replace step 2 with
the evidence this question actually needs.

1. **Pin the question.** Restate what must be answered in one or two lines and
   name the form of the answer (a markdown file at a given path, a summary in
   the final message, a proposed task list). If the goal names an output
   path, that file is the only thing written.
2. **Gather evidence.** Replace this step with one task per evidence source —
   the relevant code ranges, greps for the mechanism, commands whose output
   answers a sub-question, transcripts or logs when the question is about
   behavior in the field. Record exact paths, line numbers, counts, and
   command output as you go; a claim without a location is not a finding.
3. **Write the report.** Lead with the answer, then the evidence, then the
   open questions; use a short table where numbers compare. When the goal
   asks for a plan, give ordered steps that each name files and a check, and
   flag the decisions only the operator can make.
4. **Edit nothing else.** No source edits, no commits, no branch changes. If
   fixing something looks trivial, describe the fix in the report instead of
   making it.
