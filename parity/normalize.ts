/**
 * Artifact normalizer for the drip parity harness (drip/PLAN.md
 * "Parity strategy": lci is the oracle; drip's artifacts are diffed against
 * lci's after normalization hides everything machine-local).
 *
 * `normalizeText` maps a captured artifact (stdout, stderr, help text, git
 * status) onto a canonical form:
 *
 *   - UUIDs / session ids                       -> <UUID>
 *   - ISO timestamps / epoch-millis values      -> <TS> / <MS>
 *   - absolute temp paths                       -> <HOME> / <PROJECT> / <SLUG>
 *   - lci/LCI_/.lci/~/.lci                      -> drip/DRIP_/.drip/~/.drip
 *   - versions -> <VERSION>, loopback ports -> <PORT>, floats -> <NUM>
 *
 * `normalizeArtifact` normalizes structured artifacts per kind (session JSON
 * with unknown-* masking, jsonl transcripts, sqlite index rows, mock request
 * logs). The rules are unit-tested in normalize.test.ts (vitest).
 */

export type ArtifactKind =
  /** stdout/stderr, help text, usage errors, `git status`/`git diff`. */
  | "generic"
  /** session.json / state.json / result.json (parsed, key-insensitive). */
  | "session-json"
  /** one transcript event per line (pretty-printed lines are re-joined). */
  | "transcript"
  /** one sqlite index row as JSON (column order must not matter). */
  | "sqlite-row"
  /** one logged mock request {"seq","path","body"}. */
  | "mock-request";

export type NormalizeOptions = {
  /** Absolute path of the run's fake home root (each side maps to <HOME>). */
  home?: string;
  /** Absolute path of the run's temp project root (each side maps to <PROJECT>). */
  project?: string;
  /**
   * The collapsed projectSlug each side derived from its own temp path
   * (projectSlug embeds the absolute cwd, so the two sides spell it with
   * different temp segments; each maps to the same <SLUG>).
   */
  slug?: string;
  /** Mock-side: the session id lci used (lets an id-keyed log become side-neutral). */
  lciSessionId?: string;
};

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

type ReplaceRule = {
  pattern: RegExp;
  replacement: string;
};

// Keys that carry timestamps / durations in recorded JSON. Value-level rules
// key off these names first, so `id: 7` and `count: 12` survive untouched.
const TIMESTAMP_KEYS = new Set([
  "createdAt",
  "created_at",
  "updatedAt",
  "updated_at",
  "startedAt",
  "started_at",
  "finishedAt",
  "finished_at",
  "sentAt",
  "sent_at",
  "at",
  "timestamp",
  "ts",
  "time",
  "isoTime",
  "lastGoalAt",
  "lastGoal_at"
]);

const DURATION_KEYS = new Set([
  "durationMs",
  "duration_ms",
  "latencyMs",
  "latency_ms",
  "elapsedMs",
  "elapsed_ms",
  "wallMs",
  "wall_ms",
  "waitMs",
  "wait_ms",
  "ageMs",
  "age_ms",
  "synthesisMs",
  "totalMs",
  "unitsMs"
]);

const UUID_PATTERN = /[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}/g;

export function normalizeText(text: string, options: NormalizeOptions = {}): string {
  return applyRules(text, genericRules(options));
}

function applyRules(text: string, rules: ReplaceRule[]): string {
  let out = text;

  for (const rule of rules) {
    out = out.replace(rule.pattern, rule.replacement);
  }

  return out;
}

// --- rule construction ---------------------------------------------------------

function genericRules(options: NormalizeOptions): ReplaceRule[] {
  const rules: ReplaceRule[] = [];

  // Side-specific anchors first (longest / most specific wins).
  if (options.slug !== undefined) {
    rules.push(pathRule(options.slug, "<SLUG>"));
  }

  if (options.project !== undefined) {
    rules.push(pathRule(options.project, "<PROJECT>"));
  }

  if (options.home !== undefined) {
    rules.push(pathRule(options.home, "<HOME>"));
  }

  rules.push(...identityRules());
  return rules;
}

