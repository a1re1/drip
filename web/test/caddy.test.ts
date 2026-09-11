// Caddy hub registration against a fake admin API that keeps an in-memory
// config with just enough of Caddy's semantics: /config/<path> get/put/post
// (post appends to arrays), /id/<id> lookup, patch and delete, 404 on unknown
// ids, and `null` for a missing config key.
import { afterAll, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import {
  CaddyAdmin,
  CaddyError,
  DEFAULT_ADMIN,
  DEFAULT_HUB,
  HUB_ROUTE_ID,
  deregisterInstance,
  hubConfig,
  hubRoute,
  hubUrl,
  instanceLabel,
  instanceRoute,
  registerInstance,
  routeId,
  splitHub,
} from "../lib/caddy";
import type { InstanceRecord } from "../lib/instances";

type Json = Record<string, unknown>;

interface FakeCaddy {
  server: ReturnType<typeof Bun.serve>;
  admin: CaddyAdmin;
  config: () => Json;
  reset: () => void;
  log: string[];
  /** Next PUT of the drip_ui server answers 409 after creating it, like a lost race. */
  conflictNextServerPut: boolean;
}

function fakeCaddy(): FakeCaddy {
  let config: Json = { apps: { http: { servers: {} } } };
  const log: string[] = [];

  const walk = (path: string): { parent: Json | unknown[]; key: string } | null => {
    const parts = path.split("/").filter((part) => part !== "");
    let node: unknown = config;
    for (let i = 0; i < parts.length - 1; i += 1) {
      const part = parts[i] ?? "";
      if (Array.isArray(node)) node = node[Number(part)];
      else if (node !== null && typeof node === "object") node = (node as Json)[part];
      else return null;
      if (node === undefined) return null;
    }
    if (node === null || typeof node !== "object") return null;
    return { parent: node as Json | unknown[], key: parts[parts.length - 1] ?? "" };
  };

  const findById = (id: string): { list: unknown[]; index: number } | null => {
    const routes = ((config.apps as Json)?.http as Json)?.servers as Json;
    for (const server of Object.values(routes ?? {})) {
      const list = (server as Json).routes as unknown[] | undefined;
      const index = (list ?? []).findIndex((route) => (route as Json)["@id"] === id);
      if (list && index >= 0) return { list, index };
    }
    return null;
  };

  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      const url = new URL(request.url);
      log.push(`${request.method} ${url.pathname}`);
      const body = request.method === "GET" || request.method === "DELETE" ? undefined : ((await request.json()) as unknown);
      if (url.pathname.startsWith("/id/")) {
        const id = url.pathname.slice("/id/".length);
        const hit = findById(id);
        if (!hit) return Response.json({ error: `unknown object ID '${id}'` }, { status: 404 });
        if (request.method === "GET") return Response.json(hit.list[hit.index]);
        if (request.method === "PATCH") {
          hit.list[hit.index] = body;
          return new Response("");
        }
        if (request.method === "DELETE") {
          hit.list.splice(hit.index, 1);
          return new Response("");
        }
        return new Response("bad method", { status: 405 });
      }
      if (url.pathname === "/config" || url.pathname.startsWith("/config/")) {
        const target = walk(url.pathname.slice("/config".length));
        if (request.method === "GET") {
          if (!target) return new Response("null");
          const value = Array.isArray(target.parent) ? target.parent[Number(target.key)] : target.parent[target.key];
          return new Response(JSON.stringify(value ?? null));
        }
        if (!target) return Response.json({ error: `invalid traversal path at: ${url.pathname}` }, { status: 500 });
        const current = Array.isArray(target.parent) ? target.parent[Number(target.key)] : target.parent[target.key];
        if (request.method === "PUT") {
          (target.parent as Json)[target.key] = body;
          if (fake.conflictNextServerPut && target.key === "drip_ui") {
            fake.conflictNextServerPut = false;
            return Response.json({ error: `[${url.pathname}] key already exists: drip_ui` }, { status: 409 });
          }
          return new Response("");
        }
        if (request.method === "POST") {
          if (Array.isArray(current)) current.push(body);
          else (target.parent as Json)[target.key] = body;
          return new Response("");
        }
        if (request.method === "DELETE") {
          if (current === undefined) return Response.json({ error: "key does not exist" }, { status: 404 });
          delete (target.parent as Json)[target.key];
          return new Response("");
        }
      }
      return new Response("nope", { status: 404 });
    },
  });
  const fake: FakeCaddy = {
    server,
    admin: new CaddyAdmin(`http://127.0.0.1:${server.port}`),
    config: () => config,
    reset: () => {
      config = { apps: { http: { servers: {} } } };
      log.length = 0;
      fake.conflictNextServerPut = false;
    },
    log,
    conflictNextServerPut: false,
  };
  return fake;
}

