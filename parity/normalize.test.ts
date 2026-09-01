import { describe, expect, it } from "vitest";

import {
  normalizeArtifact,
  normalizeSqliteRow,
  normalizeText,
  sortMockRequests
} from "./normalize";

const UUID_A = "3f2a9c1e-5b7d-4c8e-9f60-1a2b3c4d5e6f";
const UUID_B = "00000000-1111-4222-8333-444455556666";

describe("normalizeText — identities and rename", () => {
  it("masks UUIDs", () => {
    expect(normalizeText(`session ${UUID_A} done`)).toBe("session <UUID> done");
  });

  it("masks ISO timestamps", () => {
    expect(normalizeText("at 2026-09-01T01:27:15.095Z end")).toBe("at <TS> end");
  });

  it("masks bare version numbers", () => {
    expect(normalizeText("drip 0.91.0")).toBe("drip <VERSION>");
    expect(normalizeText('{"version":"0.91.0"}')).toBe('{"version":"<VERSION>"}');
  });

  it("renames the product word-bounded", () => {
    expect(normalizeText("lci --help; lciw; LCI_HOME; LCI_PROJECT_DIR; LCI_QUEUE_DIR")).toBe(
      "drip --help; dripw; DRIP_HOME; DRIP_PROJECT_DIR; DRIP_QUEUE_DIR"
    );
  });

  it("leaves words that merely contain lci", () => {
    expect(normalizeText("malice alice")).toBe("malice alice");
  });

  it("renames home paths", () => {
    expect(normalizeText("~/.lci .lci/patches.jsonl")).toBe("~/.drip .drip/patches.jsonl");
  });

  it("masks machine temp paths", () => {
    expect(normalizeText("wrote /var/folders/ab/cd/T/x and /tmp/y")).toBe(
      "wrote <TMPDIR> and <TMP>"
    );
  });

  it("masks loopback ports and duration suffixes but not plain integers", () => {
    expect(normalizeText("http://127.0.0.1:45678/v1 took 12ms in 3 tries")).toBe(
      "http://<LOOPBACK>:<PORT>/v1 took <MS> in 3 tries"
    );
    // A bare loopback address must survive as one token, not "<VERSION>.1".
    expect(normalizeText("dial 127.0.0.1 refused")).toBe("dial <LOOPBACK> refused");
  });
});

describe("normalizeText — path placeholders", () => {
  it("anchors both roots", () => {
    const out = normalizeText(
      "home /t/h/projects/p/sessions; project /t/pr/fixture/hello.txt",
      { home: "/t/h", project: "/t/pr" }
    );

    expect(out).toBe("home <HOME>/projects/p/sessions; project <PROJECT>/fixture/hello.txt");
  });

  it("does not rewrite sibling prefixes", () => {
    expect(normalizeText("/t/home2/x", { home: "/t/h" })).toBe("/t/home2/x");
  });

  it("masks embedded project slugs", () => {
    const out = normalizeText(
      "/t/lci-home/projects/-Users-me-parity-run-respond-lci-project/sessions/abc",
      { home: "/t/lci-home" }
    );

    expect(out).toBe("<HOME>/projects/<SLUG>/sessions/abc");
  });
});

describe("normalizeArtifact — session-json", () => {
  it("masks known timestamp/duration keys and leaves other numbers", () => {
    const out = normalizeArtifact(
      JSON.stringify({ createdAt: 1725150000000, latencyMs: 1234, id: 7, count: 12 }),
      "session-json"
    );

    expect(JSON.parse(out)).toEqual({ createdAt: "<TS>", count: 12, id: 7, latencyMs: "<MS>" });
  });

  it("compares keys case-insensitively by sorting", () => {
    const out = normalizeArtifact('{"z":1,"a":{"b":1,"a":2}}', "session-json");

    expect(JSON.parse(out)).toEqual({ a: { a: 2, b: 1 }, z: 1 });
  });

  it("masks unknown-* keys", () => {
    const out = normalizeArtifact(
      '{"model":"m","unknown_key":"x","unknownField":"y"}',
      "session-json"
    );

    expect(JSON.parse(out)).toEqual({
      model: "m",
      unknownField: "<UNKNOWN>",
      unknown_key: "<UNKNOWN>"
    });
  });

  it("collapses recorded errors to their shape", () => {
    const out = normalizeArtifact('{"error":{"message":"boom 1","type":"x"}}', "session-json");

    expect(JSON.parse(out)).toEqual({ error: "<ERROR>" });
  });
});

describe("normalizeArtifact — transcript", () => {
  it("re-joins pretty-printed events and canonicalizes each line", () => {
    const text = `{\n  "b": 1,\n  "a": "2026-09-01T01:27:15.095Z"\n}\n{"kind":"tool","at":1725150000000}\n`;
    const lines = normalizeArtifact(text, "transcript").split("\n");

    expect(JSON.parse(lines[0]!)).toEqual({ a: "<TS>", b: 1 });
    expect(JSON.parse(lines[1]!)).toEqual({ at: "<TS>", kind: "tool" });
  });
});

describe("normalizeArtifact — mock-request", () => {
  it("drops seq from a single request", () => {
    const out = normalizeArtifact(
      '{"seq":1,"path":"/v1/messages","body":"{\\"model\\":\\"mock\\"}"}',
      "mock-request"
    );

    expect(JSON.parse(out)).toEqual({ body: '{"model":"mock"}', path: "/v1/messages" });
  });

  it("scrubs mock-minted ids inside bodies", () => {
    const out = normalizeArtifact(
      JSON.stringify({
        seq: 3,
        path: "/v1/chat/completions",
        body: '{"tool_calls":[{"id":"call_mock-1","function":{"name":"respond"}}]}'
      }),
      "mock-request"
    );
    const parsed = JSON.parse(out) as { body: string };

    expect(JSON.parse(parsed.body)).toEqual({
      tool_calls: [{ function: { name: "respond" }, id: "<UUID>" }]
    });
  });
});

describe("normalizeSqliteRow", () => {
  it("sorts keys, masks timestamp columns and applies text rules", () => {
    const out = normalizeSqliteRow(
      {
        updated_at: "2026-09-01T01:27:15.095Z",
        created_at: "2026-09-01T01:27:15.095Z",
        id: UUID_A,
        cwd: "/t/h/projects/-slug/sessions",
        goal: "lci --help"
      },
      { home: "/t/h" }
    );

    expect(JSON.parse(out)).toEqual({
      created_at: "<TS>",
      cwd: "<HOME>/projects/-slug/sessions",
      goal: "drip --help",
      id: "<UUID>",
      updated_at: "<TS>"
    });
  });

  it("masks numeric epoch timestamps in _at columns", () => {
    const out = normalizeSqliteRow({ created_at: 1725150000000, id: UUID_B });

    expect(JSON.parse(out)).toEqual({ created_at: "<TS>", id: "<UUID>" });
  });
});

describe("sortMockRequests", () => {
  it("orders side-neutrally, drops seq and scrubs per-request ids", () => {
    const mk = (seq: number, toolId: string) =>
      JSON.stringify({
        seq,
        path: "/v1/chat/completions",
        body: `{"messages":[{"role":"tool","tool_call_id":"${toolId}"}]}`
      });

    const out = sortMockRequests([mk(1, "call_mock-9"), mk(2, "call_mock-1")]);

    expect(out).toEqual([
      JSON.stringify({
        body: '{"messages":[{"role":"tool","tool_call_id":"<UUID>"}]}',
        path: "/v1/chat/completions"
      })
    ]);
  });
});
