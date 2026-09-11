// Thin HTTP layer over web/lib/drip.ts. Every handler is one or two calls
// into the network-free bridge; drip is invoked only through runDrip there.
// Config comes exclusively from the environment (see lib/drip.ts dripConfig):
//   DRIP_BIN, DRIP_CWD, DRIP_HOME, DRIP_UI_PORT, DRIP_MAX_CONTEXT_TOKENS,
//   plus DRIP_CADDY_ADMIN / DRIP_UI_HUB for the optional Caddy hub (lib/caddy.ts).
//
// Failure contract: a failed drip invocation throws DripError with an HTTP
// status and the drip stderr as the message — it is surfaced as a non-2xx
// JSON error body, never a silent 200.
//
// The page is bundled with Bun.build on first request and served with
// relative asset paths, so the same instance works at its own root
// (http://127.0.0.1:<port>/) and behind the hub's path prefix
// (http://<hub>/<label>/) without knowing which one the browser used.
import { fileURLToPath } from "node:url";
import tailwind from "bun-plugin-tailwind";
import { CaddyAdmin, CaddyError, deregisterInstance, hubConfig, hubUrl, instanceLabel, registerInstance, routeId } from "./lib/caddy";
import { findInstance, liveInstances, removeInstance, withRegistryLock, writeInstance, type InstanceRecord } from "./lib/instances";
import {
  DripError,
  bootstrap,
  dripConfig,
  listSessions,
  messageSession,
  resumeSession,
  sendToSession,
  sessionResult,
  sessionState,
  sessionTranscript,
  startRun,
  stopSession,
} from "./lib/drip";

type RouteParams = Record<string, string>;

/** Extract `:id` segments from a route pattern against a concrete path. */
function matchRoute(pattern: string, path: string): RouteParams | null {
  const patternParts = pattern.split("/").filter((part) => part !== "");
  const pathParts = path.split("/").filter((part) => part !== "");
  if (patternParts.length !== pathParts.length) return null;
  const params: RouteParams = {};
  for (let i = 0; i < patternParts.length; i += 1) {
    const patternPart = patternParts[i] ?? "";
    const pathPart = pathParts[i] ?? "";
    if (patternPart.startsWith(":")) {
      if (pathPart === "") return null;
      params[patternPart.slice(1)] = decodeURIComponent(pathPart);
    } else if (patternPart !== pathPart) {
      return null;
    }
  }
  return params;
}

function json(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data, null, 2), {
    status,
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}

/** Read a JSON request body, tolerating an empty body. */
async function readJson(request: Request): Promise<Record<string, unknown>> {
  const raw = await request.text();
  if (raw.trim() === "") return {};
  try {
    const parsed: unknown = JSON.parse(raw);
    if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
      throw new DripError(400, "request body must be a JSON object");
    }
    return parsed as Record<string, unknown>;
  } catch (error) {
    if (error instanceof DripError) throw error;
    throw new DripError(400, `request body is not valid JSON: ${(error as Error).message}`);
  }
}

/** The one error funnel: DripError.status -> HTTP status + stderr text. */
function errorResponse(error: unknown): Response {
  if (error instanceof DripError) {
    return json({ error: error.message }, error.status);
  }
  return json({ error: (error as Error)?.message ?? "internal error" }, 500);
}

/** The port this server listens on; `DRIP_UI_PORT` from `drip --ui`. */
export function uiPort(): number {
  const port = Number(process.env.DRIP_UI_PORT ?? "4141");
  return Number.isFinite(port) && port > 0 ? port : 4141;
}

const LOOPBACK_NAMES = ["127.0.0.1", "localhost", "[::1]"];

/**
 * True when `host` (a Host or Origin authority) names this server: loopback
 * on its own port, or the Caddy hub it is registered under (Caddy forwards
 * the browser's Host unchanged).
 */
function isThisServer(host: string, port: number, hub: string): boolean {
  if (host === hub) return true;
  return LOOPBACK_NAMES.some((name) => host === `${name}:${port}`);
}

/**
 * The Host header must name this loopback server (or the Caddy hub). After
 * DNS rebinding a page on `attacker.example:<port>` reaches us with a Host
 * that agrees with its own Origin but not with 127.0.0.1; that page must
 * not read transcripts and goals any more than it may drive runs, so this
 * check covers every /api request, reads included.
 */
