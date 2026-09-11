// Tests for the thin HTTP layer in server.ts. Every route runs through
// createServer().fetch against a fake drip executable (DRIP_BIN) and
// temporary session files — no real session is touched and no real drip is
// spawned. The listing cache is disabled so each test sees its own FAKE_* env.
process.env.DRIP_UI_LIST_CACHE_MS = "0";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { cpSync, mkdirSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath as fileURLToPathCompat, pathToFileURL } from "node:url";
import { createServer } from "../server";

const ROOT = join(tmpdir(), `drip-ui-server-test-${process.pid}`);
const PROJ = join(ROOT, "proj"); // DRIP_CWD — the launch project directory
const HOME = join(ROOT, "home");
const SESSION_A = "aaaaaaaa-1111-2222-3333-444444444444";
const SESSION_B = "bbbbbbbb-1111-2222-3333-444444444444";

let fakeBin = "";

/** Fake drip: argv (base64, one per line) -> $FAKE_ARGV; behavior via env. */
function makeFakeDrip(): string {
  const path = join(ROOT, `fake-drip-${process.pid}.sh`);
  writeFileSync(
    path,
    `#!/bin/bash
for arg in "$@"; do printf '%s\\n' "$(printf '%s' "$arg" | base64)"; done >> "\${FAKE_ARGV:-/dev/null}"
[ -n "$FAKE_CWD_LOG" ] && pwd >> "$FAKE_CWD_LOG"
case "$1" in
  --list)
    if [ -n "$FAKE_LIST_FAIL" ]; then echo "boom" >&2; exit 1; fi
    if [ -n "$FAKE_LIST_BADJSON" ]; then echo 'not json'; exit 0; fi
    echo "$FAKE_LIST_JSON"
    ;;
  --json)
    if [ "$2" = "--send" ]; then
      if [ -n "$FAKE_SEND_ACTIVE" ]; then echo '{"type":"sent","runActive":true}'; else echo '{"type":"sent","runActive":false}'; fi
      exit 0
    fi
    if [ -n "$FAKE_RUN_FAIL" ]; then echo "run exploded" >&2; exit 3; fi
    if [ -n "$FAKE_RUN_NOJSON" ]; then echo "hello there"; exit 0; fi
    echo '{"sessionId":"new-session-777","transcriptPath":"/tmp/t.jsonl"}'
    ;;
  --send)
    echo "fake-drip: --send without --json" >&2
    exit 1
    ;;
  --stop)
    echo "stopping session"
    ;;
  --resume)
    echo '{"sessionId":"resumed-42"}'
    ;;
  *)
    echo "fake-drip: unexpected argv: $*" >&2
    exit 1
    ;;
esac
`,
    { mode: 0o755 },
  );
  return path;
}

function row(id: string, overrides: Record<string, unknown> = {}): Record<string, unknown> {
  const dir = join(HOME, "sessions", id.slice(0, 8));
  return {
    id,
    createdAt: "2026-09-10T10:00:00Z",
    updatedAt: "2026-09-10T12:00:00Z",
    cwd: PROJ,
    dir,
    goalCount: 1,
    lastGoal: "ship the thing",
    lastRun: null,
    pid: 4242,
    running: true,
    sessionsDir: dir,
    statePath: join(dir, "state.json"),
    status: "running",
    transcriptPath: join(dir, "transcript.jsonl"),
    ...overrides,
  };
}

const LIST_JSON = JSON.stringify([
  row(SESSION_A),
  row(SESSION_B, { running: false, pid: null, updatedAt: "2026-09-09T09:00:00Z", status: "done", cwd: join(PROJ, "sub") }),
]);

