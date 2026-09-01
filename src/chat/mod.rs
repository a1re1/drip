// port of src/chat — the shared chat-contract leaf.
//
// src/chat/types.ts is imported by tools/, harness/ and cli/ alike; the
// model-call layer (src/chat/anthropic.ts, src/chat/transport.ts) is instead
// ported under harness/ per PLAN.md.
pub mod types;
