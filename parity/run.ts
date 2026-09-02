#!/usr/bin/env bun
/**
 * Differential parity runner (drip/PLAN.md, "Parity strategy").
 *
 * lci is the oracle. Every scenario under drip/parity/scenarios/<name>/ runs
 * twice — once with lci (`bun run src/cli/main.tsx`), once with the drip
 * binary — each in a fresh temp root {home/, project/} against its own mock
 * model server. The captured artifacts (stdout, stderr, exit code, session
 * files, sqlite index rows, mock request log, git status/diff) are
 * normalized (normalize.ts) and diffed. Any difference is a FAIL.
 *
 *   bun run drip/parity/run.ts [--only <name>] [--self-check] [--keep] [--drip-bin <path>]
 *
 * --self-check runs lci on both sides: it must PASS for every scenario,
 * proving the normalizer hides all machine-local nondeterminism.
 */
import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { Database } from "bun:sqlite";
import {
  chmodSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { normalizeArtifact, normalizeText, sortMockRequests, type NormalizeOptions } from "./normalize";

const PARITY_DIR = dirname(new URL(import.meta.url).pathname);
const REPO_ROOT = resolve(PARITY_DIR, "..", "..");
const SCENARIOS_DIR = join(PARITY_DIR, "scenarios");
const MOCK_MODEL = join(PARITY_DIR, "mock-model.ts");
const DEFAULT_DRIP_BIN = join(REPO_ROOT, "drip", "target", "debug", "drip");
const SCENARIO_TIMEOUT_MS = 120_000;

type Side = {
  name: "lci" | "drip";
  command: string[];
  homeEnv: string;
};

type Scenario = {
  name: string;
  dir: string;
  /** One CLI invocation per step (args.json = one step; steps.json = several, run in order in the same project/home). */
  steps: string[][];
  responsesPath: string;
  fixtureDir: string | null;
  env: Record<string, string>;
};

type RunOptions = { only: string | null; selfCheck: boolean; keep: boolean; dripBin: string };

type Artifacts = Map<string, string>;

function usageError(message: string): never {
  process.stderr.write(
    `parity: ${message}\nusage: bun run drip/parity/run.ts [--only <name>] [--self-check] [--keep] [--drip-bin <path>]\n`
  );
  process.exit(1);
}

function parseArgs(argv: string[]): RunOptions {
  const options: RunOptions = { only: null, selfCheck: false, keep: false, dripBin: DEFAULT_DRIP_BIN };
  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index]!;
    if (arg === "--only") {
      const value = argv[++index];
      if (!value) usageError("--only requires a scenario name");
      options.only = value;
    } else if (arg === "--self-check") {
      options.selfCheck = true;
    } else if (arg === "--keep") {
      options.keep = true;
    } else if (arg === "--drip-bin") {
      const value = argv[++index];
      if (!value) usageError("--drip-bin requires a path");
      options.dripBin = resolve(value);
    } else {
      usageError(`unknown argument ${arg}`);
    }
  }
  return options;
}

function discoverScenarios(only: string | null): Scenario[] {
  if (!existsSync(SCENARIOS_DIR)) return [];
  const names = readdirSync(SCENARIOS_DIR)
    .filter((name) => statSync(join(SCENARIOS_DIR, name)).isDirectory())
    .sort();
  const scenarios: Scenario[] = [];
  for (const name of names) {
    if (only && name !== only) continue;
    const dir = join(SCENARIOS_DIR, name);
    const argsPath = join(dir, "args.json");
    const stepsPath = join(dir, "steps.json");
    if (!existsSync(argsPath) && !existsSync(stepsPath)) continue;
    const steps = existsSync(stepsPath)
      ? (JSON.parse(readFileSync(stepsPath, "utf8")) as string[][])
      : [JSON.parse(readFileSync(argsPath, "utf8")) as string[]];
    const responsesPath = join(dir, "responses.jsonl");
    if (!existsSync(responsesPath)) writeFileSync(responsesPath, "");
    const fixtureDir = existsSync(join(dir, "fixture")) ? join(dir, "fixture") : null;
    const envPath = join(dir, "env.json");
    const env = existsSync(envPath) ? (JSON.parse(readFileSync(envPath, "utf8")) as Record<string, string>) : {};
    scenarios.push({ name, dir, steps, responsesPath, fixtureDir, env });
  }
  return scenarios;
}

