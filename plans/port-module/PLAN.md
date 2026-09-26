---
name: port-module
description: Port one module from a reference implementation — read the source once and the pattern to imitate, write the port in compiling chunks that keep the reference behavior byte-for-byte where it is observable, port the tests, and finish the moment the module is green
---

# Plan: port a module

Use this plan when the goal names a source file in one language or codebase
and a destination file to write in another — a TypeScript module into a Rust
crate, a script into a library — often with a stub already in place and a
sibling file to imitate. Adapt it: keep the order, and replace step 3 with
the chunks this module divides into.

1. **Read the reference once.** Read the source module end to end, or the
   pasted source if the goal carries it, and list its public surface: every
   exported function, type, constant, and error string. Read the destination
   stub and the sibling file the goal says to imitate, and nothing else
   unless a specific symbol forces it.
2. **Contract before code.** Write the destination file's public signatures
   first (or keep the stubbed ones exactly as declared), so callers and tests
   compile against the contract while bodies land.
3. **Port in compiling chunks.** Replace this step with one task per chunk —
   types, helpers, the main path, the edge paths — each ending with the crate
   compiling. Keep observable behavior identical: error text, ordering,
   defaults, and serialized field names are part of the port, not style.
4. **Port the tests.** Translate the reference tests into the destination's
   test framework, keeping each test's name and assertion; list any test that
   cannot be ported (needs a runtime the destination lacks) by name instead
   of dropping it silently.
5. **Finish when green.** Run the module's tests and the crate build; finish
   as soon as they pass. Do not audit the rest of the tree, reformat other
   files, or improve the reference's design.