function withEnv<T>(overrides: Record<string, string | undefined>, fn: () => Promise<T>): Promise<T> {
  const saved: Record<string, string | undefined> = {};
  for (const [key, value] of Object.entries(overrides)) {
    saved[key] = process.env[key];
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
  return fn().finally(() => {
    for (const [key, value] of Object.entries(saved)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  });
}

/** Actions resolve their row via a fresh `--list --json --recursive` probe
 *  (lib/drip.ts findSession); strip those leading args from captures. */
async function actionArgv(path: string): Promise<string[]> {
  const argv = (await Bun.file(path).text())
    .trim()
    .split("\n")
    .filter((line) => line !== "")
    .map((line) => Buffer.from(line, "base64").toString("utf8"));
  if (argv[0] === "--list" && argv[1] === "--json" && argv[2] === "--recursive") {
    return argv.slice(3);
  }
  return argv;
}

let fetcher: (request: Request) => Promise<Response>;

async function api(path: string, init?: RequestInit): Promise<Response> {
  return fetcher(new Request(`http://127.0.0.1:4141${path}`, init));
}

beforeAll(() => {
  mkdirSync(join(PROJ, "sub"), { recursive: true });
  mkdirSync(join(HOME, "sessions", SESSION_A.slice(0, 8)), { recursive: true });
  mkdirSync(join(HOME, "sessions", SESSION_B.slice(0, 8)), { recursive: true });
  writeFileSync(join(HOME, "sessions", SESSION_A.slice(0, 8), "state.json"), JSON.stringify({ tasks: [] }));
  writeFileSync(
    join(HOME, "sessions", SESSION_A.slice(0, 8), "transcript.jsonl"),
    '{"type":"goal","text":"first"}\n{"type":"info","text":"second"}\n',
  );
  // result.json exists for A but not for B.
  writeFileSync(join(HOME, "sessions", SESSION_A.slice(0, 8), "result.json"), JSON.stringify({ ok: true }));
  fakeBin = makeFakeDrip();
  fetcher = createServer().fetch;
});

afterAll(() => {
  rmSync(ROOT, { recursive: true, force: true });
});

describe("static + bootstrap routes", () => {
  test("GET / returns the HTML entrypoint", async () => {
    await withEnv({ DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141", DRIP_MAX_CONTEXT_TOKENS: "48000" }, async () => {
      const response = await api("/");
      expect(response.status).toBe(200);
      expect(response.headers.get("content-type")).toContain("text/html");
      const body = await response.text();
      expect(body).toContain("<!doctype html>");
      expect(body).toContain("drip ui");
    });
  });

  test("GET / references its assets relatively so a path prefix works", async () => {
    await withEnv(uiEnv(), async () => {
      const body = await (await api("/")).text();
      expect(body).toContain('href="./main.css"');
      expect(body).toContain('src="./main.js"');
      expect(body).not.toMatch(/(src|href)="\//);
    });
  });

  test("GET /main.js and /main.css are the built bundle", async () => {
    await withEnv(uiEnv(), async () => {
      const js = await api("/main.js");
      expect(js.status).toBe(200);
      expect(js.headers.get("content-type")).toContain("javascript");
      expect(await js.text()).toContain("createRoot");
      const css = await api("/main.css");
      expect(css.status).toBe(200);
      expect(css.headers.get("content-type")).toContain("text/css");
      expect((await css.text()).length).toBeGreaterThan(100);
      expect((await api("/nope.js")).status).toBe(404);
    });
  });

  test("the bundle builds when the unpack directory has spaces and non-ASCII in its path", async () => {
    // ~/.drip/ui/<version> sits under the user's home, e.g. /Users/Ana María.
    const web = dirname(dirname(fileURLToPathCompat(import.meta.url)));
    const copy = join(ROOT, "unpack dir ü #1", "0.0.0");
    mkdirSync(copy, { recursive: true });
    for (const name of ["server.ts", "index.html", "lib", "src", "tsconfig.json"]) cpSync(join(web, name), join(copy, name), { recursive: true });
    symlinkSync(join(web, "node_modules"), join(copy, "node_modules"));
    const mod = (await import(pathToFileURL(join(copy, "server.ts")).href)) as { createServer: typeof createServer };
    await withEnv(uiEnv(), async () => {
      const js = await mod.createServer().fetch(new Request("http://127.0.0.1:4141/main.js"));
      expect(js.status).toBe(200);
      expect(await js.text()).toContain("createRoot");
    });
  });

  test("GET /hub lists live instances from the registry", async () => {
    const instancesDir = join(HOME, "ui", "instances");
    mkdirSync(instancesDir, { recursive: true });
    writeFileSync(
      join(instancesDir, "repo-abc123.json"),
      JSON.stringify({ label: "repo-abc123", cwd: "/work/repo", port: 4142, pid: process.pid, startedAt: "now", version: "0" }),
    );
    writeFileSync(
      join(instancesDir, "gone-000000.json"),
      JSON.stringify({ label: "gone-000000", cwd: "/work/gone", port: 4143, pid: 2 ** 22 + 999, startedAt: "then", version: "0" }),
    );
    await withEnv(uiEnv({ DRIP_UI_HUB: "drip.localhost:4140" }), async () => {
      const response = await api("/hub");
      expect(response.status).toBe(200);
      const body = await response.text();
      expect(body).toContain("1 live instance");
      expect(body).toContain('href="http://drip.localhost:4140/repo-abc123/"');
      expect(body).toContain("/work/repo");
      expect(body).not.toContain("gone-000000");
    });
  });

  test("GET /api/bootstrap serves env-only config", async () => {
    await withEnv({ DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141", DRIP_MAX_CONTEXT_TOKENS: "48000" }, async () => {
      const response = await api("/api/bootstrap");
      expect(response.status).toBe(200);
      expect(await response.json()).toEqual({
        cwd: PROJ,
        home: HOME,
        maxContextTokens: 48000,
        dripBin: fakeBin,
      });
    });
  });
});

describe("GET /api/sessions", () => {
  test("returns running/recent lists from the drip listing", async () => {
    await withEnv({ DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141", FAKE_LIST_JSON: LIST_JSON }, async () => {
      const response = await api("/api/sessions");
      expect(response.status).toBe(200);
      const body = (await response.json()) as { running: unknown[]; recent: unknown[] };
      expect(body.running).toHaveLength(1);
      expect(body.recent).toHaveLength(1);
      expect((body.running[0] as Record<string, unknown>).id).toBe(SESSION_A);
      expect((body.recent[0] as Record<string, unknown>).id).toBe(SESSION_B);
    });
  });

  test("listing failure becomes 502 with stderr text", async () => {
    await withEnv({ DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141", FAKE_LIST_FAIL: "1" }, async () => {
      const response = await api("/api/sessions");
      expect(response.status).toBe(502);
      expect(await response.json()).toEqual({ error: "boom" });
    });
  });

  test("unparseable listing becomes 502, never a silent 200", async () => {
    await withEnv({ DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141", FAKE_LIST_BADJSON: "1" }, async () => {
      const response = await api("/api/sessions");
      expect(response.status).toBe(502);
      const body = (await response.json()) as { error: string };
      expect(body.error).toContain("unparseable");
    });
  });
});

describe("POST /api/run", () => {
  test("spawns a detached run via DRIP_BIN with cwd=DRIP_CWD and returns the handle", async () => {
    const argvPath = join(ROOT, "run-argv.log");
    await withEnv(
      { DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141", FAKE_ARGV: argvPath },
      async () => {
        const response = await api("/api/run", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ goal: "write more tests", maxIterations: 7 }),
        });
        expect(response.status).toBe(200);
        const body = (await response.json()) as { sessionId: string; handle: { sessionId: string } };
        expect(body.sessionId).toBe("new-session-777");
        expect(body.handle.sessionId).toBe("new-session-777");
        const argv = await actionArgv(argvPath);
        expect(argv).toEqual(["--json", "--detach", "--max-iterations", "7", "write more tests"]);
      },
    );
  });

  test("empty goal is 400", async () => {
    await withEnv({ DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141" }, async () => {
      const response = await api("/api/run", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ goal: "   " }),
      });
      expect(response.status).toBe(400);
      const body = (await response.json()) as { error: string };
      expect(body.error).toContain("non-empty");
    });
  });

  test("failed run surfaces 500 with drip stderr", async () => {
    await withEnv({ DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141", FAKE_RUN_FAIL: "1" }, async () => {
      const response = await api("/api/run", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ goal: "explode please" }),
      });
      expect(response.status).toBe(500);
      expect(await response.json()).toEqual({ error: "run exploded" });
    });
  });

  test("detach without a JSON handle is 502", async () => {
    await withEnv({ DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141", FAKE_RUN_NOJSON: "1" }, async () => {
      const response = await api("/api/run", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ goal: "no handle" }),
      });
      expect(response.status).toBe(502);
      const body = (await response.json()) as { error: string };
      expect(body.error).toContain("did not print a JSON handle");
    });
  });

  test("non-integer maxIterations is 400 and never reaches drip", async () => {
    await withEnv({ DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141" }, async () => {
      for (const maxIterations of [2.5, "7", true, 0]) {
        const response = await api("/api/run", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ goal: "write more tests", maxIterations }),
        });
        expect(response.status).toBe(400);
        const body = (await response.json()) as { error: string };
        expect(body.error).toContain("positive integer");
      }
    });
  });

  test("malformed JSON body is 400", async () => {
    await withEnv({ DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141" }, async () => {
      const response = await api("/api/run", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: "{not json",
      });
      expect(response.status).toBe(400);
      const body = (await response.json()) as { error: string };
      expect(body.error).toContain("not valid JSON");
    });
  });
});

