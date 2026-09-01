/**
 * Mock OpenAI/Anthropic model server for the drip parity harness
 * (drip/PLAN.md "Parity strategy": lci is the oracle; both sides under test
 * talk to this canned model so no real network endpoint is ever hit).
 *
 * Usage:
 *   bun run drip/parity/mock-model.ts --responses <file> --log <file> [--port N]
 *
 * - `--responses` is a JSONL file; each line is one canned model reply:
 *     {"content"?: string, "toolCalls"?: [{"name": string, "arguments": object}],
 *      "when"?: {"contains": string}, "status"?: number, "error"?: object}
 *   Replies are served in order. A `when`-gated line is only consumed when the
 *   raw request body contains the trigger string; unmatched gated lines stay
 *   queued and the next unconditional line is served instead. Once every line
 *   is consumed the server answers {"content":"(mock exhausted)"}.
 * - Every request is appended to `--log` as one JSON line {"seq","path","body"}.
 * - `--port 0` (the default) picks a random port; the server prints {"port":N}
 *   as its first stdout line once it is listening.
 *
 * Endpoints:
 *   POST /v1/chat/completions  OpenAI-compatible, non-streaming JSON. If the
 *                              request sets stream:true the same content is
 *                              delivered as chat.completion.chunk SSE events.
 *   POST /v1/messages          Anthropic Messages shape (content blocks,
 *                              tool_use, stop_reason). stream:true yields the
 *                              equivalent Anthropic SSE event stream.
 *
 * Tool-call replies are shaped for lci's parsers:
 *   - OpenAI: choices[0].message.tool_calls[] = {id, type:"function",
 *     function:{name, arguments: <JSON string>}} with finish_reason "tool_calls"
 *     (src/harness/model-call.ts / src/chat/transport.ts).
 *   - Anthropic: content blocks {type:"tool_use", id, name, input:<object>}
 *     with stop_reason "tool_use" (src/chat/anthropic.ts).
 */

import { appendFileSync, mkdirSync, readFileSync } from "node:fs";
import { dirname } from "node:path";

type MockToolCall = {
  name: string;
  arguments: Record<string, unknown>;
};

type MockReply = {
  content?: string;
  toolCalls?: MockToolCall[];
  when?: { contains: string };
  status?: number;
  error?: Record<string, unknown>;
};

function fail(message: string): never {
  console.error(`mock-model: ${message}`);
  process.exit(1);
}

function parseArgs(argv: string[]): { responsesPath: string; logPath: string; port: number } {
  let responsesPath = "";
  let logPath = "";
  let port = 0;

  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index]!;
    const value = argv[index + 1];
    if (arg === "--responses") {
      responsesPath = value ?? "";
      index++;
    } else if (arg === "--log") {
      logPath = value ?? "";
      index++;
    } else if (arg === "--port") {
      const parsed = Number(value);
      if (value === undefined || value === "" || !Number.isInteger(parsed) || parsed < 0) {
        fail(`--port expects a non-negative integer, got "${value ?? ""}"`);
      }
      port = parsed;
      index++;
    } else {
      fail(
        `unknown argument "${arg}" (usage: mock-model.ts --responses <file> --log <file> [--port N])`
      );
    }
  }

  if (responsesPath.length === 0) fail("missing required --responses <file>");
  if (logPath.length === 0) fail("missing required --log <file>");
  return { responsesPath, logPath, port };
}

function loadReplies(path: string): MockReply[] {
  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch (error) {
    fail(`cannot read responses file ${path}: ${String(error)}`);
  }

  const replies: MockReply[] = [];
  for (const [lineIndex, raw] of text.split("\n").entries()) {
    const line = raw.trim();
    if (line.length === 0) {
      continue;
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(line);
    } catch (error) {
      fail(`${path}:${lineIndex + 1} is not valid JSON: ${String(error)}`);
    }
    if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
      fail(`${path}:${lineIndex + 1} must be a JSON object`);
    }
    replies.push(parsed as MockReply);
  }
  return replies;
}

// --- reply selection ---------------------------------------------------------

const replies = loadReplies(parseArgs(process.argv.slice(2)).responsesPath);
const remaining: MockReply[] = [...replies];

