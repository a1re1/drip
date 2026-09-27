---
name: scaffold-project
description: Start a new project from nothing — choose the layout and toolchain, scaffold a building skeleton, ship one thin vertical slice that runs end to end, add the first tests, and write the README that lets the operator build, run, and test it
---

# Plan: scaffold a new project

Use this plan when the goal asks to bootstrap, spin up, or build a new tool,
app, service, or game in an empty or nearly empty repository. Adapt it: keep
the order, and replace step 3 with the slice this project actually needs
first.

1. **Fix the shape.** Name the language, build tool, and top-level layout
   (bin vs lib, src/tests/docs, config location), matching what the goal
   names and the operator's other projects otherwise. Write a one-paragraph
   README stating what the project is and the single command to run it.
2. **Skeleton that builds.** Create the manifest, entry point, and directory
   layout, and make the build and an empty test suite pass before adding any
   behavior.
3. **One vertical slice.** Replace this step with the tasks for the smallest
   end-to-end path the goal describes (the hello-world map, the one proxied
   request, the one CLI command), each naming its files and the check that
   proves it. Depth before breadth: one path that works beats five that are
   stubbed.
4. **First tests.** Add tests for the slice's pure logic in the project's
   test framework so later work has a harness to extend.
5. **Run it once.** Build and run the project the way the README says and
   record what the operator will see. Fix the README if it lied.
6. **Leave a map.** Note in the README (or a TODO section) what the next
   slices are, so the following session does not rediscover them. Commit or
   PR only if the goal asked for it.