describe("POST /api/sessions/:id/message", () => {
  const base = (extra: Record<string, string> = {}) => ({
    DRIP_BIN: fakeBin,
    DRIP_CWD: PROJ,
    DRIP_HOME: HOME,
    DRIP_UI_PORT: "4141",
    FAKE_LIST_JSON: LIST_JSON,
    ...extra,
  });

  test("a live session gets --send and the run's own liveness verdict", async () => {
    const argvPath = join(ROOT, "message-send-argv.log");
    await withEnv(base({ FAKE_ARGV: argvPath, FAKE_SEND_ACTIVE: "1" }), async () => {
      const response = await api(`/api/sessions/${SESSION_A}/message`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ text: "keep going" }),
      });
      expect(response.status).toBe(200);
      expect(await response.json()).toEqual({ ok: true, mode: "send" });
      expect(await actionArgv(argvPath)).toEqual(["--json", "--send", SESSION_A, "keep going"]);
    });
  });

  test("a run that ended in flight is resumed so the queued inbox text is delivered", async () => {
    const argvPath = join(ROOT, "message-inflight-argv.log");
    await withEnv(base({ FAKE_ARGV: argvPath }), async () => {
      const response = await api(`/api/sessions/${SESSION_A}/message`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ text: "keep going" }),
      });
      expect(response.status).toBe(200);
      const body = (await response.json()) as { ok: boolean; mode: string };
      expect(body.mode).toBe("resume");
      // Sent (drip queued it: runActive=false), then a plain resume — no
      // --prompt, the new run consumes the inbox.
      expect(await actionArgv(argvPath)).toEqual([
        "--json", "--send", SESSION_A, "keep going",
        "--resume", SESSION_A, "--json", "--detach",
      ]);
    });
  });

  test("an idle session in a subdirectory is sent to, then resumed, from its own cwd", async () => {
    const argvPath = join(ROOT, "message-resume-argv.log");
    const cwdLog = join(ROOT, "message-resume-cwd.log");
    await withEnv(base({ FAKE_ARGV: argvPath, FAKE_CWD_LOG: cwdLog }), async () => {
      const response = await api(`/api/sessions/${SESSION_B}/message`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ text: "pick it back up" }),
      });
      expect(response.status).toBe(200);
      const body = (await response.json()) as { ok: boolean; mode: string };
      expect(body.mode).toBe("resume");
      // Queued in the inbox by --send (runActive=false), then a plain
      // resume whose run consumes it — never --prompt, which would send
      // the text twice.
      expect(await actionArgv(argvPath)).toEqual([
        "--json", "--send", SESSION_B, "pick it back up",
        "--resume", SESSION_B, "--json", "--detach",
      ]);
      // The sweep ran from the UI root; the actions run from the session's
      // own directory, which is where drip resolves its project from.
      const cwds = (await Bun.file(cwdLog).text()).trim().split("\n");
      expect(cwds[cwds.length - 1]).toBe(realpathSync(join(PROJ, "sub")));
      expect(cwds[0]).toBe(realpathSync(PROJ));
    });
  });

  test("empty text is 400 before drip runs", async () => {
    await withEnv(base(), async () => {
      const response = await api(`/api/sessions/${SESSION_B}/message`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ text: " " }),
      });
      expect(response.status).toBe(400);
    });
  });
});

