#!/usr/bin/env bun
// Dumps the OpenAI function-call definition for each built-in read-only tool,
// sorted by name, as a JSON array.  Run from the repo root:
//   bun drip/parity/tools/dump-tools.ts

import { dirTool } from "../../../tools/dir-tool";
import { grepTool } from "../../../tools/grep-tool";
import { readTool } from "../../../tools/read-tool";

const tools = [dirTool, grepTool, readTool];

// Sort by name so the output is deterministic
tools.sort((a, b) => a.name.localeCompare(b.name));

const definitions = tools.map((t) => ({
  type: "function",
  function: {
    name: t.name,
    description: t.description,
    parameters: t.parameters,
  },
}));

console.log(JSON.stringify(definitions, null, 2));
