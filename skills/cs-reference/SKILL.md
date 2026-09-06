---
name: cs-reference
description: Answer computer-science questions from the configured REFERENCE corpus instead of memory, and cite the page you used
---

When a task turns on computer-science knowledge — an algorithm's complexity, a data
structure's trade-offs, a consensus protocol, a scheduling or caching policy, a
statistical or ML method, a security primitive, a retrieval or database technique — do
not answer from memory. Search the corpus with REFERENCE first.

The loop:

1. **Search.** `REFERENCE {action: "search", query: "<the question in the practitioner's
   words>", k: 5}`. Headings are weighted, so naming the thing ("LSM tree write
   amplification") beats describing it.
2. **Scan the hits.** Each hit's `path` is its citation. Concept pages explain how a
   thing works, source pages say which textbook, course or paper it comes from,
   synthesis pages compare two approaches, path pages lay out study order.
3. **Read what you will rely on.** `REFERENCE {action: "show", path: "<hit path>"}` for
   the whole page, or `{action: "show", chunk: <chunk id>}` for just the passage. Skip
   this when the snippets already answer the question — they usually do.
4. **Answer in your own words and cite the page path** you used, e.g.
   "(see `concepts/bm25.md`)". Never invent a path: cite only paths the search returned.
5. **If the first query misses, rephrase once.** Swap a question form for keywords (or
   the reverse), or use `mode: "lexical"` when you want an exact-term match and
   `"hybrid"` (the default) when you are paraphrasing.

Guardrails:

- REFERENCE is background knowledge, never a substitute for reading this repository.
  How *this* codebase works comes from READ, GREP and DIR — the corpus does not know it.
- A corpus miss is information: say the corpus does not cover it rather than filling the
  gap with a confident guess.
- Keep `k` small (5 is usually right) and prefer snippets over full pages; the corpus is
  large and context is not.