describe("POST /api/sessions/:id/send", () => {
  test("valid id forwards text via --send with cwd=row.cwd", async () => {
    const argvPath = join(ROOT, "send-argv.log");
    await withEnv(
      { DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141", FAKE_ARGV: argvPath, FAKE_LIST_JSON: LIST_JSON },
      async () => {
        const response = await api(`/api/sessions/${SESSION_A}/send`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ text: 'say "hi" now' }),
        });
        expect(response.status).toBe(200);
        const body = (await response.json()) as { ok: boolean; runActive: boolean; output: string };
        expect(body.ok).toBe(true);
        expect(body.runActive).toBe(false);
        expect(body.output).toBe('{"type":"sent","runActive":false}');
        const argv = await actionArgv(argvPath);
        expect(argv).toEqual(["--json", "--send", SESSION_A, 'say "hi" now']);
      },
    );
  });

  test("empty text is 400 and unknown id is 404", async () => {
    await withEnv({ DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141", FAKE_LIST_JSON: LIST_JSON }, async () => {
      const empty = await api(`/api/sessions/${SESSION_A}/send`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ text: "" }),
      });
      expect(empty.status).toBe(400);

      const unknown = await api("/api/sessions/does-not-exist/send", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ text: "hello" }),
      });
      expect(unknown.status).toBe(404);
      const body = (await unknown.json()) as { error: string };
      expect(body.error).toContain("unknown session");
    });
  });
});

