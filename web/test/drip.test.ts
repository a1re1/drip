// Tests for web/lib/drip.ts — pure helpers plus the bridge against a fake
// drip executable (a shell script that echoes recorded output), so the
// argv/cwd contract is exercised the same way the real server will.
// The listing cache would let one test see another's FAKE_* env.
process.env.DRIP_UI_LIST_CACHE_MS = "0";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync, appendFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  bootstrap,
  dripConfig,
  DripError,
  findSession,
  lastNonEmptyLine,
  listSessions,
  messageSession,
  parseDetachHandle,
  readJsonlFrom,
  resumeSession,
  runDrip,
  sendToSession,
  sessionResult,
  sessionState,
  sessionTranscript,
  startRun,
  stopSession,
  type SessionRow,
} from "../lib/drip";

let tmp = "";

beforeEach(() => {
  tmp = mkdtempSync(join(tmpdir(), "drip-bridge-"));
});

afterEach(() => {
  rmSync(tmp, { recursive: true, force: true });
});

/** POSIX single-quote a string for embedding in a generated shell script. */
function shq(s: string): string {
  return "'" + s.replace(/'/g, "'\\''") + "'";
}

function writeScript(name: string, body: string): string {
  const path = join(tmp, name);
  writeFileSync(path, body, { mode: 0o755 });
  return path;
}

/**
 * Fake drip answering with the given streams regardless of argv. When
 * FAKE_ARGV is set it appends one line per invocation — each arg base64
 * encoded, space separated — so tests can assert the exact argv that reached
 * the binary (runDrip must never route through a shell, so spaces/quotes
 * survive intact).
 */
function fakeDrip(stdout: string, stderr = "", code = 0): string {
  const lines = [
    "#!/bin/bash",
    'if [ -n "${FAKE_ARGV:-}" ]; then',
    '  for a in "$@"; do printf \'%s \' "$(printf \'%s\' "$a" | base64 | tr -d \'\\n\')" >> "$FAKE_ARGV"; done',
    '  echo >> "$FAKE_ARGV"',
    "fi",
    "printf '%s' " + shq(stdout),
  ];
  if (stderr !== "") lines.push("printf '%s' " + shq(stderr) + " >&2");
  lines.push("exit " + String(code), "");
  return writeScript("fake-drip.sh", lines.join("\n"));
}

/** Failure-path sugar: a fake that prints nothing on stdout. */
function failingDrip(stderr: string, code: number): string {
  return fakeDrip("", stderr, code);
}

/**
 * Fake drip that answers different payloads per flag ($1) — for actions whose
 * bridge first re-lists sessions with the same binary.
 */
function fakeDripDispatch(
  handlers: Record<string, { stdout?: string; stderr?: string; code?: number }>,
): string {
  const capture = [
    'if [ -n "${FAKE_ARGV:-}" ]; then',
    '  for a in "$@"; do printf \'%s \' "$(printf \'%s\' "$a" | base64 | tr -d \'\\n\')" >> "$FAKE_ARGV"; done',
    '  echo >> "$FAKE_ARGV"',
    "fi",
  ];
  const blocks = Object.entries(handlers).map(([flag, r]) => {
    const lines = ["if [ \"$1\" = " + shq(flag) + " ]; then"];
    if (r.stderr) lines.push("  printf '%s' " + shq(r.stderr) + " >&2");
    lines.push("  printf '%s' " + shq(r.stdout ?? ""));
    lines.push("  exit " + String(r.code ?? 0));
    lines.push("fi");
    return lines.join("\n");
  });
  return writeScript(
    "fake-drip-dispatch.sh",
    "#!/bin/bash\n" + capture.join("\n") + "\n" + blocks.join("\n") + "\nprintf '%s' \"unexpected invocation: $*\" >&2\nexit 1\n",
  );
}

/** Decodes the FAKE_ARGV capture: one invocation per line, base64 args. */
function readArgvCapture(): string[][] {
  try {
    return readFileSync(join(tmp, "argv"), "utf8")
      .split("\n")
      .filter((line) => line.trim() !== "")
      .map((line) =>
        line
          .split(" ")
          .filter((token) => token !== "")
          .map((token) => Buffer.from(token, "base64").toString("utf8")),
      );
  } catch {
    return [];
  }
}

function row(overrides: Partial<SessionRow> & { id: string }): SessionRow {
  const dir = join(tmp, "sessions", overrides.id);
  mkdirSync(dir, { recursive: true });
  return {
    createdAt: "2026-09-10T10:00:00Z",
    cwd: tmp,
    dir,
    goalCount: 1,
    lastGoal: "demo goal",
    lastRun: null,
    running: false,
    statePath: join(dir, "state.json"),
    status: "completed",
    transcriptPath: join(dir, "transcript.jsonl"),
    updatedAt: "2026-09-10T10:05:00Z",
    ...overrides,
  };
}

function writeList(rows: SessionRow[]): string {
  return fakeDrip(JSON.stringify(rows));
}

describe("lastNonEmptyLine and parseDetachHandle", () => {
  test("lastNonEmptyLine skips trailing blank lines and whitespace-only lines", () => {
    expect(lastNonEmptyLine("")).toBe("");
    expect(lastNonEmptyLine("\n  \n")).toBe("");
    expect(lastNonEmptyLine('{"a":1}\n')).toBe('{"a":1}');
    expect(lastNonEmptyLine('plugin chatter\n{"a":1}\n\n  \n')).toBe('{"a":1}');
  });

  test("parseDetachHandle takes the last JSON line and rejects a missing or malformed handle", () => {
    const parsed = parseDetachHandle('noise\n{"sessionId":"abc","type":"detached","pid":42}\n');
    expect(parsed.sessionId).toBe("abc");
    expect(parsed.handle).toEqual({ sessionId: "abc", type: "detached", pid: 42 });
    for (const stdout of ["", "started\n", '{"type":"detached"}\n', '{"sessionId":7}\n', "{not json\n"]) {
      let caught: unknown = null;
      try {
        parseDetachHandle(stdout);
      } catch (error) {
        caught = error;
      }
      expect(caught).toBeInstanceOf(DripError);
      expect((caught as DripError).status).toBe(502);
      expect((caught as DripError).message).toContain("did not print a JSON handle");
    }
  });
});

describe("runDrip failures", () => {
  test("a missing binary or a vanished cwd is a 502 DripError, not a raw exception", async () => {
    let caught: unknown = null;
    try {
      await runDrip(["--list"], { bin: join(tmp, "no-such-drip") });
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(DripError);
    expect((caught as DripError).status).toBe(502);
    caught = null;
    try {
      await runDrip(["--list"], { bin: fakeDrip("[]"), cwd: join(tmp, "deleted-project") });
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(DripError);
    expect((caught as DripError).status).toBe(502);
  });
});

describe("runDrip timeout and listing backpressure", () => {
  test("a hung drip child is killed and reported as 504", async () => {
    const bin = writeScript("hung-drip.sh", "#!/bin/bash\nsleep 30\n");
    const started = Date.now();
    let caught: unknown = null;
    try {
      await runDrip(["--list"], { bin, timeoutMs: 200 });
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(DripError);
    expect((caught as DripError).status).toBe(504);
    expect((caught as DripError).message).toContain("200ms");
    expect(Date.now() - started).toBeLessThan(5_000);
  });

  test("polls during a sweep slower than the cache TTL share the one in flight", async () => {
    const counter = join(tmp, "sweeps.count");
    const bin = writeScript(
      "slow-list.sh",
      `#!/bin/bash\necho x >> ${shq(counter)}\nsleep 0.4\necho '[]'\n`,
    );
    process.env.DRIP_UI_LIST_CACHE_MS = "50";
    try {
      const first = listSessions({ bin });
      await new Promise((resolve) => setTimeout(resolve, 150)); // past the TTL, sweep still running
      const second = listSessions({ bin });
      await Promise.all([first, second]);
      expect(readFileSync(counter, "utf8").trim().split("\n")).toHaveLength(1);
      // Once settled and past the TTL a new sweep is spawned.
      await new Promise((resolve) => setTimeout(resolve, 60));
      await listSessions({ bin });
      expect(readFileSync(counter, "utf8").trim().split("\n")).toHaveLength(2);
    } finally {
      process.env.DRIP_UI_LIST_CACHE_MS = "0";
    }
  });
});

describe("messageSession when the resume is refused", () => {
  test("queued (not failed) only when a fresh lease shows the run went live in between", async () => {
    const listFile = join(tmp, "queued-list.json");
    const liveList = join(tmp, "queued-list-live.json");
    writeFileSync(listFile, JSON.stringify([row({ id: "queued", running: false })]));
    writeFileSync(liveList, JSON.stringify([row({ id: "queued", running: true, pid: 4242 })]));
    // --send queues the text and, as a side effect of the race, the run goes live.
    const bin = writeScript(
      "refusing-drip.sh",
      `#!/bin/bash\nif [ "$1" = "--list" ]; then cat ${shq(listFile)}; elif [ "$1" = "--json" ]; then cp ${shq(liveList)} ${shq(listFile)}; echo '{"sessionId":"queued","runActive":false}'; else echo "A goal is already running in session queued (pid 4242)." >&2; exit 1; fi\n`,
    );
    const result = await messageSession("queued", { text: "later" }, { bin });
    expect(result.ok).toBe(true);
    expect(result.mode).toBe("queued");
    expect(result.detail).toContain("already running");
  });

  test("a resume that fails while the session is still idle is an error, not a queued message", async () => {
    const listFile = join(tmp, "broken-list.json");
    writeFileSync(listFile, JSON.stringify([row({ id: "broken", running: false })]));
    const bin = writeScript(
      "broken-drip.sh",
      `#!/bin/bash\nif [ "$1" = "--list" ]; then cat ${shq(listFile)}; elif [ "$1" = "--json" ]; then echo '{"sessionId":"broken","runActive":false}'; else echo "profile not found" >&2; exit 1; fi\n`,
    );
    let caught: unknown = null;
    try {
      await messageSession("broken", { text: "later" }, { bin });
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(DripError);
    expect((caught as DripError).message).toContain("profile not found");
  });
});

describe("readJsonlFrom", () => {
  test("empty and missing files", async () => {
    writeFileSync(join(tmp, "empty.jsonl"), "");
    expect(await readJsonlFrom(join(tmp, "empty.jsonl"), 0)).toEqual({
      entries: [],
      nextOffset: 0,
      reset: false,
    });
    expect(await readJsonlFrom(join(tmp, "missing.jsonl"), 0)).toEqual({
      entries: [],
      nextOffset: 0,
      reset: false,
    });
  });

  test("a file shorter than the cursor replays from byte 0 with reset", async () => {
    const path = join(tmp, "rewritten.jsonl");
    writeFileSync(path, '{"a":1}\n{"a":2}\n{"a":3}\n');
    const first = await readJsonlFrom(path, 0);
    expect(first.nextOffset).toBe(24);
    expect(first.reset).toBe(false);
    // The session is resumed and its transcript rewritten shorter.
    writeFileSync(path, '{"b":1}\n');
    const after = await readJsonlFrom(path, first.nextOffset);
    expect(after).toEqual({ entries: [{ b: 1 }], nextOffset: 8, reset: true });
    // Truncated to empty: still a reset, cursor back at 0, so growth is seen.
    writeFileSync(path, "");
    expect(await readJsonlFrom(path, 8)).toEqual({ entries: [], nextOffset: 0, reset: true });
    appendFileSync(path, '{"c":1}\n');
    expect(await readJsonlFrom(path, 0)).toEqual({ entries: [{ c: 1 }], nextOffset: 8, reset: false });
  });

  test("consumes complete lines, never a trailing partial", async () => {
    const path = join(tmp, "t.jsonl");
    writeFileSync(path, '{"a":1}\n{"a":2}\n{"a":3');
    const first = await readJsonlFrom(path, 0);
    expect(first.entries).toEqual([{ a: 1 }, { a: 2 }]);
    expect(first.nextOffset).toBe(16);
    // The poller passes nextOffset back; the partial line stays unread.
    const second = await readJsonlFrom(path, first.nextOffset);
    expect(second.entries).toEqual([]);
    expect(second.nextOffset).toBe(first.nextOffset);
    // Writer completes the line (plus a new one); the next poll returns both.
    appendFileSync(path, '}\n{"a":4}\n');
    const third = await readJsonlFrom(path, first.nextOffset);
    expect(third.entries).toEqual([{ a: 3 }, { a: 4 }]);
    expect(third.nextOffset).toBe(32);
  });

  test("UTF-8 multi-byte content survives byte-offset polling", async () => {
    const path = join(tmp, "utf8.jsonl");
    const line1 = '{"text":"héllo — ünïcode ✓"}\n';
    writeFileSync(path, line1);
    const first = await readJsonlFrom(path, 0);
    expect(first.entries).toEqual([{ text: "héllo — ünïcode ✓" }]);
    expect(first.nextOffset).toBe(Buffer.byteLength(line1, "utf8"));
    // Append the second (multi-byte) line; the incremental poll must slice on
    // byte offsets without splitting a code point.
    appendFileSync(path, '{"text":"日本語テキスト"}\n');
    const second = await readJsonlFrom(path, first.nextOffset);
    expect(second.entries).toEqual([{ text: "日本語テキスト" }]);
    expect(second.nextOffset).toBeGreaterThan(first.nextOffset);
  });
});

describe("config and bootstrap", () => {
  test("env-only config with defaults", () => {
    const config = dripConfig({
      DRIP_BIN: "/usr/local/bin/drip",
      DRIP_CWD: "/tmp/proj",
      DRIP_HOME: "/Users/x/.drip",
      DRIP_UI_PORT: "5151",
      DRIP_MAX_CONTEXT_TOKENS: "128000",
    });
    expect(config).toEqual({
      dripBin: "/usr/local/bin/drip",
      cwd: "/tmp/proj",
      home: "/Users/x/.drip",
      port: 5151,
      maxContextTokens: 128000,
    });
    expect(bootstrap(config)).toEqual({
      cwd: "/tmp/proj",
      home: "/Users/x/.drip",
      maxContextTokens: 128000,
      dripBin: "/usr/local/bin/drip",
    });
  });

  test("unset or malformed env falls back safely", () => {
    expect(dripConfig({}).maxContextTokens).toBeNull();
    expect(dripConfig({ DRIP_MAX_CONTEXT_TOKENS: "not-a-number" }).maxContextTokens).toBeNull();
    expect(dripConfig({ DRIP_UI_PORT: "abc" }).port).toBe(4141);
    expect(dripConfig({}).dripBin).toBe("drip");
  });
});

describe("runDrip", () => {
  test("passes argv verbatim and never a shell", async () => {
    process.env.FAKE_ARGV = join(tmp, "argv");
    const bin = fakeDrip("");
    const out = await runDrip(["--send", "sess with spaces", 'quote"me'], {
      bin,
      cwd: tmp,
    });
    expect(out.code).toBe(0);
    const calls = readArgvCapture();
    expect(calls.length).toBe(1);
    // Bash "$@" excludes the script path, so the capture holds argv after
    // the binary — spaces and quotes intact (never routed through a shell).
    expect(calls[0]).toEqual(["--send", "sess with spaces", 'quote"me']);
    delete process.env.FAKE_ARGV;
  });

  test("captures stdout, stderr and exit code", async () => {
    const out = await runDrip(["--whatever"], { bin: failingDrip("boom\n", 3), cwd: tmp });
    expect(out.code).toBe(3);
    expect(out.stderr).toContain("boom");
  });
});

describe("listSessions", () => {
  test("splits running/recent and sorts each by updatedAt desc", async () => {
    const bin = writeList([
      row({ id: "old-running", running: true, updatedAt: "2026-09-10T08:00:00Z" }),
      row({ id: "new-running", running: true, updatedAt: "2026-09-10T09:00:00Z" }),
      row({ id: "new-recent", updatedAt: "2026-09-10T11:00:00Z" }),
      row({ id: "old-recent", updatedAt: "2026-09-10T07:00:00Z" }),
    ]);
    const lists = await listSessions({ bin });
    expect(lists.running.map((r) => r.id)).toEqual(["new-running", "old-running"]);
    expect(lists.recent.map((r) => r.id)).toEqual(["new-recent", "old-recent"]);
  });

  test("failed listing surfaces stderr as DripError, never a silent empty list", async () => {
    let caught: unknown = null;
    try {
      await listSessions({ bin: failingDrip("registry locked\n", 2) });
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(DripError);
    expect((caught as DripError).status).toBe(502);
    expect((caught as DripError).message).toContain("registry locked");
  });

  test("a session missing from the cached sweep triggers one fresh sweep before 404", async () => {
    // Same binary, output read from a file we rewrite between calls, so the
    // cache key stays identical and only a refresh can see the new row.
    const listFile = join(tmp, "list-cache.json");
    writeFileSync(listFile, JSON.stringify([row({ id: "known" })]));
    const bin = writeScript("cached-drip.sh", `#!/bin/bash\ncat ${shq(listFile)}\n`);
    const saved = process.env.DRIP_UI_LIST_CACHE_MS;
    process.env.DRIP_UI_LIST_CACHE_MS = "60000";
    try {
      expect((await findSession("known", { bin })).id).toBe("known");
      writeFileSync(listFile, JSON.stringify([row({ id: "known" }), row({ id: "just-started" })]));
      // Cached: the plain listing still shows the stale sweep…
      expect((await listSessions({ bin })).recent.map((r) => r.id)).toEqual(["known"]);
      // …but resolving the new id refreshes instead of trusting a 404.
      expect((await findSession("just-started", { bin })).id).toBe("just-started");
      expect((await listSessions({ bin })).recent.map((r) => r.id)).toContain("just-started");
    } finally {
      if (saved === undefined) delete process.env.DRIP_UI_LIST_CACHE_MS;
      else process.env.DRIP_UI_LIST_CACHE_MS = saved;
    }
  });

  test("a miss from a fresh sweep is final — no second sweep", async () => {
    const listFile = join(tmp, "list-count.json");
    const countFile = join(tmp, "list-count.n");
    writeFileSync(listFile, JSON.stringify([row({ id: "known" })]));
    writeFileSync(countFile, "");
    const bin = writeScript(
      "counting-drip.sh",
      `#!/bin/bash\necho x >> ${shq(countFile)}\ncat ${shq(listFile)}\n`,
    );
    const sweeps = () => readFileSync(countFile, "utf8").split("\n").filter((l) => l !== "").length;
    const saved = process.env.DRIP_UI_LIST_CACHE_MS;
    process.env.DRIP_UI_LIST_CACHE_MS = "60000";
    try {
      await expect(findSession("nosuch", { bin })).rejects.toBeInstanceOf(DripError);
      // Cache was cold: one real sweep, and its miss is trusted.
      expect(sweeps()).toBe(1);
      await expect(findSession("nosuch", { bin })).rejects.toBeInstanceOf(DripError);
      // Cache was warm: the cached miss earns exactly one refresh.
      expect(sweeps()).toBe(2);
      // messageSession decides from --send's own verdict, never from the
      // (possibly stale) listing: the cached row says running, drip says
      // the run is gone → the queued text is delivered by a plain resume.
      writeFileSync(listFile, JSON.stringify([row({ id: "known", running: true })]));
      const argvFile = join(tmp, "message-fresh.argv");
      const live = writeScript(
        "fresh-drip.sh",
        `#!/bin/bash\nif [ "$1" = "--list" ]; then cat ${shq(listFile)}; else echo "$@" >> ${shq(argvFile)}; echo '{"sessionId":"known","runActive":false}'; fi\n`,
      );
      await listSessions({ bin: live }); // warm the cache with the live row
      const before = sweeps();
      const result = await messageSession("known", { text: "go" }, { bin: live });
      expect(result.mode).toBe("resume");
      expect(readFileSync(argvFile, "utf8")).toBe("--json --send known go\n--resume known --json --detach\n");
      // The cached row was enough to locate the session: no extra sweep.
      expect(sweeps()).toBe(before);
    } finally {
      if (saved === undefined) delete process.env.DRIP_UI_LIST_CACHE_MS;
      else process.env.DRIP_UI_LIST_CACHE_MS = saved;
    }
  });

  test("unknown session resolves to 404 only after a real listing", async () => {
    const bin = writeList([row({ id: "known" })]);
    let caught: unknown = null;
    try {
      await findSession("nosuch", { bin });
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(DripError);
    expect((caught as DripError).status).toBe(404);
  });
});

describe("session files", () => {
  test("state returns file contents plus liveness", async () => {
    const target = row({ id: "with-state", running: true });
    writeFileSync(target.statePath, JSON.stringify({ tasks: [{ title: "t", status: "done" }] }));
    const bin = writeList([target]);
    const state = await sessionState("with-state", { bin });
    expect(state.isRunning).toBe(true);
    expect(state.state).toEqual({ tasks: [{ title: "t", status: "done" }] });
  });

  test("result returns null when result.json is absent", async () => {
    const target = row({ id: "no-result" });
    const bin = writeList([target]);
    expect(await sessionResult("no-result", { bin })).toBeNull();
  });

  test("result reads the resultPath drip reports, falling back to the state.json sibling for older rows", async () => {
    const elsewhere = join(tmp, "elsewhere-result.json");
    writeFileSync(elsewhere, JSON.stringify({ reason: "completed" }));
    const reported = row({ id: "reported", resultPath: elsewhere });
    const legacy = row({ id: "legacy" });
    delete (legacy as Partial<SessionRow>).resultPath;
    writeFileSync(legacy.statePath.replace(/state\.json$/, "result.json"), JSON.stringify({ reason: "budget" }));
    const bin = writeList([reported, legacy]);
    expect(await sessionResult("reported", { bin })).toEqual({ reason: "completed" });
    expect(await sessionResult("legacy", { bin })).toEqual({ reason: "budget" });
  });

  test("transcript polls through the same byte offset pipeline", async () => {
    const target = row({ id: "with-transcript" });
    writeFileSync(target.transcriptPath, '{"type":"goal"}\n{"type":"model"}\n');
    const bin = writeList([target]);
    const poll = await sessionTranscript("with-transcript", 0, { bin });
    expect(poll.entries).toEqual([{ type: "goal" }, { type: "model" }]);
    expect(poll.isRunning).toBe(false);
  });
});

describe("actions", () => {
  test("startRun validates goal and parses the last non-empty detach line", async () => {
    try {
      await startRun({ goal: "   " }, { bin: fakeDrip("") });
      expect.unreachable();
    } catch (error) {
      expect(error).toBeInstanceOf(DripError);
      expect((error as DripError).status).toBe(400);
    }

    process.env.FAKE_ARGV = join(tmp, "argv");
    const handleJson = JSON.stringify({
      sessionId: "sess-1234",
      type: "started",
      pid: 4242,
    });
    const bin = fakeDrip(`noise line\n\n${handleJson}\n`);
    const result = await startRun({ goal: "ship it", maxIterations: 7 }, { bin, cwd: tmp });
    expect(result.sessionId).toBe("sess-1234");
    expect(result.handle).toEqual({ sessionId: "sess-1234", type: "started", pid: 4242 });
    const calls = readArgvCapture();
    const last = calls[calls.length - 1] ?? [];
    expect(last).toEqual(["--json", "--detach", "--max-iterations", "7", "ship it"]);
    delete process.env.FAKE_ARGV;
  });

  test("startRun failure becomes a 500 with stderr", async () => {
    let caught: unknown = null;
    try {
      await startRun({ goal: "x" }, { bin: failingDrip("no registry\n", 1) });
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(DripError);
    expect((caught as DripError).status).toBe(500);
    expect((caught as DripError).message).toContain("no registry");
  });

  test("send validates text, resolves the row and reads runActive", async () => {
    process.env.FAKE_ARGV = join(tmp, "argv");
    const target = row({ id: "sendee", running: true });
    const bin = fakeDripDispatch({
      "--list": { stdout: JSON.stringify([target]) },
      // `--json` leads the send argv so drip prints the result line.
      "--json": {
        stdout: JSON.stringify({ runActive: true, type: "sent", sessionId: "sendee" }) + "\n",
      },
    });
    const result = await sendToSession("sendee", { text: "pause here" }, { bin });
    expect(result).toEqual({ ok: true, runActive: true, output: expect.any(String) });
    const calls = readArgvCapture();
    const last = calls[calls.length - 1] ?? [];
    expect(last).toEqual(["--json", "--send", "sendee", "pause here"]);
    // The dispatch fake exits 1 on unmatched flags, so a successful result
    // already proves the injected bin (not a shell or other binary) ran.
    delete process.env.FAKE_ARGV;

    try {
      await sendToSession("sendee", { text: "" }, { bin });
      expect.unreachable();
    } catch (error) {
      expect((error as DripError).status).toBe(400);
    }
  });

  test("stop and resume route through resolved ids", async () => {
    process.env.FAKE_ARGV = join(tmp, "argv");
    const target = row({ id: "stoppable" });
    // One fake binary answers per flag, exactly like the real drip: actions
    // that resolve an id first re-list sessions with the same executable.
    const bin = fakeDripDispatch({
      "--list": { stdout: JSON.stringify([target]) },
      "--stop": { stdout: "stopped session stoppable\n" },
      "--resume": { stdout: JSON.stringify({ sessionId: "stoppable", type: "started" }) + "\n" },
    });
    const stopped = await stopSession("stoppable", { bin });
    expect(stopped.ok).toBe(true);
    const calls = readArgvCapture();
    // Two invocations so far: the id-validating --list, then the action.
    expect(calls.length).toBe(2);
    expect(calls[1]).toEqual(["--stop", "stoppable"]);

    const resumed = await resumeSession("stoppable", { prompt: "continue" }, { bin });
    expect(resumed.ok).toBe(true);
    const after = readArgvCapture();
    expect(after.length).toBe(4);
    expect(after[3]).toEqual([
      "--resume",
      "stoppable",
      "--json",
      "--detach",
      "--prompt",
      "continue",
    ]);
    delete process.env.FAKE_ARGV;
  });

  test("failed action surfaces stderr as 502, never a silent 200", async () => {
    const target = row({ id: "doomed" });
    const bin = fakeDripDispatch({
      "--list": { stdout: JSON.stringify([target]) },
      "--stop": { stderr: "session gone", code: 1 },
    });
    let caught: unknown = null;
    try {
      await stopSession("doomed", { bin });
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(DripError);
    expect((caught as DripError).status).toBe(502);
    expect((caught as DripError).message).toContain("session gone");
  });

  test("listing failure during an action surfaces stderr as 502", async () => {
    let caught: unknown = null;
    try {
      await stopSession("ghost", { bin: failingDrip("no such session\n", 1) });
    } catch (error) {
      caught = error;
    }
    // The id-validating listing itself failed: non-2xx with stderr text,
    // never a silent 200.
    expect(caught).toBeInstanceOf(DripError);
    expect((caught as DripError).status).toBe(502);
    expect((caught as DripError).message).toContain("no such session");
  });
});
