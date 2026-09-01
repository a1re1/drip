#!/usr/bin/env bun
// Dumps lci's shipped default settings values (the JSON strings stored under
// runtime.model_profiles / runtime.system_prompt_profiles / credentials.stored_api_keys)
// so drip can embed byte-identical defaults via include_str!.
//   bun run drip/parity/tools/dump-defaults.ts <out-dir>
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import {
  getDefaultWebSettingValues,
  MODEL_PROFILES_SETTING_ID,
  STORED_API_KEYS_SETTING_ID,
  SYSTEM_PROMPT_PROFILES_SETTING_ID
} from "../../../src/web/settings";

const outDir = process.argv[2];
if (!outDir) {
  process.stderr.write("usage: dump-defaults.ts <out-dir>\n");
  process.exit(1);
}
mkdirSync(outDir, { recursive: true });
const defaults = getDefaultWebSettingValues();
writeFileSync(join(outDir, "model_profiles.json"), defaults[MODEL_PROFILES_SETTING_ID]!);
writeFileSync(join(outDir, "system_prompt_profiles.json"), defaults[SYSTEM_PROMPT_PROFILES_SETTING_ID]!);
writeFileSync(join(outDir, "stored_api_keys.json"), defaults[STORED_API_KEYS_SETTING_ID]!);
const other: Record<string, string> = {};
for (const [key, value] of Object.entries(defaults)) {
  if (![MODEL_PROFILES_SETTING_ID, SYSTEM_PROMPT_PROFILES_SETTING_ID, STORED_API_KEYS_SETTING_ID].includes(key)) other[key] = value;
}
writeFileSync(join(outDir, "other_settings.json"), `${JSON.stringify(other, null, 2)}\n`);
process.stdout.write(`wrote defaults to ${outDir}\n`);