function uiEnv(extra: Record<string, string> = {}): Record<string, string> {
  return { DRIP_BIN: fakeBin, DRIP_CWD: PROJ, DRIP_HOME: HOME, DRIP_UI_PORT: "4141", ...extra };
}

const TRANSCRIPT_A = join(HOME, "sessions", SESSION_A.slice(0, 8), "transcript.jsonl");

describe("GET /api/sessions/:id/state", () => {
  test("returns state.json contents plus listing liveness", async () => {
    await withEnv(uiEnv({ FAKE_LIST_JSON: LIST_JSON }), async () => {
      const response = await api(`/api/sessions/${SESSION_A}/state`);
      expect(response.status).toBe(200);
      expect(await response.json()).toEqual({ state: { tasks: [] }, isRunning: true });
    });
  });

  test("missing state.json yields state null (session B)", async () => {
    await withEnv(uiEnv({ FAKE_LIST_JSON: LIST_JSON }), async () => {
      const response = await api(`/api/sessions/${SESSION_B}/state`);
      expect(response.status).toBe(200);
      expect(await response.json()).toEqual({ state: null, isRunning: false });
    });
  });

  test("unknown session is 404", async () => {
    await withEnv(uiEnv({ FAKE_LIST_JSON: LIST_JSON }), async () => {
      const response = await api("/api/sessions/nope/state");
      expect(response.status).toBe(404);
    });
  });

  test("listing failure during state resolution is 502 with stderr", async () => {
    await withEnv(uiEnv({ FAKE_LIST_FAIL: "1" }), async () => {
      const response = await api(`/api/sessions/${SESSION_A}/state`);
      expect(response.status).toBe(502);
      expect(await response.json()).toEqual({ error: "boom" });
    });
  });
});

describe("GET /api/sessions/:id/transcript", () => {
  test("polls complete lines incrementally and never a partial line", async () => {
    await withEnv(uiEnv({ FAKE_LIST_JSON: LIST_JSON }), async () => {
      const first = await api(`/api/sessions/${SESSION_A}/transcript?offset=0`);
      expect(first.status).toBe(200);
      const firstBody = (await first.json()) as {
        entries: { type: string }[];
        nextOffset: number;
        isRunning: boolean;
      };
      expect(firstBody.entries.map((entry) => entry.type)).toEqual(["goal", "info"]);
      expect(firstBody.isRunning).toBe(true);
      const base = firstBody.nextOffset;
      expect(base).toBe(await Bun.file(TRANSCRIPT_A).size);

      // A trailing partial write must not be consumed...
      writeFileSync(TRANSCRIPT_A, '{"type":"info","te', { flag: "a" });
      const second = await api(`/api/sessions/${SESSION_A}/transcript?offset=${base}`);
      const secondBody = (await second.json()) as { entries: unknown[]; nextOffset: number };
      expect(secondBody.entries).toEqual([]);
      expect(secondBody.nextOffset).toBe(base);

      // ...and completing the line makes exactly that line available.
      writeFileSync(TRANSCRIPT_A, 'xt":"third"}\n', { flag: "a" });
      const third = await api(`/api/sessions/${SESSION_A}/transcript?offset=${base}`);
      const thirdBody = (await third.json()) as {
        entries: { type: string; text: string }[];
        nextOffset: number;
      };
      expect(thirdBody.entries).toEqual([{ type: "info", text: "third" }]);
      expect(thirdBody.nextOffset).toBe(await Bun.file(TRANSCRIPT_A).size);
    });
  });

  test("unknown session is 404", async () => {
    await withEnv(uiEnv({ FAKE_LIST_JSON: LIST_JSON }), async () => {
      const response = await api("/api/sessions/nope/transcript?offset=0");
      expect(response.status).toBe(404);
    });
  });

  test("non-numeric offset is 400", async () => {
    await withEnv(uiEnv({ FAKE_LIST_JSON: LIST_JSON }), async () => {
      const response = await api(`/api/sessions/${SESSION_A}/transcript?offset=abc`);
      expect(response.status).toBe(400);
      const body = (await response.json()) as { error: string };
      expect(body.error).toContain("offset");
    });
  });
});

