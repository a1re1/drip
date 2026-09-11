// Thin bridge between the UI server and the drip CLI. Everything here is
// network-free so `bun test` can exercise it directly; server.ts only adds
// HTTP routing on top.
//
// Contract with the Rust side (src/cli/entry.rs):
// - `drip --list --json --recursive` prints one JSON array of session rows
//   (keys camelCase: id, cwd, dir, statePath, transcriptPath, running, ...).
// - `--detach` prints a single-line JSON handle whose `sessionId` is the new
//   session (entry.rs:2024-2031).
// - `--json --send` prints `{ type: "sent", runActive, ... }` (entry.rs, the
//   --send arm), where runActive is the lease at send time; `--stop` prints
//   a human-readable line; both exit non-zero on failure with text on stderr.

/** Row shape of `drip --list --json` (entry.rs print_session_list). */
export interface SessionRow {
  id: string;
  createdAt: string;
  cwd: string;
  dir: string;
  goalCount: number;
  lastGoal: string | null;
  lastRun: { endedAt: string; reason: string; taskStats: unknown } | null;
  pid?: number;
  /** The run record drip itself reads for --result; sibling of state.json on current sessions. */
  resultPath?: string;
  running: boolean;
  statePath: string;
  status: string;
  transcriptPath: string;
  updatedAt: string;
}

export interface SessionLists {
  running: SessionRow[];
  recent: SessionRow[];
}

/** Error carrying the HTTP status the server should respond with. */
export class DripError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "DripError";
    this.status = status;
  }
}

/** Immutable server configuration — sourced only from the environment. */
export interface DripConfig {
  dripBin: string;
  cwd: string;
  home: string;
  port: number;
  maxContextTokens: number | null;
}

export function dripConfig(env: Record<string, string | undefined> = process.env): DripConfig {
  const port = Number.parseInt(env.DRIP_UI_PORT ?? "4141", 10);
  const maxRaw = env.DRIP_MAX_CONTEXT_TOKENS;
  const maxParsed = maxRaw === undefined || maxRaw === "" ? null : Number.parseInt(maxRaw, 10);
  return {
    dripBin: env.DRIP_BIN ?? "drip",
    cwd: env.DRIP_CWD ?? process.cwd(),
    home: env.DRIP_HOME ?? "",
    port: Number.isFinite(port) ? port : 4141,
    maxContextTokens: maxParsed !== null && Number.isFinite(maxParsed) ? maxParsed : null,
  };
}

export interface RunOutcome {
  code: number;
  stdout: string;
  stderr: string;
}

/**
 * The single choke point for invoking drip. Argv-only (never a shell) so
 * goals and prompts with spaces/quotes arrive intact.
 */
/** Longest a drip child may run before the bridge kills it and reports 504 (DRIP_UI_DRIP_TIMEOUT_MS). */
export function dripTimeoutMs(): number {
  const raw = Number(process.env.DRIP_UI_DRIP_TIMEOUT_MS ?? "30000");
  return Number.isFinite(raw) && raw > 0 ? raw : 30_000;
}

export async function runDrip(
  args: string[],
  options: { cwd?: string; bin?: string; timeoutMs?: number } = {},
): Promise<RunOutcome> {
  const bin = options.bin ?? dripConfig().dripBin;
  const cwd = options.cwd ?? dripConfig().cwd;
  const timeoutMs = options.timeoutMs ?? dripTimeoutMs();
  let proc: Bun.Subprocess<"ignore", "pipe", "pipe">;
  try {
    proc = Bun.spawn([bin, ...args], {
      // Bun.spawn does not observe process.env mutations made after startup, so
      // the (possibly test-mutated) environment must be passed explicitly.
      env: process.env,
      cwd,
      stdout: "pipe",
      stderr: "pipe",
      stdin: "ignore",
    });
  } catch (error) {
    // A missing binary, or a session directory deleted since the sweep:
    // report it as a gateway failure rather than a raw exception.
    throw new DripError(502, `could not run ${bin} in ${cwd}: ${(error as Error).message}`);
  }
  // Every bridge call is a short, non-interactive drip invocation (a listing
  // sweep, a detached start, a send); one that hangs must not pin a poller
  // — or the shared listing cache — forever.
  // The output readers are not awaited past the deadline either: a killed
  // shell may leave a grandchild holding the pipes open.
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      proc.kill();
      reject(new DripError(504, `${bin} ${args[0] ?? ""} did not finish within ${timeoutMs}ms and was killed`));
    }, timeoutMs);
  });
  const finished = Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  finished.catch(() => {});
  try {
    const [stdout, stderr, code] = await Promise.race([finished, deadline]);
    return { code, stdout, stderr };
  } finally {
    clearTimeout(timer);
  }
}

