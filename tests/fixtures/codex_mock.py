#!/usr/bin/env python3
"""Deterministic fake `codex app-server` for drip's CodexBridge integration tests.

Speaks JSON-RPC 2.0 over stdio JSONL exactly like the real Codex app-server, but
never touches the network or any model. A scenario is chosen with --script.

Behavior shared by every scenario:
- Replies to `initialize` (id 0) immediately.
- Logs every client frame as a JSON line to <cwd>/codex_mock_log.jsonl so the
  Rust tests can assert on what drip actually sent (ids, params, order).
- Ignores unknown notifications (the real server emits unsolicited ones).
- Never reads/writes anything outside its cwd.

Scenarios:
  happy         initialize, account chatgpt, thread, turn -> agentMessage text.
  echo_turn     like happy, but the final agentMessage text echoes the model,
                reasoning effort and cwd drip requested (assertion vehicle).
  tool_roundtrip one item/tool/call server request (id 71, callId "c1", tool
                "shell"), then completes the turn after drip answers it.
  tool_two      two tool calls in sequence: "c1" then "c2" (each answered by a
                separate drip call), then completes.
  auth_apikey   account/read reports API-key billing (must fail before
                thread/start).
  no_handshake  never answers `initialize` (initialize timeout path).
  malformed     replies with a garbage line instead of a JSON-RPC response.
  turn_failed   turn/completed with status "failed" and a TurnError.
  replay_check  completes every turn; increments a turn counter so tests can
                assert how many threads/turns were started.
"""

import argparse
import json
import os
import sys
import threading

REQUEST_IDS = {
    "initialize": 0,
    "account/read": 1,
    "thread/start": 2,
    "turn/start": 3,
}

ACCOUNT_CHATGPT = {"account": {"type": "chatgpt", "planType": "pro"}}
ACCOUNT_APIKEY = {"account": {"type": "apiKey"}}


