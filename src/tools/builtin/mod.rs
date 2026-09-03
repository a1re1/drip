// In TS each built-in tool is a defineSyncTool/defineAsyncTool object with
// prepare/execute/complete stages (src/tools/types.ts) — the per-tool
// implementations live in tools/*.ts. The Rust port splits that in two: the
// stage framework is ported separately in ../types.rs + ../execute.rs, and
// each tool module here exposes the same behavior as plain functions:
//   - definition() — the exact OpenAI function schema drip sends, i.e. the
//     {type: "function", function: {name, description, parameters}} envelope
//     built by buildTransportTools (src/chat/runtime.ts)
//   - execute(args, ctx) — the whole prepare/execute/complete pipeline for
//     one call
// ToolCtx is the slice of the runtime context built-ins receive; ToolOutcome
// is the model-facing result.

pub mod bash;
pub mod check;
pub mod dir;
pub mod fetch;
pub mod grep;
pub mod patch;
pub mod read;
pub mod verify;

use anyhow::{anyhow, Result};
use serde_json::{Map, Value};
use std::path::PathBuf;

use crate::tools::helpers::parse_tool_arguments;

/// Port of the `context` slice built-in tools touch (ChatRuntimeContext in
/// src/chat/types.ts; see createStageContext in tools/test/test-helpers.ts):
/// the cwd tools resolve relative paths against, and the network permission
/// flag services like fetch honor. Later ports add fields as tools need
/// them.
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub allow_net: bool,
}

impl Default for ToolCtx {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            allow_net: false,
        }
    }
}

/// Port of the ExecutedToolCall surface the model sees. `text` is
/// `toolContent` — the content of the tool-role message (runtime.ts pushes
/// it verbatim). Failures are prefixed with "ERROR: " exactly like
/// buildFailureResult in src/tools/execute.ts; `failed` mirrors the
/// tool-call block status "failed".
pub struct ToolOutcome {
    pub text: String,
    pub failed: bool,
}

impl ToolOutcome {
    pub fn success(text: String) -> Self {
        Self { text, failed: false }
    }

    /// Mirrors buildFailureResult: toolContent = `ERROR: ${error.message}`.
    pub fn error(error: anyhow::Error) -> Self {
        Self {
            text: format!("ERROR: {error}"),
            failed: true,
        }
    }
}

/// Bridge from the execute(args: &Value) contract above to the helpers'
/// parse_tool_arguments (parseToolArguments in src/tools/helpers.ts). The TS
/// stages receive rawInput — the raw model string — and parse it themselves,
/// so here a JSON string is parsed exactly like that path while a JSON
/// object is the already-parsed argument map; anything else fails with the
/// parseToolArguments error text.
pub fn tool_arguments(args: &Value) -> Result<Map<String, Value>> {
    match args {
        Value::String(raw_input) => parse_tool_arguments(raw_input),
        Value::Object(map) => Ok(map.clone()),
        _ => Err(anyhow!("Tool arguments must be a JSON object.")),
    }
}

/// Port of the completion block the complete stages emit — the
/// `{ type: "completion", code, description, language, path }` object in
/// src/tools/types.ts. The `type: "completion"` discriminator is the Rust
/// type itself; `language` is the fenced-code language id ("text" for trees
/// and file dumps).
pub struct ToolCompletionBlock {
    pub code: String,
    pub description: String,
    pub language: String,
    pub path: PathBuf,
}

/// Port of the complete stage result: `{ blocks, toolContent }`.
pub struct ToolCompletion {
    pub blocks: Vec<ToolCompletionBlock>,
    pub tool_content: String,
}