describe("GET /api/sessions/:id/result", () => {
  test("returns result.json when present and null when absent", async () => {
    await withEnv(uiEnv({ FAKE_LIST_JSON: LIST_JSON }), async () => {
      const present = await api(`/api/sessions/${SESSION_A}/result`);
      expect(present.status).toBe(200);
      expect(await present.json()).toEqual({ ok: true });

      const absent = await api(`/api/sessions/${SESSION_B}/result`);
      expect(absent.status).toBe(200);
      expect(await absent.json()).toBeNull();
    });
  });

  test("unknown session is 404", async () => {
    await withEnv(uiEnv({ FAKE_LIST_JSON: LIST_JSON }), async () => {
      const response = await api("/api/sessions/nope/result");
      expect(response.status).toBe(404);
    });
  });
});

describe("POST /api/sessions/:id/stop", () => {
  test("runs --stop with cwd=row.cwd and returns the output", async () => {
    const argvPath = join(ROOT, "stop-argv.log");
    await withEnv(uiEnv({ FAKE_ARGV: argvPath, FAKE_LIST_JSON: LIST_JSON }), async () => {
      const response = await api(`/api/sessions/${SESSION_A}/stop`, { method: "POST" });
      expect(response.status).toBe(200);
      expect(await response.json()).toEqual({ ok: true, output: "stopping session" });
      const argv = await actionArgv(argvPath);
      expect(argv).toEqual(["--stop", SESSION_A]);
    });
  });

  test("unknown session is 404; listing failure is 502", async () => {
    await withEnv(uiEnv({ FAKE_LIST_JSON: LIST_JSON }), async () => {
      const unknown = await api("/api/sessions/nope/stop", { method: "POST" });
      expect(unknown.status).toBe(404);
    });
    await withEnv(uiEnv({ FAKE_LIST_FAIL: "1" }), async () => {
      const failed = await api(`/api/sessions/${SESSION_A}/stop`, { method: "POST" });
      expect(failed.status).toBe(502);
      expect(await failed.json()).toEqual({ error: "boom" });
    });
  });
});

describe("POST /api/sessions/:id/resume", () => {
  test("resume with prompt posts --prompt and returns the handle", async () => {
    const argvPath = join(ROOT, "resume-argv.log");
    await withEnv(uiEnv({ FAKE_ARGV: argvPath, FAKE_LIST_JSON: LIST_JSON }), async () => {
      const response = await api(`/api/sessions/${SESSION_B}/resume`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ prompt: "continue the work" }),
      });
      expect(response.status).toBe(200);
      const body = (await response.json()) as { ok: boolean; handle: { sessionId: string } };
      expect(body.ok).toBe(true);
      expect(body.handle.sessionId).toBe("resumed-42");
      const argv = await actionArgv(argvPath);
      expect(argv).toEqual(["--resume", SESSION_B, "--json", "--detach", "--prompt", "continue the work"]);
    });
  });

  test("resume without prompt omits --prompt; whitespace prompt is 400", async () => {
    const argvPath = join(ROOT, "resume-noargv.log");
    await withEnv(uiEnv({ FAKE_ARGV: argvPath, FAKE_LIST_JSON: LIST_JSON }), async () => {
      const plain = await api(`/api/sessions/${SESSION_B}/resume`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({}),
      });
      expect(plain.status).toBe(200);
      const argv = await actionArgv(argvPath);
      expect(argv).toEqual(["--resume", SESSION_B, "--json", "--detach"]);

      const blank = await api(`/api/sessions/${SESSION_B}/resume`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ prompt: "   " }),
      });
      expect(blank.status).toBe(400);
    });
  });

  test("unknown session is 404", async () => {
    await withEnv(uiEnv({ FAKE_LIST_JSON: LIST_JSON }), async () => {
      const response = await api("/api/sessions/nope/resume", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ prompt: "hi" }),
      });
      expect(response.status).toBe(404);
    });
  });
});