function instance(label: string, port: number): InstanceRecord {
  return { label, cwd: `/work/${label}`, port, pid: process.pid, startedAt: "2026-09-10T10:00:00.000Z", version: "0.103.0" };
}

let caddy: FakeCaddy;

beforeAll(() => {
  caddy = fakeCaddy();
});

afterAll(() => {
  caddy.server.stop(true);
});

beforeEach(() => {
  caddy.reset();
});

const servers = () => ((caddy.config().apps as Json).http as Json).servers as Json;
const routes = () => ((servers().drip_ui as Json | undefined)?.routes ?? []) as Json[];
const ids = () => routes().map((route) => route["@id"]);
const hubUpstreams = () => {
  const hub = routes().find((route) => route["@id"] === HUB_ROUTE_ID);
  const proxy = ((hub?.handle as Json[]) ?? []).find((handler) => handler.handler === "reverse_proxy");
  return ((proxy?.upstreams as Json[]) ?? []).map((upstream) => upstream.dial);
};

describe("pure pieces", () => {
  test("hubConfig defaults, trimming and the off switch", () => {
    expect(hubConfig({})).toEqual({ admin: DEFAULT_ADMIN, hub: DEFAULT_HUB });
    expect(hubConfig({ DRIP_CADDY_ADMIN: "off", DRIP_UI_HUB: "" })).toEqual({ admin: null, hub: DEFAULT_HUB });
    expect(hubConfig({ DRIP_CADDY_ADMIN: "off", DRIP_UI_HUB: "http://ui.localhost:8080/" }).hub).toBe("ui.localhost:8080");
    expect(hubConfig({ DRIP_CADDY_ADMIN: "http://127.0.0.1:2020/", DRIP_UI_HUB: "ui.localhost:8080" })).toEqual({
      admin: "http://127.0.0.1:2020",
      hub: "ui.localhost:8080",
    });
  });

  test("splitHub and hubUrl", () => {
    expect(splitHub("drip.localhost:4140")).toEqual({ host: "drip.localhost", port: 4140 });
    expect(splitHub("drip.localhost")).toEqual({ host: "drip.localhost", port: 80 });
    expect(hubUrl("drip.localhost:4140")).toBe("http://drip.localhost:4140/");
    expect(hubUrl("drip.localhost:4140", "drip-ab12cd")).toBe("http://drip.localhost:4140/drip-ab12cd/");
  });

  test("instanceLabel is the directory name plus a stable path hash", () => {
    const label = instanceLabel("/Users/me/src/my repo (v2)");
    expect(label).toMatch(/^my-repo-v2-[0-9a-f]{10}$/);
    expect(instanceLabel("/Users/me/src/my repo (v2)")).toBe(label);
    expect(instanceLabel("/elsewhere/my repo (v2)")).not.toBe(label);
    expect(instanceLabel("/")).toMatch(/^project-[0-9a-f]{10}$/);
  });

  test("instanceRoute redirects the bare prefix and strips it for the proxy", () => {
    const route = instanceRoute("app-123456", 4142, "drip.localhost");
    expect(route["@id"]).toBe(routeId("app-123456"));
    expect(route.match).toEqual([{ host: ["drip.localhost"], path: ["/app-123456", "/app-123456/*"] }]);
    const sub = ((route.handle as Json[])[0] as Json).routes as Json[];
    expect(((sub[0] as Json).handle as Json[])[0]).toMatchObject({ handler: "static_response", status_code: 308 });
    expect((sub[1] as Json).handle).toEqual([
      { handler: "rewrite", strip_path_prefix: "/app-123456" },
      { handler: "reverse_proxy", upstreams: [{ dial: "127.0.0.1:4142" }] },
    ]);
  });

  test("hubRoute rewrites / to /hub across every live port", () => {
    const route = hubRoute("drip.localhost", [4141, 4142]);
    expect(route.match).toEqual([{ host: ["drip.localhost"], path: ["/"] }]);
    expect((route.handle as Json[])[0]).toEqual({ handler: "rewrite", uri: "/hub" });
    expect(((route.handle as Json[])[1] as Json).upstreams).toEqual([{ dial: "127.0.0.1:4141" }, { dial: "127.0.0.1:4142" }]);
  });
});