/**
 * Serve replies in file order. A `when`-gated line is only consumed when the
 * request body contains its trigger string; when it does not match, scanning
 * continues to the next unconditional line and the gated line stays queued for
 * later requests. Returns the exhaustion reply once nothing usable is left.
 */
function pickReply(requestText: string): MockReply {
  const index = remaining.findIndex((reply) => {
    const gate = reply.when?.contains;
    return gate === undefined || requestText.includes(gate);
  });
  if (index === -1) {
    return { content: "(mock exhausted)" };
  }
  return remaining.splice(index, 1)[0]!;
}

// --- response shaping --------------------------------------------------------

let callCounter = 0;

function openAiToolCalls(reply: MockReply): unknown[] {
  return (reply.toolCalls ?? []).map((call) => {
    callCounter++;
    return {
      id: `call_mock-${callCounter}`,
      type: "function",
      function: { name: call.name, arguments: JSON.stringify(call.arguments ?? {}) }
    };
  });
}

function openAiPayload(reply: MockReply, model: string): Record<string, unknown> {
  const toolCalls = openAiToolCalls(reply);
  const message: Record<string, unknown> = { role: "assistant" };
  // Mirror OpenAI: tool-call turns carry no content key unless one was scripted.
  if (reply.content !== undefined) {
    message.content = reply.content;
  }
  if (toolCalls.length > 0) {
    message.tool_calls = toolCalls;
  }
  return {
    id: `chatcmpl-mock-${callCounter}`,
    object: "chat.completion",
    created: 0,
    model,
    choices: [
      {
        index: 0,
        message,
        finish_reason: toolCalls.length > 0 ? "tool_calls" : "stop",
        logprobs: null
      }
    ],
    usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 }
  };
}

function anthropicBlocks(reply: MockReply): unknown[] {
  const blocks: unknown[] = [];
  const text = reply.content ?? "";
  const hasToolCalls = (reply.toolCalls?.length ?? 0) > 0;
  // Plain replies always carry a text block (even an empty one); tool-only
  // replies are blocks of just tool_use.
  if (text.length > 0 || !hasToolCalls) {
    blocks.push({ type: "text", text });
  }
  for (const call of reply.toolCalls ?? []) {
    callCounter++;
    blocks.push({
      type: "tool_use",
      id: `toolu_mock-${callCounter}`,
      name: call.name,
      input: call.arguments ?? {}
    });
  }
  return blocks;
}

function anthropicPayload(reply: MockReply, model: string): Record<string, unknown> {
  const hasToolCalls = (reply.toolCalls?.length ?? 0) > 0;
  return {
    id: `msg_mock-${callCounter + 1}`,
    type: "message",
    role: "assistant",
    model,
    content: anthropicBlocks(reply),
    stop_reason: hasToolCalls ? "tool_use" : "end_turn",
    stop_sequence: null,
    usage: { input_tokens: 1, output_tokens: 1 }
  };
}

// --- SSE variants (same content, streamed) -----------------------------------

function sseEvent(payload: unknown): Uint8Array {
  return new TextEncoder().encode(`data: ${JSON.stringify(payload)}\n\n`);
}

function openAiStream(reply: MockReply, model: string): ReadableStream<Uint8Array> {
  const toolCalls = openAiToolCalls(reply);
  const base = {
    id: `chatcmpl-mock-${callCounter}`,
    object: "chat.completion.chunk",
    created: 0,
    model
  };
  const chunks: unknown[] = [
    { ...base, choices: [{ index: 0, delta: { role: "assistant", content: "" }, finish_reason: null }] }
  ];
  if (reply.content !== undefined && reply.content.length > 0) {
    chunks.push({
      ...base,
      choices: [{ index: 0, delta: { content: reply.content }, finish_reason: null }]
    });
  }
  (toolCalls as Array<Record<string, unknown>>).forEach((call, index) => {
    chunks.push({
      ...base,
      choices: [
        {
          index: 0,
          delta: { tool_calls: [{ index, ...call }] },
          finish_reason: null
        }
      ]
    });
  });
  chunks.push({
    ...base,
    choices: [
      {
        index: 0,
        delta: {},
        finish_reason: toolCalls.length > 0 ? "tool_calls" : "stop",
        logprobs: null
      }
    ]
  });

  return new ReadableStream({
    start(controller) {
      for (const chunk of chunks) {
        controller.enqueue(sseEvent(chunk));
      }
      controller.enqueue(new TextEncoder().encode("data: [DONE]\n\n"));
      controller.close();
    }
  });
}