/** GET /api/bootstrap payload. */
export function bootstrap(config: DripConfig = dripConfig()): {
  cwd: string;
  home: string;
  maxContextTokens: number | null;
  dripBin: string;
} {
  return {
    cwd: config.cwd,
    home: config.home,
    maxContextTokens: config.maxContextTokens,
    dripBin: config.dripBin,
  };
}

function parseRow(raw: unknown): SessionRow {
  const row = raw as Record<string, unknown>;
  return {
    id: String(row.id ?? ""),
    createdAt: String(row.createdAt ?? ""),
    cwd: String(row.cwd ?? ""),
    dir: String(row.dir ?? ""),
    goalCount: Number(row.goalCount ?? 0),
    lastGoal: row.lastGoal === null || row.lastGoal === undefined ? null : String(row.lastGoal),
    lastRun: (row.lastRun as SessionRow["lastRun"]) ?? null,
    pid: typeof row.pid === "number" ? row.pid : undefined,
    resultPath: typeof row.resultPath === "string" && row.resultPath !== "" ? row.resultPath : undefined,
    running: Boolean(row.running),
    statePath: String(row.statePath ?? ""),
    status: String(row.status ?? ""),
    transcriptPath: String(row.transcriptPath ?? ""),
    updatedAt: String(row.updatedAt ?? ""),
  };
}

const byUpdatedAtDesc = (a: SessionRow, b: SessionRow): number =>
  b.updatedAt.localeCompare(a.updatedAt);

/**
 * GET /api/sessions: recursive listing split into running/recent, each sorted
 * newest-first. Failure to list is a 502 with the drip stderr, never a
 * silent empty list.
 */
/**
 * The recursive sweep opens every registry under the home, and three pollers
 * per open page each resolve their session through it. A short-lived cache
 * (DRIP_UI_LIST_CACHE_MS, default 1000; 0 disables — tests do) collapses
 * those into one sweep per tick without hiding a lease change for long.
 */
let listCache: { key: string; at: number; result: Promise<SessionLists>; settled: boolean } | null = null;

function listCacheMs(): number {
  const raw = Number(process.env.DRIP_UI_LIST_CACHE_MS ?? "1000");
  return Number.isFinite(raw) && raw > 0 ? raw : 0;
}

export async function listSessions(
  options: { bin?: string; cwd?: string; fresh?: boolean } = {},
): Promise<SessionLists> {
  const config = dripConfig();
  const ttl = listCacheMs();
  const key = `${options.bin ?? config.dripBin}\0${options.cwd ?? config.cwd}`;
  const now = Date.now();
  // A sweep still in flight is shared even past the TTL: a slow sweep must
  // not make every poll in the meantime spawn another one.
  if (!options.fresh && ttl > 0 && listCache && listCache.key === key && (!listCache.settled || now - listCache.at < ttl)) {
    return listCache.result;
  }
  const result = listSessionsUncached(config, options);
  if (ttl > 0) {
    const entry = { key, at: now, result, settled: false };
    listCache = entry;
    result.then(
      () => {
        entry.settled = true;
      },
      () => {
        // A failed sweep must not be served from the cache.
        entry.settled = true;
        if (listCache === entry) listCache = null;
      },
    );
  }
  return result;
}

