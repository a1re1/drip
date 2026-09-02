#!/usr/bin/env bun
// Dumps the OpenAI function-call definition for each harness (framework) tool,
// in declaration order, as a JSON array.  Run from the repo root:
//   bun drip/parity/tools/dump-harness-tools.ts

import { HARNESS_TOOL_SPECS } from "../../../src/harness/harness-tools";

const tools = [...HARNESS_TOOL_SPECS];

// Declaration order is part of the contract: the transport sends tools in
// this order and the model sees it. Do NOT sort.


const definitions = tools.map((t) => ({
  type: "function",
  function: {
    name: t.function.name,
    description: t.function.description,
    parameters: t.function.parameters,
  },
}));

console.log(JSON.stringify(definitions, null, 2));