function isRebound(request: Request, port: number, hub: string): boolean {
  // Bun.serve always sets Host; a hand-built Request (tests) may not.
  const host = request.headers.get("host") ?? new URL(request.url).host;
  return !isThisServer(host, port, hub);
}

/**
 * Mutating routes run `drip` on the operator's behalf, so a page from
 * another origin must not be able to drive them. The server binds to
 * loopback, but any site the browser visits can still fetch loopback URLs;
 * browsers attach Origin (and Sec-Fetch-Site) to cross-site requests, so a
 * mutating request that carries either from another origin is refused.
 * curl and same-origin fetches pass.
 */
function isCrossOrigin(request: Request, port: number, hub: string): boolean {
  const site = request.headers.get("sec-fetch-site");
  if (site !== null && site !== "same-origin" && site !== "none") return true;
  const origin = request.headers.get("origin");
  if (origin === null) return false;
  try {
    return !isThisServer(new URL(origin).host, port, hub);
  } catch {
    return true;
  }
}

/** Dispatch one /api request; returns null when no route matches. */
async function handleApi(
  request: Request,
  method: string,
  path: string,
  port: number,
  hub: string,
): Promise<Response | null> {
  const apiPath = path.slice("/api".length) || "/";
  if (isRebound(request, port, hub) || (method !== "GET" && isCrossOrigin(request, port, hub))) {
    throw new DripError(403, "cross-origin request refused");
  }

  if (method === "GET" && apiPath === "/bootstrap") {
    return json(bootstrap());
  }
  if (method === "GET" && apiPath === "/sessions") {
    return json(await listSessions());
  }

  const stateMatch = matchRoute("/sessions/:id/state", apiPath);
  if (method === "GET" && stateMatch) {
    return json(await sessionState(stateMatch.id ?? ""));
  }
  const resultMatch = matchRoute("/sessions/:id/result", apiPath);
  if (method === "GET" && resultMatch) {
    return json(await sessionResult(resultMatch.id ?? ""));
  }
  const transcriptMatch = matchRoute("/sessions/:id/transcript", apiPath);
  if (method === "GET" && transcriptMatch) {
    const url = new URL(request.url);
    const raw = url.searchParams.get("offset") ?? "0";
    if (!/^\d+$/.test(raw)) throw new DripError(400, "offset must be a non-negative integer");
    return json(await sessionTranscript(transcriptMatch.id ?? "", Number(raw)));
  }

  const runMatch = matchRoute("/run", apiPath);
  if (method === "POST" && runMatch) {
    return json(await startRun(await readJson(request)));
  }
  const messageMatch = matchRoute("/sessions/:id/message", apiPath);
  if (method === "POST" && messageMatch) {
    return json(await messageSession(messageMatch.id ?? "", await readJson(request)));
  }
  const sendMatch = matchRoute("/sessions/:id/send", apiPath);
  if (method === "POST" && sendMatch) {
    return json(await sendToSession(sendMatch.id ?? "", await readJson(request)));
  }
  const stopMatch = matchRoute("/sessions/:id/stop", apiPath);
  if (method === "POST" && stopMatch) {
    return json(await stopSession(stopMatch.id ?? ""));
  }
  const resumeMatch = matchRoute("/sessions/:id/resume", apiPath);
  if (method === "POST" && resumeMatch) {
    return json(await resumeSession(resumeMatch.id ?? "", await readJson(request)));
  }

  return null;
}

// ---------------------------------------------------------------------------
// Static page: index.html plus a Bun.build bundle (main.js, main.css) held in
// memory and built on first request.

interface Asset {
  body: Blob;
  type: string;
}

let bundlePromise: Promise<Map<string, Asset>> | null = null;

function buildBundle(): Promise<Map<string, Asset>> {
  bundlePromise ??= (async () => {
    const result = await Bun.build({
      // fileURLToPath, not URL.pathname: the unpack directory lives under the
      // user's home, which may contain spaces or non-ASCII that the URL
      // form percent-encodes.
      entrypoints: [fileURLToPath(new URL("./src/main.tsx", import.meta.url))],
      plugins: [tailwind],
      target: "browser",
      naming: "[name].[ext]",
      sourcemap: "none",
    });
    if (!result.success) {
      bundlePromise = null;
      throw new Error(`bundle failed: ${result.logs.map((log) => String(log)).join("\n")}`);
    }
    const assets = new Map<string, Asset>();
    for (const output of result.outputs) {
      const name = output.path.split("/").pop() ?? "";
      const type = name.endsWith(".css") ? "text/css; charset=utf-8" : "text/javascript; charset=utf-8";
      assets.set(`/${name}`, { body: new Blob([await output.arrayBuffer()]), type });
    }
    return assets;
  })();
  return bundlePromise;
}

