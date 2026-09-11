// Caddy hub: every `drip --ui` instance registers a path-prefixed route with
// the local Caddy (via its admin API) so all of them share one stable
// address — http://<hub>/<label>/ per project, and http://<hub>/ as a hub
// page listing every live instance. Nothing here is required: when Caddy is
// not running the instance is simply reachable on its own port.
//
// The Caddy objects are created on demand under a drip-owned server named
// `drip_ui` and tagged with `@id`s (drip-ui-<label>, drip-ui-hub) so they can
// be replaced or removed without touching the rest of Caddy's config
// (Caddyfile-managed sites included). The whole server is removed when the
// last instance leaves.
import { basename } from "node:path";
import type { InstanceRecord } from "./instances";

export const SERVER_NAME = "drip_ui";
export const HUB_ROUTE_ID = "drip-ui-hub";
export const ROUTE_ID_PREFIX = "drip-ui-";
export const DEFAULT_ADMIN = "http://127.0.0.1:2019";
export const DEFAULT_HUB = "drip.localhost:4140";

export interface HubConfig {
  /** Caddy admin endpoint, or null when the hub is disabled (`DRIP_CADDY_ADMIN=off`). */
  admin: string | null;
  /** host[:port] the hub listens on, as typed into a browser. */
  hub: string;
}