// Path replacement needs word-ish boundaries: never rewrite a path that merely
// shares a prefix with the temp root (both roots are siblings under one
// mkdtemp parent, so a missing separator would otherwise corrupt siblings).
function pathRule(root: string, token: string): ReplaceRule {
  const escaped = root.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

  return {
    pattern: new RegExp(`${escaped}(?=/|"|'|\\s|$|\\\\)`, "g"),
    replacement: token
  };
}

// Rules that need no side anchors. Order matters: identities before the
// rename (an id never contains "lci"), rename before versions ("lci 0.91.0"
// becomes "drip <VERSION>"), numeric catch-alls last.
function identityRules(): ReplaceRule[] {
  return [
    // --- identities ---------------------------------------------------------
    { pattern: UUID_PATTERN, replacement: "<UUID>" },

    // ISO timestamps before the version rule (which would otherwise eat the
    // "15.095" out of "01:27:15.095Z" and leave mangled text behind).
    {
      pattern: /\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?/g,
      replacement: "<TS>"
    },

    // --- textual product rename (word-bounded; PLAN.md 67-70) ---------------
    { pattern: /(?<![A-Za-z])LCI_PROJECT_DIR\b/g, replacement: "DRIP_PROJECT_DIR" },
    { pattern: /\bLCI_HOME\b/g, replacement: "DRIP_HOME" },
    { pattern: /\bLCI_TMUX_PREFIX\b/g, replacement: "DRIP_TMUX_PREFIX" },
    { pattern: /(?<![A-Za-z])LCI_(?=[A-Z])/g, replacement: "DRIP_" },
    { pattern: /(?<![A-Za-z])lciw\b/g, replacement: "dripw" },
    { pattern: /(?<![A-Za-z])lci(?![A-Za-z0-9])/g, replacement: "drip" },
    { pattern: /\.lci\b/g, replacement: ".drip" },

    // --- documented drip-only surfaces (drip/README.md known deviations) ----
    // The `--migrate-from-lci` OPTIONS block has no lci counterpart: strip
    // it (synopsis line through the last continuation line before
    // `--version`) so `--help` compares byte-for-byte otherwise.
    { pattern: /\t--migrate-from-(?:lci|drip)[^\n]*\n(?:\t {30}[^\n]*\n)*/g, replacement: "" },
    // drip embeds the seven built-in skills; lci reads them from <repo>/skills.
    { pattern: /"path": "[^"\n]*\/skills\/([A-Za-z0-9._-]+\/SKILL\.md)"/g, replacement: '"path": "<builtin>/$1"' },

    // --- machine-local addresses --------------------------------------------
    // Loopback first: the version rule below would otherwise eat "127.0.0"
    // out of the IP and leave a corrupted "<VERSION>.1:<PORT>" behind.
    { pattern: /(127\.0\.0\.1|localhost|\[::1\]):\d{1,5}\b/g, replacement: "<LOOPBACK>:<PORT>" },
    { pattern: /127\.0\.0\.1/g, replacement: "<LOOPBACK>" },
    { pattern: /:(\d{2,5})\/v1\b/g, replacement: ":<PORT>/v1" },
    { pattern: /\/private\/var\/folders\/[^\s"'`,:;)\\]+/g, replacement: "<TMPDIR>" },
    { pattern: /\/var\/folders\/[^\s"'`,:;)\\]+/g, replacement: "<TMPDIR>" },
    { pattern: /\/tmp\/[^\s"'`,:;)\\]+/g, replacement: "<TMP>" },
    // Only slugs that look like a collapsed multi-segment absolute path
    // (leading dash plus internal dashes, e.g. "-Users-me-repo"); short
    // literal slugs such as "-slug" in a doc example must survive.
    {
      pattern: /\/projects\/-(?:[A-Za-z0-9]+-)+[A-Za-z0-9]+\/sessions\b/g,
      replacement: "/projects/<SLUG>/sessions"
    },

    // --- versions -----------------------------------------------------------
    { pattern: /\bv?\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?\b/g, replacement: "<VERSION>" },

    // --- nondeterministic leftovers (best effort; runs are deterministic) ---
    { pattern: /\b\d+(?:\.\d+)?(?:ms|µs)\b/g, replacement: "<MS>" },
    // Duration fields inside JSON lines printed to stdout (--json NDJSON events).
    {
      pattern: /"(durationMs|duration_ms|latencyMs|latency_ms|elapsedMs|elapsed_ms|wallMs|wall_ms|waitMs|wait_ms|ageMs|age_ms|synthesisMs|totalMs|unitsMs)":\s*\d+(?:\.\d+)?/g,
      replacement: '"$1":<MS>'
    },
    // --review progress on stderr rounds wall-clock to seconds ("in 4s — 13s total").
    { pattern: /\b(in|elapsed|total|done in) (\d+)s\b/g, replacement: "$1 <S>s" },
    { pattern: /— (\d+)s total\b/g, replacement: "— <S>s total" },
    { pattern: /\((\d+)s elapsed\)/g, replacement: "(<S>s elapsed)" },
    // Session-id prefixes (resumeIdPrefix, `--resume 91c1c835`): 8 lowercase hex.
    { pattern: /\b[0-9a-f]{8}\b/g, replacement: "<ID8>" },
    { pattern: /\b\d+\.\d+(?:e[+-]?\d+)?\b/gi, replacement: "<NUM>" }
  ];
}

// --- structured artifacts ---------------------------------------------------------

export function normalizeArtifact(
  text: string,
  kind: ArtifactKind,
  options: NormalizeOptions = {}
): string {
  if (kind === "sqlite-row") {
    return normalizeSqliteRowText(text, options);
  }

  if (kind === "transcript" || kind === "mock-request") {
    return normalizeJsonLines(text, kind, options);
  }

  return normalizeJsonArtifact(text, kind, options);
}

/** Parses jsonl OR pretty-printed multi-line JSON documents. */
function parseJsonValues(text: string): Json[] {
  const values: Json[] = [];
  let buffer = "";

  for (const line of text.split("\n")) {
    if (line.trim().length === 0) {
      continue;
    }

    buffer = buffer.length === 0 ? line : `${buffer}\n${line}`;

    try {
      values.push(JSON.parse(buffer) as Json);
      buffer = "";
    } catch {
      // Keep accumulating — pretty-printed JSON spans lines.
    }
  }

  return values;
}

function normalizeJsonLines(text: string, kind: ArtifactKind, options: NormalizeOptions): string {
  let values: Json[];

  try {
    values = parseJsonValues(text);
  } catch {
    return normalizeText(text, options);
  }

  if (kind === "mock-request") {
    return values.map((value) => normalizeMockRequestValue(value, options)).join("\n");
  }

  const rules = genericRules(options);
  const normalized = values.map((value) => normalizeJson(value, kind, options, rules));

  return normalized.map((value) => JSON.stringify(value)).join("\n");
}

function normalizeJsonArtifact(text: string, kind: ArtifactKind, options: NormalizeOptions): string {
  let value: Json;

  try {
    value = JSON.parse(text) as Json;
  } catch {
    return normalizeText(text, options);
  }

  const rules = genericRules(options);

  return JSON.stringify(normalizeJson(value, kind, options, rules), null, 2);
}

function normalizeJson(
  value: Json,
  kind: ArtifactKind,
  options: NormalizeOptions,
  rules: ReplaceRule[]
): Json {
  if (typeof value === "string") {
    return applyRules(value, rules);
  }

  if (Array.isArray(value)) {
    return value.map((entry) => normalizeJson(entry, kind, options, rules));
  }

  if (value !== null && typeof value === "object") {
    const out: { [key: string]: Json } = {};
    let hasError = false;

    for (const key of Object.keys(value).sort()) {
      const raw = value[key]!;

      // Unknown fields are implementation-private; the two sides may record
      // different sets, so they compare as one placeholder.
      if (kind === "session-json" && /^unknown/i.test(key)) {
        out[key] = "<UNKNOWN>";
        continue;
      }

      if (typeof raw === "number" && TIMESTAMP_KEYS.has(key)) {
        out[key] = "<TS>";
        continue;
      }

      if (typeof raw === "number" && DURATION_KEYS.has(key)) {
        out[key] = "<MS>";
        continue;
      }

      if (key === "error") {
        hasError = true;
      }

      out[key] = normalizeJson(raw, kind, options, rules);
    }

    // Recorded error payloads are compared by shape, not by dialect wording:
    // whatever sits under "error" (and a bare sibling "message") collapses.
    if (hasError) {
      out.error = "<ERROR>";

      if (out.message !== undefined) {
        out.message = "<ERROR>";
      }
    }

    return out;
  }

  if (typeof value === "number" && !Number.isInteger(value)) {
    return "<NUM>";
  }

  return value;
}

// --- sqlite session index -----------------------------------------------------

export type SqliteRow = Record<string, string | number | null>;

/**
 * Normalizes one session-index row (from `sqlite3 -json` or bun:sqlite) into
 * a canonical string so two sides agree regardless of column order or type
 * affinities.
 */
export function normalizeSqliteRow(row: SqliteRow, options: NormalizeOptions = {}): string {
  const normalized: { [key: string]: Json } = {};

  for (const key of Object.keys(row).sort()) {
    const raw = row[key];
    const text = typeof raw === "string" ? raw : String(raw);
    normalized[key] = isTimestampColumnName(key) ? "<TS>" : applyRules(text, genericRules(options));
  }

  return JSON.stringify(normalized);
}

function isTimestampColumnName(key: string): boolean {
  return key.endsWith("_at");
}

function normalizeSqliteRowText(text: string, options: NormalizeOptions): string {
  let parsed: unknown;

  try {
    parsed = JSON.parse(text);
  } catch {
    return normalizeText(text, options);
  }

  if (Array.isArray(parsed)) {
    return parsed.map((row) => normalizeSqliteRow(row as SqliteRow, options)).join("\n");
  }

  return normalizeSqliteRow(parsed as SqliteRow, options);
}

// --- mock request log ----------------------------------------------------------

export type MockRequest = { seq: number; path: string; body: string };

/**
 * Sums can differ across sides (retry ladders), so request logs are compared
 * as multisets: normalized, seq stripped, sorted. Model-side ids (tool_call
 * ids, completion ids) are scrubbed — they are per-request counters.
 */
export function sortMockRequests(
  requests: Array<MockRequest | string>,
  options: NormalizeOptions = {}
): string[] {
  const normalized = requests.map((request) =>
    typeof request === "string"
      ? normalizeMockRequestLine(request, options)
      : normalizeMockRequestValue(request, options)
  );

  // After id-scrubbing, requests that differ only by mock-minted ids collapse
  // to one canonical line.
  return [...new Set(normalized)].sort(compareStrings);
}

function compareStrings(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function normalizeMockRequestLine(line: string, options: NormalizeOptions): string {
  let parsed: unknown;

  try {
    parsed = JSON.parse(line);
  } catch {
    return normalizeText(line, options);
  }

  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return normalizeText(line, options);
  }

  return normalizeMockRequestValue(parsed as Json, options);
}

/** One logged request {seq,path,body} -> canonical {body,path} (seq dropped). */
function normalizeMockRequestValue(value: Json, options: NormalizeOptions): string {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return normalizeText(JSON.stringify(value) ?? "", options);
  }

  const record = value as { [key: string]: Json };
  const rawBody = record.body;
  const body = typeof rawBody === "string" ? rawBody : JSON.stringify(rawBody ?? null);
  const path = typeof record.path === "string" ? record.path : "";

  return JSON.stringify({ body: normalizeMockBody(body, options), path });
}

// Models never see the fake home or project, but request bodies may embed the
// cwd in prompts, so the full rule set still applies.
function normalizeMockBody(body: string, options: NormalizeOptions = {}): string {
  let out = applyRules(body, genericRules(options));

  // Per-request ids the mock server mints (tool_call ids, completion ids) are
  // echoed back in follow-up requests; key-scoped so model names and semantic
  // string values survive.
  out = out.replace(/("(?:id|tool_call_id|call_id)"\s*:\s*)"[^"]*"/g, '$1"<UUID>"');

  return out;
}

// --- request-log dedupe (identical retries collapse) -----------------------------

export function dedupeMockRequests(requests: string[]): string[] {
  return Array.from(new Set(requests));
}