describe("routing and error funnel", () => {
  test("unknown /api route and unknown path are 404 JSON", async () => {
    await withEnv(uiEnv(), async () => {
      const noRoute = await api("/api/nope");
      expect(noRoute.status).toBe(404);
      expect((await noRoute.json()).error).toContain("no route");

      const noPath = await api("/whatever");
      expect(noPath.status).toBe(404);
    });
  });

  test("wrong method on a known path is 404", async () => {
    await withEnv(uiEnv(), async () => {
      const response = await api("/api/run", {
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ goal: "x" }),
      });
      expect(response.status).toBe(404);
    });
  });

  test("empty POST /api/run body is 400 (missing goal)", async () => {
    await withEnv(uiEnv(), async () => {
      const response = await api("/api/run", { method: "POST" });
      expect(response.status).toBe(400);
    });
  });
});

describe("cross-origin protection", () => {
  // fakeBin is created in beforeAll, so the env is built per test.
  const env = () => uiEnv({ FAKE_LIST_JSON: LIST_JSON });

  test("a mutating request from another origin is refused before drip runs", async () => {
    await withEnv(env(), async () => {
      const response = await api(`/api/sessions/${SESSION_A}/stop`, {
        method: "POST",
        headers: { origin: "http://evil.example", host: "127.0.0.1:4141" },
      });
      expect(response.status).toBe(403);
    });
  });

  test("sec-fetch-site cross-site is refused even without an origin header", async () => {
    await withEnv(env(), async () => {
      const response = await api(`/api/sessions/${SESSION_A}/stop`, {
        method: "POST",
        headers: { "sec-fetch-site": "cross-site" },
      });
      expect(response.status).toBe(403);
    });
  });

  test("same-origin and origin-less (curl) mutations pass through", async () => {
    await withEnv(env(), async () => {
      const same = await api(`/api/sessions/${SESSION_A}/stop`, {
        method: "POST",
        headers: { origin: "http://127.0.0.1:4141", host: "127.0.0.1:4141", "sec-fetch-site": "same-origin" },
      });
      expect(same.status).toBe(200);
      const bare = await api(`/api/sessions/${SESSION_A}/stop`, { method: "POST" });
      expect(bare.status).toBe(200);
    });
  });

  test("a rebound Host is refused even when Origin agrees with it", async () => {
    // DNS rebinding: attacker.example resolves to 127.0.0.1, so Host and
    // Origin match each other — only the loopback check catches it.
    await withEnv(env(), async () => {
      const response = await api(`/api/sessions/${SESSION_A}/stop`, {
        method: "POST",
        headers: { origin: "http://attacker.example:4141", host: "attacker.example:4141" },
      });
      expect(response.status).toBe(403);
    });
  });

  test("a rebound Host cannot read either: sessions, state and transcript are refused", async () => {
    await withEnv(env(), async () => {
      const headers = { host: "attacker.example:4141" };
      for (const path of ["/api/sessions", `/api/sessions/${SESSION_A}/state`, `/api/sessions/${SESSION_A}/transcript?offset=0`]) {
        const response = await api(path, { headers });
        expect(response.status).toBe(403);
      }
      // The same reads pass with our own Host, and the hub host.
      expect((await api("/api/sessions", { headers: { host: "127.0.0.1:4141" } })).status).toBe(200);
      expect((await api("/api/sessions", { headers: { host: "localhost:4141" } })).status).toBe(200);
    });
  });

  test("the Caddy hub host is this server too", async () => {
    await withEnv({ ...env(), DRIP_UI_HUB: "drip.localhost:4140" }, async () => {
      const response = await api(`/api/sessions/${SESSION_A}/stop`, {
        method: "POST",
        headers: { origin: "http://drip.localhost:4140", host: "drip.localhost:4140", "sec-fetch-site": "same-origin" },
      });
      expect(response.status).toBe(200);
      const other = await api(`/api/sessions/${SESSION_A}/stop`, {
        method: "POST",
        headers: { origin: "http://drip.localhost:4141", host: "drip.localhost:4141" },
      });
      expect(other.status).toBe(403);
    });
  });

  test("localhost on the served port is this server", async () => {
    await withEnv(env(), async () => {
      const response = await api(`/api/sessions/${SESSION_A}/stop`, {
        method: "POST",
        headers: { origin: "http://localhost:4141", host: "localhost:4141" },
      });
      expect(response.status).toBe(200);
    });
  });

  test("GET routes ignore origin", async () => {
    await withEnv(env(), async () => {
      const response = await api("/api/bootstrap", { headers: { origin: "http://evil.example" } });
      expect(response.status).toBe(200);
    });
  });
});