export function hubConfig(env: Record<string, string | undefined> = process.env): HubConfig {
  const admin = (env.DRIP_CADDY_ADMIN ?? DEFAULT_ADMIN).trim();
  // Accept a pasted URL too: the scheme and any path are not part of a host.
  const hub = (env.DRIP_UI_HUB ?? DEFAULT_HUB).trim().replace(/^https?:\/\//i, "").replace(/\/.*$/, "");
  return {
    admin: admin === "" || admin.toLowerCase() === "off" ? null : admin.replace(/\/+$/, ""),
    hub: hub === "" ? DEFAULT_HUB : hub,
  };
}

/** `drip.localhost:4140` → { host: "drip.localhost", port: 4140 } (port defaults to 80). */
export function splitHub(hub: string): { host: string; port: number } {
  const match = /^(.*?)(?::(\d+))?$/.exec(hub);
  const host = match?.[1] ?? hub;
  const port = match?.[2] !== undefined ? Number.parseInt(match[2], 10) : 80;
  return { host, port: Number.isFinite(port) && port > 0 ? port : 80 };
}

export function hubUrl(hub: string, label?: string): string {
  return `http://${hub}/${label === undefined ? "" : `${label}/`}`;
}

/**
 * Path label for a project: its directory name plus a hash of the full
 * path, so two checkouts named alike get distinct, stable prefixes. The
 * label keys both the Caddy route and the instance registry, so the hash
 * is wide enough (40 bits) that a collision is not a practical concern.
 */
export function instanceLabel(cwd: string): string {
  const name = basename(cwd).replace(/[^A-Za-z0-9._-]+/g, "-").replace(/^-+|-+$/g, "") || "project";
  const digest = new Bun.CryptoHasher("sha1").update(cwd).digest("hex").slice(0, 10);
  return `${name}-${digest}`;
}

export function routeId(label: string): string {
  return `${ROUTE_ID_PREFIX}${label}`;
}

/** The Caddy route for one instance: /label → /label/, /label/* → 127.0.0.1:port with the prefix stripped. */
export function instanceRoute(label: string, port: number, hubHost: string): Record<string, unknown> {
  return {
    "@id": routeId(label),
    match: [{ host: [hubHost], path: [`/${label}`, `/${label}/*`] }],
    handle: [
      {
        handler: "subroute",
        routes: [
          {
            match: [{ path: [`/${label}`] }],
            handle: [{ handler: "static_response", status_code: 308, headers: { Location: [`/${label}/`] } }],
          },
          {
            handle: [
              { handler: "rewrite", strip_path_prefix: `/${label}` },
              { handler: "reverse_proxy", upstreams: [{ dial: `127.0.0.1:${port}` }] },
            ],
          },
        ],
      },
    ],
    terminal: true,
  };
}

/**
 * The hub route: `/` on the hub host is served by whichever live instance
 * answers first (every instance renders the same /hub page from the shared
 * registry), so the hub survives any one instance leaving.
 */
export function hubRoute(hubHost: string, ports: number[]): Record<string, unknown> {
  return {
    "@id": HUB_ROUTE_ID,
    match: [{ host: [hubHost], path: ["/"] }],
    handle: [
      { handler: "rewrite", uri: "/hub" },
      {
        handler: "reverse_proxy",
        upstreams: ports.map((port) => ({ dial: `127.0.0.1:${port}` })),
        load_balancing: { selection_policy: { policy: "first" } },
        health_checks: { passive: { fail_duration: "3s", max_fails: 1 } },
      },
    ],
    terminal: true,
  };
}

export function serverConfig(hubPort: number): Record<string, unknown> {
  return { listen: [`127.0.0.1:${hubPort}`], automatic_https: { disable: true }, routes: [] };
}

export class CaddyError extends Error {}

/** Minimal client for Caddy's admin API; every failure is a CaddyError. */
export class CaddyAdmin {
  constructor(
    private readonly base: string,
    private readonly timeoutMs = 2000,
  ) {}

  private async request(method: string, path: string, body?: unknown): Promise<{ status: number; text: string }> {
    let response: Response;
    try {
      response = await fetch(`${this.base}${path}`, {
        method,
        headers: body === undefined ? {} : { "content-type": "application/json" },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: AbortSignal.timeout(this.timeoutMs),
      });
    } catch (error) {
      throw new CaddyError(`caddy admin ${this.base} unreachable: ${(error as Error).message}`);
    }
    return { status: response.status, text: await response.text() };
  }

  /** GET a config path; Caddy answers `null` for a missing key. */
  async get(path: string): Promise<unknown> {
    const { status, text } = await this.request("GET", path);
    if (status === 404) return null;
    if (status >= 300) throw new CaddyError(`GET ${path}: ${status} ${text.trim()}`);
    return text.trim() === "" ? null : JSON.parse(text);
  }

  async exists(id: string): Promise<boolean> {
    return (await this.get(`/id/${id}`)) !== null;
  }

  private async mutate(method: string, path: string, body?: unknown): Promise<void> {
    const { status, text } = await this.request(method, path, body);
    if (status >= 300) throw new CaddyError(`${method} ${path}: ${status} ${text.trim()}`);
  }

  put(path: string, body: unknown): Promise<void> {
    return this.mutate("PUT", path, body);
  }

  post(path: string, body: unknown): Promise<void> {
    return this.mutate("POST", path, body);
  }

  patch(path: string, body: unknown): Promise<void> {
    return this.mutate("PATCH", path, body);
  }

  async delete(path: string): Promise<void> {
    const { status, text } = await this.request("DELETE", path);
    if (status !== 404 && status >= 300) throw new CaddyError(`DELETE ${path}: ${status} ${text.trim()}`);
  }
}

const SERVER_PATH = `/config/apps/http/servers/${SERVER_NAME}`;

/** Create or replace a route by its @id (PATCH when present, else append). */
async function upsertRoute(admin: CaddyAdmin, route: Record<string, unknown>): Promise<void> {
  const id = String(route["@id"]);
  if (await admin.exists(id)) await admin.patch(`/id/${id}`, route);
  else await admin.post(`${SERVER_PATH}/routes`, route);
}

/** Drop routes for instances that are no longer alive; returns the labels removed. */
async function pruneRoutes(admin: CaddyAdmin, liveLabels: Set<string>): Promise<string[]> {
  const routes = (await admin.get(`${SERVER_PATH}/routes`)) as Array<Record<string, unknown>> | null;
  const removed: string[] = [];
  for (const route of routes ?? []) {
    const id = typeof route["@id"] === "string" ? route["@id"] : "";
    if (!id.startsWith(ROUTE_ID_PREFIX) || id === HUB_ROUTE_ID) continue;
    const label = id.slice(ROUTE_ID_PREFIX.length);
    if (liveLabels.has(label)) continue;
    await admin.delete(`/id/${id}`);
    removed.push(label);
  }
  return removed;
}

/**
 * Register `self` and reconcile the hub with the set of live instances
 * (which must include `self`). Idempotent: safe to call again after a Caddy
 * restart wiped the config.
 */
export async function registerInstance(admin: CaddyAdmin, hub: string, self: InstanceRecord, live: InstanceRecord[]): Promise<string> {
  const { host, port } = splitHub(hub);
  if ((await admin.get(SERVER_PATH)) === null) {
    try {
      await admin.put(SERVER_PATH, serverConfig(port));
    } catch (error) {
      // 409: a peer created it between our GET and PUT — the registry lock
      // makes that rare, but Caddy is shared with anything else on the box.
      if (!(error instanceof CaddyError) || !error.message.includes(": 409 ")) throw error;
    }
  }
  await upsertRoute(admin, instanceRoute(self.label, self.port, host));
  await pruneRoutes(admin, new Set(live.map((instance) => instance.label)));
  await upsertRoute(admin, hubRoute(host, live.map((instance) => instance.port)));
  return hubUrl(hub, self.label);
}

/** Remove `self`; `remaining` are the instances still alive after it leaves. */
export async function deregisterInstance(admin: CaddyAdmin, hub: string, self: InstanceRecord, remaining: InstanceRecord[]): Promise<void> {
  if ((await admin.get(SERVER_PATH)) === null) return;
  await admin.delete(`/id/${routeId(self.label)}`);
  if (remaining.length === 0) {
    await admin.delete(SERVER_PATH);
    return;
  }
  await upsertRoute(admin, hubRoute(splitHub(hub).host, remaining.map((instance) => instance.port)));
}