async function listSessionsUncached(config: DripConfig, options: { bin?: string; cwd?: string }): Promise<SessionLists> {
  const out = await runDrip(["--list", "--json", "--recursive"], {
    cwd: options.cwd ?? config.cwd,
    bin: options.bin,
  });
  if (out.code !== 0) {
    throw new DripError(502, out.stderr.trim() || `drip --list exited with code ${out.code}`);
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(out.stdout);
  } catch {
    throw new DripError(502, "drip --list produced unparseable JSON");
  }
  if (!Array.isArray(parsed)) {
    throw new DripError(502, "drip --list output was not a JSON array");
  }
  const rows = parsed.map(parseRow).sort(byUpdatedAtDesc);
  return {
    running: rows.filter((row) => row.running),
    recent: rows.filter((row) => !row.running),
  };
}

/**
 * Resolve a session id against the listing; 404 when unknown. `fresh` skips
 * the cache, for decisions that hinge on liveness (see messageSession).
 */
export async function findSession(
  id: string,
  options: { bin?: string; cwd?: string; fresh?: boolean } = {},
): Promise<SessionRow> {
  const pick = (lists: SessionLists) =>
    lists.running.find((candidate) => candidate.id === id) ?? lists.recent.find((candidate) => candidate.id === id);
  const cached = listCache;
  let row = pick(await listSessions(options));
  if (!row && cached !== null && listCache === cached) {
    // The miss came from a cached sweep: a session started a moment ago
    // (POST /api/run) is not in it yet, so one fresh sweep decides before a
    // 404 is trusted. A miss from a sweep that just ran is final.
    listCache = null;
    row = pick(await listSessions(options));
  }
  if (!row) throw new DripError(404, `unknown session ${id}`);
  return row;
}

export interface SessionState {
  state: Record<string, unknown> | null;
  isRunning: boolean;
}

/** GET /api/sessions/:id/state — state.json contents plus liveness. */
export async function sessionState(
  id: string,
  options: { bin?: string; cwd?: string } = {},
): Promise<SessionState> {
  const row = await findSession(id, options);
  let state: Record<string, unknown> | null = null;
  const file = Bun.file(row.statePath);
  if (await file.exists()) {
    try {
      state = (await file.json()) as Record<string, unknown>;
    } catch {
      state = null;
    }
  }
  return { state, isRunning: row.running };
}

export interface JsonlPage {
  entries: Record<string, unknown>[];
  /** Byte offset to pass back on the next poll. */
  nextOffset: number;
  /** The file shrank below the caller's offset (truncated or rewritten): `entries` replay from byte 0 and replace what was shown. */
  reset: boolean;
}

/**
 * Read newline-delimited JSON from `path` starting at byte `offset`.
 * Only complete lines (terminated by \n) are consumed: a trailing partial
 * line stays unread so the next poll, after the writer finishes it, returns
 * it exactly once. `nextOffset` is the byte offset to pass back. A file
 * shorter than `offset` was truncated or rewritten (a run resumed into the
 * same session, a rotated log): the read restarts from byte 0 with
 * `reset: true` so the caller replaces its tail instead of freezing on a
 * cursor past the end.
 */
export async function readJsonlFrom(path: string, offset: number): Promise<JsonlPage> {
  const file = Bun.file(path);
  if (!(await file.exists())) return { entries: [], nextOffset: offset, reset: false };
  const reset = file.size < offset;
  const from = reset ? 0 : offset;
  // Only the unread suffix is loaded: transcripts grow for hours and this
  // runs every poll tick for every open page.
  if (file.size <= from) return { entries: [], nextOffset: from, reset };
  const bytes = new Uint8Array(await file.slice(from).arrayBuffer());
  let end = bytes.length;
  // Trim to the last complete line: everything after the final \n is a
  // partial write and must not be consumed or parsed.
  while (end > 0 && bytes[end - 1] !== 0x0a) end -= 1;
  if (end === 0) return { entries: [], nextOffset: from, reset };
  const text = new TextDecoder().decode(bytes.subarray(0, end));
  const entries: Record<string, unknown>[] = [];
  for (const line of text.split("\n")) {
    if (line.trim() === "") continue;
    try {
      entries.push(JSON.parse(line) as Record<string, unknown>);
    } catch {
      // A corrupted line would desync the client; skip it but keep counting
      // the bytes so the stream stays byte-accurate.
    }
  }
  return { entries, nextOffset: from + end, reset };
}

