#!/usr/bin/env bun
// Dumps the OpenAI function-call definition for each built-in tool,
// in declaration order, as a JSON array.  Run from the repo root:
//   bun drip/parity/tools/dump-tools.ts

import { asyncBashTool, bashTool } from "../../../tools/bash-tool";
import { checkTool } from "../../../tools/check-tool";
import { dirTool } from "../../../tools/dir-tool";
import { fetchTool } from "../../../tools/fetch-tool";
import { grepTool } from "../../../tools/grep-tool";
import { patchTool } from "../../../tools/patch-tool";
import { readTool } from "../../../tools/read-tool";
import { verifyTool } from "../../../tools/verify-tool";

const tools = [readTool, patchTool, dirTool, bashTool, asyncBashTool, grepTool, verifyTool, fetchTool, checkTool];

// Declaration order is part of the contract: the transport sends tools in
// this order and the model sees it. Do NOT sort.


const definitions = tools.map((t) => ({
  type: "function",
  function: {
    name: t.name,
    description: t.description,
    parameters: t.parameters,
  },
}));

console.log(JSON.stringify(definitions, null, 2));