function escapeHtml(text: string): string {
  return text.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c] ?? c);
}

/** The hub page: every live instance from the shared registry. */
export function hubPage(instances: InstanceRecord[], hub: string): string {
  const rows = instances
    .map(
      (instance) => `<li>
  <a class="label" href="${escapeHtml(hubUrl(hub, instance.label))}">${escapeHtml(instance.label)}</a>
  <span class="cwd">${escapeHtml(instance.cwd)}</span>
  <span class="meta">direct <a href="http://127.0.0.1:${instance.port}/">127.0.0.1:${instance.port}</a> · pid ${instance.pid} · since ${escapeHtml(instance.startedAt)}</span>
</li>`,
    )
    .join("\n");
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>drip ui hub</title>
<style>
body{margin:0;padding:32px;background:#0a0a0a;color:#e5e5e5;font:14px/1.5 ui-sans-serif,system-ui,sans-serif}
h1{font-size:18px;margin:0 0 16px}ul{list-style:none;padding:0;margin:0;display:grid;gap:12px}
li{border:1px solid #262626;border-radius:8px;padding:12px 16px;display:grid;gap:4px}
.label{font-family:ui-monospace,monospace;font-size:15px;color:#7dd3fc;text-decoration:none}
.cwd{color:#a3a3a3;font-family:ui-monospace,monospace;font-size:12px}
.meta{color:#737373;font-size:12px}.meta a{color:#737373}.empty{color:#737373}
</style></head><body>
<h1>drip ui — ${instances.length} live instance${instances.length === 1 ? "" : "s"}</h1>
${instances.length === 0 ? '<p class="empty">No instance is running. Start one with <code>drip --ui</code> in a project directory.</p>' : `<ul>\n${rows}\n</ul>`}
</body></html>
`;
}

/** Build the Bun.serve handlers (separate from the listen call for tests). */
export function createServer(options: { port?: number | (() => number); hub?: string; home?: string } = {}) {
  const indexHtml = Bun.file(new URL("./index.html", import.meta.url));
  return {
    async fetch(request: Request): Promise<Response> {
      const url = new URL(request.url);
      // Resolved per request so the env is read when it is in force (tests
      // build one handler and vary the env around it).
      const port = typeof options.port === "function" ? options.port() : (options.port ?? uiPort());
      const hub = options.hub ?? hubConfig().hub;
      const home = options.home ?? dripConfig().home;
      try {
        if (url.pathname === "/api" || url.pathname.startsWith("/api/")) {
          const response = await handleApi(request, request.method, url.pathname, port, hub);
          return response ?? json({ error: `no route: ${request.method} ${url.pathname}` }, 404);
        }
        if (url.pathname === "/" || url.pathname === "") {
          return new Response(indexHtml, { headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-cache" } });
        }
        if (url.pathname === "/hub") {
          return new Response(hubPage(liveInstances(home), hub), {
            headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-cache" },
          });
        }
        const asset = (await buildBundle()).get(url.pathname);
        if (asset) {
          return new Response(asset.body, { headers: { "content-type": asset.type, "cache-control": "no-cache" } });
        }
        return json({ error: `no route: ${request.method} ${url.pathname}` }, 404);
      } catch (error) {
        return errorResponse(error);
      }
    },
  };
}

// ---------------------------------------------------------------------------
// Process entry: pick a port, listen, register with the Caddy hub, clean up.

const AUTO_PORT_FIRST = 4141;
/** How long shutdown waits for the registry lock before cleaning up without it. */
const LEAVE_LOCK_WAIT_MS = 2_000;
const AUTO_PORT_SPAN = 100;

/**
 * Listen on DRIP_UI_PORT when it is set; otherwise take the first free port
 * from 4141 upward, so concurrent instances in different projects never
 * fight over a port and a lone instance still lands on the familiar one.
 */
function listen(fetch: (request: Request) => Promise<Response>): ReturnType<typeof Bun.serve> {
  const pinned = process.env.DRIP_UI_PORT;
  const candidates =
    pinned !== undefined && pinned !== ""
      ? [uiPort()]
      : Array.from({ length: AUTO_PORT_SPAN }, (_, index) => AUTO_PORT_FIRST + index);
  let lastError: unknown = null;
  for (const port of candidates) {
    try {
      return Bun.serve({ hostname: "127.0.0.1", port, fetch });
    } catch (error) {
      lastError = error;
      if ((error as NodeJS.ErrnoException).code !== "EADDRINUSE") break;
    }
  }
  throw new Error(`could not listen on ${pinned ? `port ${pinned}` : `any port ${AUTO_PORT_FIRST}-${AUTO_PORT_FIRST + AUTO_PORT_SPAN - 1}`}: ${(lastError as Error)?.message ?? "unknown error"}`);
}

if (import.meta.main) {
  const config = dripConfig();
  const { admin, hub } = hubConfig();
  const home = config.home;
  const self: InstanceRecord = {
    label: instanceLabel(config.cwd),
    cwd: config.cwd,
    port: 0,
    pid: process.pid,
    startedAt: new Date().toISOString(),
    version: process.env.DRIP_UI_VERSION ?? "",
  };
  // The origin gate reads the port through `self`, which is known only
  // once the walk below has bound: no probe-and-rebind, the first
  // successful bind in the candidate walk is the server.
  const fetchHandler = createServer({ port: () => self.port, hub, home }).fetch;
  // Identity and port are claimed together under the registry lock: one
  // UI per project directory (a second would take over the first's hub
  // route and record — it exits pointing at the running one instead), and
  // concurrent launches walk the candidate ports one at a time.
  const claim = await withRegistryLock(home, async () => {
    const owner = findInstance(home, self.label);
    if (owner !== null && owner.pid !== process.pid) return { owner, server: null };
    const bound = listen(fetchHandler);
    self.port = bound.port ?? uiPort();
    writeInstance(home, self);
    return { owner: null, server: bound };
  });
  if (claim.server === null) {
    console.error(`drip ui: already running for ${config.cwd} at http://127.0.0.1:${claim.owner.port}/ (pid ${claim.owner.pid})`);
    process.exit(1);
  }
  const server = claim.server;
  const port = self.port;

  const caddy = admin === null ? null : new CaddyAdmin(admin);
  let registered = false;
  const register = async (): Promise<void> => {
    if (caddy === null) return;
    try {
      await withRegistryLock(home, () => registerInstance(caddy, hub, self, liveInstances(home)));
      if (!registered) console.log(`drip ui: ${hubUrl(hub, self.label)}  (hub ${hubUrl(hub)})`);
      registered = true;
    } catch (error) {
      if (!registered) {
        const reason = error instanceof CaddyError ? error.message : String(error);
        console.error(`drip ui: caddy hub disabled — ${reason}`);
      }
      registered = false;
    }
  };

  console.log(`drip ui: http://127.0.0.1:${port}/  (sessions under ${config.cwd})`);
  await register();
  // Caddy restarts (brew services restart caddy) drop the dynamic config;
  // re-register whenever our route has gone missing.
  const keepalive =
    caddy === null
      ? null
      : setInterval(() => {
          void caddy.exists(routeId(self.label)).then(
            (present) => (present ? undefined : register()),
            () => undefined,
          );
        }, 30_000);

  let leaving = false;
  const leave = async (signal: string): Promise<void> => {
    if (leaving) return;
    leaving = true;
    if (keepalive !== null) clearInterval(keepalive);
    // Shutdown is bounded (the launcher kills us after its grace): wait
    // briefly for the lock, then clean up best-effort without it.
    await withRegistryLock(home, async () => removeInstance(home, self.label, self.pid), LEAVE_LOCK_WAIT_MS).catch(() =>
      removeInstance(home, self.label, self.pid),
    );
    if (caddy !== null) {
      try {
        await withRegistryLock(home, () => deregisterInstance(caddy, hub, self, liveInstances(home)), LEAVE_LOCK_WAIT_MS);
      } catch {
        // Caddy gone or unreachable: nothing to clean up there.
      }
    }
    server.stop(true);
    process.exit(signal === "SIGINT" ? 130 : 0);
  };
  process.on("SIGINT", () => void leave("SIGINT"));
  process.on("SIGTERM", () => void leave("SIGTERM"));
}