/** GET /api/sessions/:id/transcript?offset=N */
export async function sessionTranscript(
  id: string,
  offset: number,
  options: { bin?: string; cwd?: string } = {},
): Promise<JsonlPage & { isRunning: boolean }> {
  const row = await findSession(id, options);
  const safeOffset = Number.isFinite(offset) && offset >= 0 ? Math.floor(offset) : 0;
  const page = await readJsonlFrom(row.transcriptPath, safeOffset);
  return { ...page, isRunning: row.running };
}

/** GET /api/sessions/:id/result — result.json, or null when absent. */
export async function sessionResult(
  id: string,
  options: { bin?: string; cwd?: string } = {},
): Promise<Record<string, unknown> | null> {
  const row = await findSession(id, options);
  // Prefer the path drip reports; older CLIs (no resultPath on the row)
  // keep the record beside state.json.
  const resultPath = row.resultPath ?? row.statePath.replace(/state\.json$/, "result.json");
  const file = Bun.file(resultPath);
  if (!(await file.exists())) return null;
  try {
    return (await file.json()) as Record<string, unknown>;
  } catch {
    return null;
  }
}

/** Last non-empty stdout line — where --detach puts its JSON handle. */
export function lastNonEmptyLine(stdout: string): string {
  const lines = stdout.split("\n").filter((line) => line.trim() !== "");
  return lines.length > 0 ? (lines[lines.length - 1] ?? "") : "";
}

export function parseDetachHandle(stdout: string): { sessionId: string; handle: unknown } {
  const line = lastNonEmptyLine(stdout);
  let parsed: Record<string, unknown> | null = null;
  try {
    parsed = JSON.parse(line) as Record<string, unknown>;
  } catch {
    parsed = null;
  }
  if (!parsed || typeof parsed.sessionId !== "string") {
    throw new DripError(502, `drip detach did not print a JSON handle: ${line.slice(0, 200)}`);
  }
  return { sessionId: parsed.sessionId, handle: parsed };
}

/**
 * POST /api/run — spawn an ordinary detached drip run from DRIP_CWD and
 * return the handle drip printed. Empty goal is a client error.
 */
export async function startRun(
  body: { goal?: unknown; maxIterations?: unknown },
  options: { bin?: string; cwd?: string } = {},
): Promise<{ sessionId: string; handle: unknown }> {
  const goal = typeof body.goal === "string" ? body.goal.trim() : "";
  if (goal === "") throw new DripError(400, "goal must be a non-empty string");
  const args = ["--json", "--detach"];
  if (body.maxIterations !== undefined && body.maxIterations !== null) {
    const iterations = body.maxIterations;
    if (typeof iterations !== "number" || !Number.isInteger(iterations) || iterations <= 0) {
      throw new DripError(400, "maxIterations must be a positive integer");
    }
    args.push("--max-iterations", String(iterations));
  }
  args.push(goal);
  const config = dripConfig();
  const out = await runDrip(args, { cwd: config.cwd, bin: options.bin });
  if (out.code !== 0) {
    throw new DripError(500, out.stderr.trim() || `drip exited with code ${out.code}`);
  }
  return parseDetachHandle(out.stdout);
}

function requireText(body: { text?: unknown }, what: string): string {
  const text = typeof body.text === "string" ? body.text : "";
  if (text.trim() === "") throw new DripError(400, `${what} must be a non-empty string`);
  return text;
}

function actionFailure(out: RunOutcome, fallback: string): DripError {
  return new DripError(502, out.stderr.trim() || fallback);
}

/**
 * POST /api/sessions/:id/send — operator input into a running session.
 * The id is validated against a fresh listing first, so unknown ids 404
 * instead of reaching drip with an arbitrary string.
 */
export async function sendToSession(
  id: string,
  body: { text?: unknown },
  options: { bin?: string; cwd?: string } = {},
): Promise<{ ok: boolean; runActive: boolean; output: string }> {
  return sendToRow(await findSession(id, options), requireText(body, "text"), options);
}