// ---------------------------------------------------------------------------
// Temp roots
// ---------------------------------------------------------------------------

function writeMockConfig(home: string, port: number): void {
  const profiles = [
    {
      id: "mock",
      model: "mock-model",
      provider: "openai-compatible",
      baseUrl: `http://127.0.0.1:${port}/v1`,
      label: "Parity mock",
      maxContextTokens: "64000"
    }
  ];
  const config = {
    settings: {
      "runtime.active_profile_id": "mock",
      "runtime.active_tool_profile_id": "mock",
      "runtime.model_profiles": JSON.stringify(profiles, null, 2)
    },
    version: 1
  };
  writeFileSync(join(home, "config.json"), `${JSON.stringify(config, null, 2)}\n`);
  const envVars = join(home, "env.vars");
  writeFileSync(envVars, "");
  chmodSync(envVars, 0o600);
}

function git(cwd: string, args: string[]): string {
  const result = spawnSync("git", args, {
    cwd,
    encoding: "utf8",
    env: {
      ...process.env,
      GIT_AUTHOR_NAME: "parity",
      GIT_AUTHOR_EMAIL: "parity@example.invalid",
      GIT_COMMITTER_NAME: "parity",
      GIT_COMMITTER_EMAIL: "parity@example.invalid",
      // Fixed dates so both sides' fixture commits hash identically (the
      // --review synthesis prompt embeds `git log --oneline`).
      GIT_AUTHOR_DATE: "2026-01-01T00:00:00Z",
      GIT_COMMITTER_DATE: "2026-01-01T00:00:00Z",
      GIT_CONFIG_GLOBAL: "/dev/null"
    }
  });
  if (result.status !== 0) {
    throw new Error(`git ${args.join(" ")} failed in ${cwd}: ${result.stderr}`);
  }
  return result.stdout;
}

function prepareProject(project: string, scenario: Scenario): void {
  mkdirSync(project, { recursive: true });
  git(project, ["init", "-q", "-b", "main"]);
  // A scenario that reviews a diff (--review --base base) ships `fixture-base/`:
  // committed first and tagged `base`, so `fixture/` lands as the change on top.
  const baseDir = join(scenario.dir, "fixture-base");
  if (existsSync(baseDir)) {
    cpSync(baseDir, project, { recursive: true });
    git(project, ["add", "-A"]);
    git(project, ["commit", "-q", "-m", "base"]);
    git(project, ["tag", "base"]);
  }
  if (scenario.fixtureDir) cpSync(scenario.fixtureDir, project, { recursive: true });
  // An empty fixture still needs one committed file (git refuses an empty commit).
  if (readdirSync(project).filter((name) => name !== ".git").length === 0) {
    writeFileSync(join(project, "README.md"), `# parity fixture: ${scenario.name}\n`);
  }
  git(project, ["add", "-A"]);
  git(project, ["commit", "-q", "-m", "fixture"]);
}

// ---------------------------------------------------------------------------
// Mock model server
// ---------------------------------------------------------------------------

type MockServer = { port: number; child: ChildProcess; logPath: string };

function startMock(root: string, responsesPath: string): Promise<MockServer> {
  const logPath = join(root, "requests.jsonl");
  const child = spawn("bun", ["run", MOCK_MODEL, "--responses", responsesPath, "--log", logPath, "--port", "0"], {
    stdio: ["ignore", "pipe", "pipe"]
  });
  return new Promise((resolvePort, reject) => {
    let buffer = "";
    let settled = false;
    const fail = (error: Error) => {
      if (settled) return;
      settled = true;
      reject(error);
    };
    child.stdout!.on("data", (chunk: Buffer) => {
      buffer += chunk.toString();
      const newline = buffer.indexOf("\n");
      if (newline === -1 || settled) return;
      const line = buffer.slice(0, newline);
      try {
        const parsed = JSON.parse(line) as { port?: number };
        if (typeof parsed.port !== "number") throw new Error(`mock did not print a port: ${line}`);
        settled = true;
        resolvePort({ port: parsed.port, child, logPath });
      } catch (error) {
        fail(error as Error);
      }
    });
    child.stderr!.on("data", (chunk: Buffer) => process.stderr.write(`[mock] ${chunk.toString()}`));
    child.on("exit", (code) => fail(new Error(`mock server exited early (code ${code})`)));
    setTimeout(() => fail(new Error("mock server did not start within 10s")), 10_000);
  });
}