function anthropicStream(reply: MockReply, model: string): ReadableStream<Uint8Array> {
  const blocks = anthropicBlocks(reply) as Array<Record<string, unknown>>;
  const stopReason = blocks.some((block) => block.type === "tool_use") ? "tool_use" : "end_turn";
  const events: unknown[] = [
    {
      type: "message_start",
      message: {
        id: `msg_mock-${callCounter + 1}`,
        type: "message",
        role: "assistant",
        model,
        content: [],
        stop_reason: null,
        stop_sequence: null,
        usage: { input_tokens: 1, output_tokens: 0 }
      }
    }
  ];
  blocks.forEach((block, index) => {
    events.push({ type: "content_block_start", index, content_block: block });
    if (block.type === "text") {
      events.push({
        type: "content_block_delta",
        index,
        delta: { type: "text_delta", text: block.text ?? "" }
      });
    } else if (block.type === "tool_use") {
      events.push({
        type: "content_block_delta",
        index,
        delta: { type: "input_json_delta", partial_json: JSON.stringify(block.input ?? {}) }
      });
    }
    events.push({ type: "content_block_stop", index });
  });
  events.push({
    type: "message_delta",
    delta: { stop_reason: stopReason, stop_sequence: null },
    usage: { output_tokens: 1 }
  });
  events.push({ type: "message_stop" });

  return new ReadableStream({
    start(controller) {
      for (const event of events) {
        controller.enqueue(sseEvent(event));
      }
      controller.close();
    }
  });
}

// --- server ------------------------------------------------------------------

const args = parseArgs(process.argv.slice(2));
mkdirSync(dirname(args.logPath), { recursive: true });

let nextSeq = 0;

function appendLog(entry: { seq: number; path: string; body: unknown }): void {
  appendFileSync(args.logPath, `${JSON.stringify(entry)}\n`);
}

const server = Bun.serve({
  port: args.port,
  async fetch(req): Promise<Response> {
    const url = new URL(req.url);
    const path = url.pathname;

    // Log every request, even ones that will 404.
    const raw = await req.text();
    let body: unknown = raw;
    try {
      body = JSON.parse(raw);
    } catch {
      // Non-JSON body: log the raw text so the parity diff still sees it.
    }
    appendLog({ seq: nextSeq, path, body });
    nextSeq++;

    const isChat = req.method === "POST" && path === "/v1/chat/completions";
    const isMessages = req.method === "POST" && path === "/v1/messages";
    if (!isChat && !isMessages) {
      return Response.json(
        { error: { message: `mock-model: no route for ${req.method} ${path}`, type: "mock_no_route" } },
        { status: 404 }
      );
    }

    const reply = pickReply(raw);
    const parsedBody = typeof body === "object" && body !== null ? (body as Record<string, unknown>) : {};
    const model = typeof parsedBody.model === "string" ? parsedBody.model : "mock-model";
    const stream = parsedBody.stream === true;
    const status = reply.status ?? (reply.error !== undefined ? 500 : 200);

    if (reply.error !== undefined || status !== 200) {
      const error = reply.error ?? { message: `mock-model status ${status}`, type: "mock_error" };
      const payload = isMessages ? { type: "error", error } : { error };
      return Response.json(payload, { status });
    }

    if (stream) {
      return new Response(isMessages ? anthropicStream(reply, model) : openAiStream(reply, model), {
        status: 200,
        headers: { "content-type": "text/event-stream" }
      });
    }

    return Response.json(isMessages ? anthropicPayload(reply, model) : openAiPayload(reply, model), {
      status: 200
    });
  }
});

process.on("SIGINT", () => {
  server.stop(true);
  process.exit(0);
});
process.on("SIGTERM", () => {
  server.stop(true);
  process.exit(0);
});

// First stdout line: the port the harness should point both configs at.
console.log(JSON.stringify({ port: server.port }));
