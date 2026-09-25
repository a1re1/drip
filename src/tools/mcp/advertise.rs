// The server-advertised tool snapshot for one session.
//
// An MCP tool's definition lives in another process: only the handshake
// (`initialize` → `notifications/initialized` → `tools/list`) ever sees what
// the server itself says about a tool, and the harness normalizes that reply
// into `MCP__<server>__<tool>` definitions on its way to the model. A reader
// that wants the same listing later — dripw's `[4]` pane read-up — must not
// launch a server of its own to ask again, so every run records what its own
// spawned servers advertised in its session directory and readers load that
// snapshot back. Nothing here is fatal: a session that cannot record its
// advertisement still runs, and a reader that finds none falls back to
// whatever other surface carries the name.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The snapshot's file name, under a session's directory.
pub const MCP_TOOLS_FILE: &str = "mcp_tools.json";

/// One tool as its own server advertised it, beside the normalized definition
/// the harness built from that advertisement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpToolAdvertisement {
    /// The namespaced name loops call the tool by (`MCP__<server>__<tool>`).
    pub name: String,
    /// The configured server that advertises it.
    pub server: String,
    /// The tool name as the server spells it.
    pub tool: String,
    /// The server's own description, verbatim.
    pub description: String,
    /// The raw `inputSchema` from `tools/list` — the whole advertised shape,
    /// including every nested key the adapter drops — or `null` when the
    /// server omitted it.
    pub input_schema: Value,
    /// The schema the model is actually handed, after normalization.
    pub parameters: Value,
}

/// Writes `advertisements` to `<dir>/mcp_tools.json`. Best-effort by design:
/// the caller ignores the result and the session runs either way.
pub fn save(dir: &Path, advertisements: &[McpToolAdvertisement]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec_pretty(advertisements).map_err(std::io::Error::other)?;
    std::fs::write(dir.join(MCP_TOOLS_FILE), bytes)
}

/// The snapshot recorded for a session directory, or an empty list when there
/// is none — a session from before this existed, a run that spawned no
/// server, or an unreadable/malformed file. Never fails, so a reader always
/// has something to fall back from.
pub fn load(dir: &Path) -> Vec<McpToolAdvertisement> {
    std::fs::read(dir.join(MCP_TOOLS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn advertisement() -> McpToolAdvertisement {
        McpToolAdvertisement {
            name: "MCP__echo__echo".to_string(),
            server: "echo".to_string(),
            tool: "echo".to_string(),
            // Unicode, an ellipsis and a newline survive the round trip byte
            // for byte: the reader shows the server's own words.
            description: "Echo text back — ünïcode ✓…\nsecond line".to_string(),
            input_schema: json!({
                "additionalProperties": false,
                "properties": {
                    "text": {
                        "description": "the text to echo…",
                        "enum": ["a", "b"],
                        "maxLength": 64,
                        "type": "string"
                    }
                },
                "required": ["text"],
                "title": "echo input",
                "type": "object"
            }),
            parameters: json!({
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "type": "object"
            }),
        }
    }

    #[test]
    fn a_snapshot_round_trips_the_advertised_shape_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let written = vec![advertisement()];
        save(dir.path(), &written).expect("save");
        let loaded = load(dir.path());
        // Whole advertisements, raw nested schema and unicode included — not
        // the normalized subset the model gets.
        assert_eq!(loaded, written);
        assert_eq!(loaded[0].input_schema["properties"]["text"]["enum"][1], "b");
        assert_eq!(
            loaded[0].input_schema["properties"]["text"]["maxLength"],
            64
        );
        assert_eq!(loaded[0].input_schema["title"], "echo input");
        assert!(loaded[0].description.contains('—'));
    }

    #[test]
    fn a_session_that_recorded_nothing_loads_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load(dir.path()).is_empty(), "missing file");
        assert!(
            load(Path::new("/nope/never-there")).is_empty(),
            "missing dir"
        );
    }

    #[test]
    fn a_malformed_snapshot_loads_empty_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(MCP_TOOLS_FILE), b"{not json").expect("write");
        assert!(load(dir.path()).is_empty());
        // A snapshot of the wrong shape (an object, not an array) is the same
        // graceful fallback.
        std::fs::write(dir.path().join(MCP_TOOLS_FILE), br#"{"tools":[]}"#).expect("write");
        assert!(load(dir.path()).is_empty());
    }
}