describe("registration against the admin API", () => {
  test("first instance creates the server, its route and the hub", async () => {
    const self = instance("one-000001", 4141);
    const url = await registerInstance(caddy.admin, "drip.localhost:4140", self, [self]);
    expect(url).toBe("http://drip.localhost:4140/one-000001/");
    expect((servers().drip_ui as Json).listen).toEqual(["127.0.0.1:4140"]);
    expect((servers().drip_ui as Json).automatic_https).toEqual({ disable: true });
    expect(ids()).toEqual([routeId("one-000001"), HUB_ROUTE_ID]);
    expect(hubUpstreams()).toEqual(["127.0.0.1:4141"]);
  });

  test("a second instance joins the existing server and the hub balances over both", async () => {
    const one = instance("one-000001", 4141);
    const two = instance("two-000002", 4142);
    await registerInstance(caddy.admin, "drip.localhost:4140", one, [one]);
    await registerInstance(caddy.admin, "drip.localhost:4140", two, [one, two]);
    expect(ids()).toEqual([routeId("one-000001"), HUB_ROUTE_ID, routeId("two-000002")]);
    expect(hubUpstreams()).toEqual(["127.0.0.1:4141", "127.0.0.1:4142"]);
    // Only one server was ever created.
    expect(caddy.log.filter((line) => line === "PUT /config/apps/http/servers/drip_ui")).toHaveLength(1);
  });

  test("re-registering replaces the route in place (new port) and prunes dead instances", async () => {
    const one = instance("one-000001", 4141);
    const stale = instance("stale-00000f", 4150);
    await registerInstance(caddy.admin, "drip.localhost:4140", one, [one, stale]);
    await registerInstance(caddy.admin, "drip.localhost:4140", stale, [one, stale]);
    expect(ids()).toContain(routeId("stale-00000f"));
    // `stale` died without cleaning up; `one` restarts on another port.
    const moved = { ...one, port: 4143 };
    await registerInstance(caddy.admin, "drip.localhost:4140", moved, [moved]);
    expect(ids()).toEqual([routeId("one-000001"), HUB_ROUTE_ID]);
    expect(hubUpstreams()).toEqual(["127.0.0.1:4143"]);
    const own = routes().find((route) => route["@id"] === routeId("one-000001"));
    expect(JSON.stringify(own)).toContain("127.0.0.1:4143");
  });

  test("leaving removes the route; the last one out removes the server", async () => {
    const one = instance("one-000001", 4141);
    const two = instance("two-000002", 4142);
    await registerInstance(caddy.admin, "drip.localhost:4140", one, [one]);
    await registerInstance(caddy.admin, "drip.localhost:4140", two, [one, two]);
    await deregisterInstance(caddy.admin, "drip.localhost:4140", one, [two]);
    expect(ids()).toEqual([HUB_ROUTE_ID, routeId("two-000002")]);
    expect(hubUpstreams()).toEqual(["127.0.0.1:4142"]);
    await deregisterInstance(caddy.admin, "drip.localhost:4140", two, []);
    expect(servers().drip_ui).toBeUndefined();
    // Leaving when nothing is registered (Caddy restarted) is a no-op.
    await deregisterInstance(caddy.admin, "drip.localhost:4140", two, []);
  });

  test("a server created by a peer between GET and PUT (409) is not an error", async () => {
    const one = instance("one-000001", 4141);
    caddy.conflictNextServerPut = true;
    await registerInstance(caddy.admin, "drip.localhost:4140", one, [one]);
    expect(ids()).toEqual([routeId("one-000001"), HUB_ROUTE_ID]);
  });

  test("exists() answers by @id", async () => {
    const one = instance("one-000001", 4141);
    expect(await caddy.admin.exists(routeId("one-000001"))).toBe(false);
    await registerInstance(caddy.admin, "drip.localhost:4140", one, [one]);
    expect(await caddy.admin.exists(routeId("one-000001"))).toBe(true);
  });

  test("an unreachable admin is a CaddyError, not a crash", async () => {
    const dead = new CaddyAdmin("http://127.0.0.1:1", 500);
    let caught: unknown = null;
    try {
      await registerInstance(dead, "drip.localhost:4140", instance("x-000000", 4141), []);
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(CaddyError);
    expect((caught as Error).message).toContain("unreachable");
  });
});