async function sendToRow(
  row: SessionRow,
  text: string,
  options: { bin?: string; cwd?: string },
): Promise<{ ok: boolean; runActive: boolean; output: string }> {
  const out = await runDrip(["--json", "--send", row.id, text], { cwd: row.cwd, bin: options.bin });
  if (out.code !== 0) throw actionFailure(out, `drip --send exited with code ${out.code}`);
  let sent: Record<string, unknown>;
  try {
    sent = JSON.parse(lastNonEmptyLine(out.stdout)) as Record<string, unknown>;
  } catch {
    throw new DripError(502, `drip --send printed no JSON result: ${out.stdout.trim() || "(empty)"}`);
  }
  if (typeof sent.runActive !== "boolean") {
    throw new DripError(502, `drip --send result lacks runActive: ${out.stdout.trim()}`);
  }
  return { ok: true, runActive: sent.runActive, output: out.stdout.trim() };
}

/**
 * POST /api/sessions/:id/message — the composer's single entry point. The
 * text always goes through `drip --send`, which queues it in the session
 * inbox and reports `runActive` from the lease at that instant; that verdict
 * — not the listing the browser or this server last swept — decides what
 * happens next. A live run picks the message up at its next cycle boundary
 * (mode "send"). Otherwise a plain `--resume` starts the next run, which
 * consumes the inbox (mode "resume"); `--resume` itself refuses a session
 * whose run went live in between, so drip stays the final arbiter.
 */
export async function messageSession(
  id: string,
  body: { text?: unknown },
  options: { bin?: string; cwd?: string } = {},
): Promise<{ ok: boolean; mode: "send" | "resume" | "queued"; handle?: unknown; detail?: string }> {
  const text = requireText(body, "text");
  const row = await findSession(id, options);
  const sent = await sendToRow(row, text, options);
  if (sent.runActive) return { ok: true, mode: "send" };
  try {
    const resumed = await resumeRow(row, {}, options);
    return { ok: true, mode: "resume", handle: resumed.handle };
  } catch (error) {
    // The text is already queued in the inbox. When `--resume` was refused
    // because a run went live in between, that run consumes the message and
    // this is not a failure — confirmed against a fresh lease reading. Any
    // other resume failure is reported as such.
    const fresh = await findSession(id, { ...options, fresh: true });
    if (!fresh.running) throw error;
    const detail = error instanceof Error ? error.message : String(error);
    return { ok: true, mode: "queued", detail };
  }
}

/** POST /api/sessions/:id/stop */
export async function stopSession(
  id: string,
  options: { bin?: string; cwd?: string } = {},
): Promise<{ ok: boolean; output: string }> {
  const row = await findSession(id, options);
  const out = await runDrip(["--stop", row.id], { cwd: row.cwd, bin: options.bin });
  if (out.code !== 0) throw actionFailure(out, `drip --stop exited with code ${out.code}`);
  return { ok: true, output: out.stdout.trim() };
}

/**
 * POST /api/sessions/:id/resume — detach-resume, optionally with a prompt
 * (the idle-session composer path).
 */
export async function resumeSession(
  id: string,
  body: { prompt?: unknown } = {},
  options: { bin?: string; cwd?: string } = {},
): Promise<{ ok: boolean; handle: unknown }> {
  return resumeRow(await findSession(id, options), body, options);
}

async function resumeRow(
  row: SessionRow,
  body: { prompt?: unknown },
  options: { bin?: string; cwd?: string },
): Promise<{ ok: boolean; handle: unknown }> {
  const args = ["--resume", row.id, "--json", "--detach"];
  if (body.prompt !== undefined && body.prompt !== null) {
    const prompt = typeof body.prompt === "string" ? body.prompt.trim() : "";
    if (prompt === "") throw new DripError(400, "prompt must be a non-empty string");
    args.push("--prompt", prompt);
  }
  const out = await runDrip(args, { cwd: row.cwd, bin: options.bin });
  if (out.code !== 0) throw actionFailure(out, `drip --resume exited with code ${out.code}`);
  return { ok: true, handle: parseDetachHandle(out.stdout).handle };
}
