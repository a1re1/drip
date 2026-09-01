#!/usr/bin/env bun
// Dumps the OpenAI function-call definition for each harness (framework) tool,
// sorted by name, as a JSON array.  Run from the repo root:
//   bun drip/parity/tools/dump-harness-tools.ts

import { HARNESS_TOOL_SPECS } from "../../../src/harness/harness-tools";

const tools = [...HARNESS_TOOL_SPECS];

// Sort by name so the output is deterministic
tools.sort((a, b) => a.function.name.localeCompare(b.function.name));

const definitions = tools.map((t) => ({
  type: "function",
  function: {
    name: t.function.name,
    description: t.function.description,
    parameters: t.function.parameters,
  },
}));

console.log(JSON.stringify(definitions, null, 2));
