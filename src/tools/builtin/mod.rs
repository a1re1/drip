// Each built-in tool module exposes the same behavior as plain functions:
//   - definition() — the exact OpenAI function schema drip sends, i.e. the
//     {type: "function", function: {name, description, parameters}} envelope
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

/// The `context` slice built-in tools touch: the cwd tools resolve
/// relative paths against, and the network permission flag services like
/// fetch honor. Later revisions add fields as tools need them.
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

/// The tool-call surface the model sees. `text` is the content of the
/// tool-role message, pushed to the transcript verbatim. Failures are
/// prefixed with "ERROR: "; `failed` marks the tool-call block status
/// "failed".
pub struct ToolOutcome {
    pub text: String,
    pub failed: bool,
}

impl ToolOutcome {
    pub fn success(text: String) -> Self {
        Self { text, failed: false }
    }

    /// Tool content is "ERROR: " followed by the error message.
    pub fn error(error: anyhow::Error) -> Self {
        Self {
            text: format!("ERROR: {error}"),
            failed: true,
        }
    }
}

/// Bridge from the execute(args: &Value) contract above to the helpers'
/// parse_tool_arguments. A JSON string is the raw model string and is
/// parsed; a JSON object is the already-parsed argument map; anything else
/// fails with the parse_tool_arguments error text.
pub fn tool_arguments(args: &Value) -> Result<Map<String, Value>> {
    match args {
        Value::String(raw_input) => parse_tool_arguments(raw_input),
        Value::Object(map) => Ok(map.clone()),
        _ => Err(anyhow!("Tool arguments must be a JSON object.")),
    }
}

/// The completion block the complete stages emit: the code, description,
/// fenced-code language id ("text" for trees and file dumps), and path. The
/// Rust type itself is the block's discriminator.
pub struct ToolCompletionBlock {
    pub code: String,
    pub description: String,
    pub language: String,
    pub path: PathBuf,
}

/// The complete stage result: the emitted blocks plus the tool-role content string.
pub struct ToolCompletion {
    pub blocks: Vec<ToolCompletionBlock>,
    pub tool_content: String,
}