// ---------------------------------------------------------------------------
// Running one side
// ---------------------------------------------------------------------------

type Capture = { stdout: string; stderr: string; exit: string };

function runCli(side: Side, scenario: Scenario, args: string[], home: string, project: string): Promise<Capture> {
  const env: Record<string, string> = { ...(process.env as Record<string, string>) };
  delete env.LCI_HOME;
  delete env.LCI_PROJECT_DIR;
  delete env.DRIP_HOME;
  delete env.DRIP_PROJECT_DIR;
  env.HOME = home;
  env[side.homeEnv] = home;
  env.NO_COLOR = "1";
  env.TERM = "dumb";
  Object.assign(env, scenario.env);
  const [command, ...prefix] = side.command;
  return new Promise((resolveCapture) => {
    const child = spawn(command!, [...prefix, ...args], { cwd: project, env, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    child.stdout!.on("data", (chunk: Buffer) => (stdout += chunk.toString()));
    child.stderr!.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
    child.on("error", (error) => {
      clearTimeout(timer);
      resolveCapture({ stdout, stderr: `${stderr}${error.message}\n`, exit: "spawn-error" });
    });
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, SCENARIO_TIMEOUT_MS);
    child.on("close", (code, signal) => {
      clearTimeout(timer);
      const exit = timedOut ? "timeout" : code === null ? `signal ${signal}` : String(code);
      resolveCapture({ stdout, stderr, exit });
    });
  });
}

// ---------------------------------------------------------------------------
// Artifact collection
// ---------------------------------------------------------------------------

function listDirs(path: string): string[] {
  if (!existsSync(path)) return [];
  return readdirSync(path)
    .filter((name) => statSync(join(path, name)).isDirectory())
    .sort();
}

function readIfExists(path: string): string | null {
  return existsSync(path) ? readFileSync(path, "utf8") : null;
}

function sqliteRows(dbPath: string, table: string): string[] {
  const db = new Database(dbPath, { readonly: true });
  try {
    // A detached run may still be closing its connection when we read.
    db.run("PRAGMA busy_timeout = 5000");
    const exists = db.query("SELECT name FROM sqlite_master WHERE type='table' AND name=?").get(table);
    if (!exists) return [];
    return (db.query(`SELECT * FROM ${table}`).all() as Array<Record<string, unknown>>).map((row) =>
      JSON.stringify(row)
    );
  } finally {
    db.close();
  }
}

function collectArtifacts(capture: Capture, home: string, project: string, mock: MockServer): Artifacts {
  const artifacts: Artifacts = new Map();
  const projectsDir = join(home, "projects");
  const slugs = listDirs(projectsDir);
  const options: NormalizeOptions = { home, project, slug: slugs[0] };

  // Session directories, oldest first (ids are masked, so order by creation).
  const sessionDirs: string[] = [];
  for (const slug of slugs) {
    for (const id of listDirs(join(projectsDir, slug, "sessions"))) {
      sessionDirs.push(join(projectsDir, slug, "sessions", id));
    }
  }
  for (const base of [".lci", ".drip"]) {
    for (const legacy of listDirs(join(project, base, "sessions"))) {
      sessionDirs.push(join(project, base, "sessions", legacy));
    }
  }
  sessionDirs.sort((a, b) => {
    const at = statSync(a).birthtimeMs;
    const bt = statSync(b).birthtimeMs;
    return at === bt ? basename(a).localeCompare(basename(b)) : at - bt;
  });
  const first = sessionDirs[0];
  if (first) {
    const sessionJson = readIfExists(join(first, "session.json"));
    if (sessionJson) {
      try {
        const parsed = JSON.parse(sessionJson) as { id?: string; sessionId?: string };
        options.lciSessionId = parsed.id ?? parsed.sessionId;
      } catch {
        // an unparseable session.json is itself a parity artifact below
      }
    }
  }

  artifacts.set("stdout", normalizeText(capture.stdout, options));
  artifacts.set("stderr", normalizeText(capture.stderr, options));
  artifacts.set("exit", capture.exit);
  artifacts.set("sessions/count", String(sessionDirs.length));

  sessionDirs.forEach((dir, index) => {
    for (const file of ["session.json", "state.json", "result.json"]) {
      const text = readIfExists(join(dir, file));
      if (text !== null) artifacts.set(`session[${index}]/${file}`, normalizeArtifact(text, "session-json", options));
    }
    const transcript = readIfExists(join(dir, "transcript.jsonl"));
    if (transcript !== null) {
      artifacts.set(`session[${index}]/transcript.jsonl`, normalizeArtifact(transcript, "transcript", options));
    }
    const extras = readdirSync(dir)
      .filter((name) => !["session.json", "state.json", "result.json", "transcript.jsonl", "run.log"].includes(name))
      .sort();
    artifacts.set(`session[${index}]/files`, extras.join("\n"));
  });

  for (const slug of slugs) {
    const dbPath = join(projectsDir, slug, "index.sqlite");
    if (!existsSync(dbPath)) continue;
    for (const table of ["sessions", "session_memories"]) {
      const rows = sqliteRows(dbPath, table)
        .map((row) => normalizeArtifact(row, "sqlite-row", options))
        .sort();
      artifacts.set(`index/${table}`, rows.join("\n"));
    }
  }

  const requestLog = readIfExists(mock.logPath) ?? "";
  const requests = requestLog.split("\n").filter((line) => line.trim().length > 0);
  artifacts.set("mock/requests", sortMockRequests(requests, options).join("\n"));

  artifacts.set("git/status", normalizeText(git(project, ["status", "--porcelain"]), options));
  artifacts.set("git/diff", normalizeText(git(project, ["diff"]), options));
  return artifacts;
}

// ---------------------------------------------------------------------------
// Diffing
// ---------------------------------------------------------------------------

function unifiedDiff(label: string, left: string, right: string, scratch: string): string {
  const leftPath = join(scratch, "lci");
  const rightPath = join(scratch, "drip");
  writeFileSync(leftPath, left.endsWith("\n") ? left : `${left}\n`);
  writeFileSync(rightPath, right.endsWith("\n") ? right : `${right}\n`);
  const result = spawnSync("diff", ["-u", "--label", `lci:${label}`, "--label", `drip:${label}`, leftPath, rightPath], {
    encoding: "utf8"
  });
  return result.stdout;
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function runSide(side: Side, scenario: Scenario, keep: boolean): Promise<{ artifacts: Artifacts; root: string }> {
  const root = mkdtempSync(join(tmpdir(), `parity-${scenario.name}-${side.name}-`));
  const home = join(root, "home");
  const project = join(root, "project");
  mkdirSync(home, { recursive: true });
  let mock: MockServer | null = null;
  try {
    prepareProject(project, scenario);
    mock = await startMock(root, scenario.responsesPath);
    writeMockConfig(home, mock.port);
    // Steps run back to back; their captures are joined with a marker so a
    // multi-invocation scenario (run, then --state; --detach, then --wait)
    // diffs as one artifact per stream.
    const captures: Capture[] = [];
    for (const args of scenario.steps) {
      // ["@sleep", "<ms>"]: a pause between steps (a --detach child needs a
      // moment to take its lease before --wait can attach to it).
      if (args[0] === "@sleep") {
        await new Promise((resolve) => setTimeout(resolve, Number(args[1] ?? "0")));
        captures.push({ stdout: "", stderr: "", exit: "sleep" });
        continue;
      }
      captures.push(await runCli(side, scenario, args, home, project));
    }
    const joined = (pick: (c: Capture) => string) =>
      captures.length === 1 ? pick(captures[0]!) : captures.map((c, i) => `=== step ${i + 1} ===\n${pick(c)}`).join("");
    const capture: Capture = { stdout: joined((c) => c.stdout), stderr: joined((c) => c.stderr), exit: captures.map((c) => c.exit).join(",") };
    const artifacts = collectArtifacts(capture, home, project, mock);
    if (keep) {
      writeFileSync(join(root, "stdout.txt"), capture.stdout);
      writeFileSync(join(root, "stderr.txt"), capture.stderr);
    }
    return { artifacts, root };
  } catch (error) {
    if (!keep) rmSync(root, { recursive: true, force: true });
    throw error;
  } finally {
    mock?.child.kill("SIGTERM");
  }
}

async function main(): Promise<void> {
  const options = parseArgs(process.argv.slice(2));
  const lciSide: Side = {
    name: "lci",
    command: ["bun", "run", join(REPO_ROOT, "src", "cli", "main.tsx")],
    homeEnv: "LCI_HOME"
  };
  const dripSide: Side = options.selfCheck
    ? { ...lciSide, name: "drip" }
    : { name: "drip", command: [options.dripBin], homeEnv: "DRIP_HOME" };
  if (!options.selfCheck && !existsSync(options.dripBin)) {
    process.stderr.write(
      `parity: drip binary not found at ${options.dripBin} — build it with (cd drip && cargo build) or pass --drip-bin; use --self-check to validate the harness with lci on both sides\n`
    );
    process.exit(2);
  }
  const scenarios = discoverScenarios(options.only);
  if (scenarios.length === 0) usageError(options.only ? `no scenario named ${options.only}` : "no scenarios found");

  let failures = 0;
  for (const scenario of scenarios) {
    const started = Date.now();
    let left: { artifacts: Artifacts; root: string } | null = null;
    let right: { artifacts: Artifacts; root: string } | null = null;
    let error: string | null = null;
    const settled = await Promise.allSettled([
      runSide(lciSide, scenario, options.keep),
      runSide(dripSide, scenario, options.keep)
    ]);
    for (const outcome of settled) {
      if (outcome.status === "rejected") {
        error = outcome.reason instanceof Error ? outcome.reason.message : String(outcome.reason);
      }
    }
    if (settled[0].status === "fulfilled") left = settled[0].value;
    if (settled[1].status === "fulfilled") right = settled[1].value;
    if (error && !options.keep) {
      for (const side of [left, right]) if (side) rmSync(side.root, { recursive: true, force: true });
    }
    const elapsed = ((Date.now() - started) / 1000).toFixed(1);
    if (error || !left || !right) {
      failures++;
      process.stdout.write(`FAIL ${scenario.name} (${elapsed}s) — harness error: ${error}\n`);
      continue;
    }
    const keys = [...new Set([...left.artifacts.keys(), ...right.artifacts.keys()])].sort();
    const diffs: string[] = [];
    const scratch = mkdtempSync(join(tmpdir(), "parity-diff-"));
    for (const key of keys) {
      const a = left.artifacts.get(key) ?? "<missing>";
      const b = right.artifacts.get(key) ?? "<missing>";
      if (a !== b) diffs.push(unifiedDiff(key, a, b, scratch));
    }
    rmSync(scratch, { recursive: true, force: true });
    if (diffs.length === 0) {
      process.stdout.write(`PASS ${scenario.name} (${elapsed}s, ${keys.length} artifacts)\n`);
    } else {
      failures++;
      process.stdout.write(`FAIL ${scenario.name} (${elapsed}s, ${diffs.length} of ${keys.length} artifacts differ)\n`);
      process.stdout.write(diffs.join("\n"));
    }
    if (options.keep) {
      process.stdout.write(`  kept: ${left.root}\n  kept: ${right.root}\n`);
    } else {
      rmSync(left.root, { recursive: true, force: true });
      rmSync(right.root, { recursive: true, force: true });
    }
  }
  process.stdout.write(`${scenarios.length - failures} passed, ${failures} failed\n`);
  process.exit(failures === 0 ? 0 : 1);
}

await main();