class Mock:
    def __init__(self, script, cwd):
        self.script = script
        self.cwd = cwd
        self.tool_call_count = 0
        self.turn_count = 0
        self.next_thread_number = 0
        self.next_request_id = 100
        self.observed_answers = []
        self.lock = threading.Lock()

    # -- I/O ----------------------------------------------------------------

    def log(self, frame):
        with self.lock:
            with open(os.path.join(self.cwd, "codex_mock_log.jsonl"), "a") as f:
                f.write(json.dumps(frame, sort_keys=True) + "\n")

    def send(self, payload):
        sys.stdout.write(json.dumps(payload) + "\n")
        sys.stdout.flush()

    def result(self, request_id, result):
        self.send({"jsonrpc": "2.0", "id": request_id, "result": result})

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def server_request(self, method, request_id, params):
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method,
                   "params": params})

    # -- Handshake ----------------------------------------------------------

    def handle_initialize(self, request_id):
        self.result(request_id, {
            "userAgent": "codex-mock/0.0.0",
            "codexHome": self.cwd,
            "platformFamily": "unix",
            "platformOs": "test",
        })

    def handle_account_read(self, request_id):
        if self.script == "auth_null":
            # Exact-match validation: a null account type must be rejected.
            account = {"account": {"type": None}}
        elif self.script == "auth_apikey":
            account = ACCOUNT_APIKEY
        else:
            account = ACCOUNT_CHATGPT
        self.result(request_id, account)

    def handle_thread_start(self, request_id, params):
        self.log({"sent": "thread/start", "params": params})
        self.turn_count = 0
        self.next_thread_number += 1
        self.thread_id = "th mock thread id %d" % self.next_thread_number
        self.result(request_id, {"thread": {
            "id": self.thread_id,
            "model": params.get("model"),
        }})
        self.notify("thread/started", {"thread_id": self.thread_id})

    def handle_turn_start(self, request_id, params):
        self.log({"sent": "turn/start", "params": params})
        self.turn_count += 1
        turn_id = "turn-%d" % self.turn_count
        self.result(request_id, {"turn": {"id": turn_id, "status": "inProgress"}})
        threading.Thread(target=self.drive_turn, args=(params, turn_id),
                         daemon=True).start()

    # -- Turn scripting -----------------------------------------------------

    def usage(self):
        return {
            "last": {"inputTokens": 10, "cachedInputTokens": 2,
                     "outputTokens": 5, "totalTokens": 15},
            "total": {"inputTokens": 10, "cachedInputTokens": 2,
                      "outputTokens": 5, "totalTokens": 15},
        }

    def complete_turn(self, thread_id, turn_id, text, status="completed"):
        self.notify("thread/tokenUsage/updated", {
            "threadId": thread_id,
            "tokenUsage": self.usage(),
        })
        self.notify("turn/completed", {
            "threadId": thread_id,
            "turn": {"id": turn_id, "status": status,
                     "items": [{"type": "agentMessage",
                                "content": [{"type": "text", "text": text}]}]},
        })

    def complete_turn_failed(self, thread_id, turn_id):
        self.notify("thread/tokenUsage/updated", {
            "threadId": thread_id,
            "tokenUsage": self.usage(),
        })
        self.notify("turn/completed", {
            "threadId": thread_id,
            "turn": {"id": turn_id, "status": "failed",
                     "error": {"code": "internalServerError",
                               "message": "mock turn exploded"}},
        })

    def drive_turn(self, params, turn_id):
        thread_id = params["threadId"]
        if self.script == "hang_turn":
            # The turn never completes: the client must abort/interrupt it.
            return
        if self.script == "turn_failed":
            self.complete_turn_failed(thread_id, turn_id)
            return
        if self.script in ("tool_roundtrip", "tool_two"):
            self.tool_call_count += 1
            call_id = "c1" if self.tool_call_count == 1 else "c2"
            self.pending = {"call_id": call_id, "turn_id": turn_id}
            self.server_request("item/tool/call", 70 + self.tool_call_count, {
                "threadId": thread_id,
                "turnId": turn_id,
                "callId": call_id,
                "tool": "shell",
                "arguments": json.dumps({"command": "ls"}),
            })
            return
        if self.script == "echo_turn":
            self.complete_turn(thread_id, turn_id, json.dumps({
                "model": params.get("model"),
                "effort": params.get("effort"),
                "cwd": params.get("cwd"),
            }))
            return
        self.complete_turn(thread_id, turn_id, "final answer from mock")

    def handle_tool_answer(self, request_id, params):
        self.log({"sent": "item/tool/call answer", "id": request_id,
                  "params": params})
        self.observed_answers.append({"id": request_id, "params": params})
        self.result(request_id, {"accepted": True})
        turn_id = (self.pending or {}).get("turn_id") or params.get("turnId")
        if self.script == "tool_two" and self.tool_call_count == 1:
            self.tool_call_count = 2
            # Second tool call happens inside the SAME turn, before completion.
            self.pending = {"call_id": "c2", "turn_id": turn_id}
            self.server_request("item/tool/call", 72, {
                "threadId": self.thread_id,
                "turnId": turn_id,
                "callId": "c2",
                "tool": "shell",
                "arguments": json.dumps({"command": "cat"}),
            })
            return
        self.complete_turn(self.thread_id, turn_id,
                           "tool result was %r" % params["contentItems"][0]["text"])

    # -- Dispatch -----------------------------------------------------------

    def dispatch(self, frame):
        method = frame.get("method")
        request_id = frame.get("id")
        params = frame.get("params") or {}
        if method == "initialize":
            self.handle_initialize(request_id)
        elif method == "account/read":
            self.handle_account_read(request_id)
        elif method == "thread/start":
            self.handle_thread_start(request_id, params)
        elif method == "turn/start":
            self.handle_turn_start(request_id, params)
        elif method == "turn/interrupt":
            self.log({"sent": "turn/interrupt", "params": params})
            if request_id is not None:
                self.result(request_id, {})
        elif method is None and "result" in frame:
            # Answer to our item/tool/call request; id must be echoed verbatim.
            self.handle_tool_answer(frame["id"], frame["result"])
        # Unknown notifications and anything else: ignored.


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--script", default="happy")
    parser.add_argument("app_server", nargs="?", default="app-server")
    args = parser.parse_args()

    cwd = os.getcwd()
    mock = Mock(args.script, cwd)

    if args.script == "malformed":
        # One garbage line, then exit: the reader must classify a Malformed
        # event and the bridge must surface it instead of hanging.
        sys.stdout.write("this is not json\n")
        sys.stdout.flush()
        return

    if args.script == "no_handshake":
        # Stay alive but never answer initialize; the bridge must time out.
        try:
            for line in sys.stdin:
                pass
        except KeyboardInterrupt:
            pass
        return

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            frame = json.loads(line)
        except ValueError:
            continue
        mock.log({"received": frame})
        mock.dispatch(frame)


if __name__ == "__main__":
    main()
