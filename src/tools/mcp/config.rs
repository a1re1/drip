// The MCP client configuration. This mirrors Claude Code's `.mcp.json`
// shape: a top-level `mcpServers` object keyed by server name, each entry
// a stdio command plus optional args/env/timeout. Everything here is
// opt-in and never fatal — a malformed file or entry warns and yields an
// empty map, mirroring how hooks degrade in `loadCliConfig`.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

// The on-disk shape of one MCP server entry. Wire fields stay camelCase so
// the config matches Claude Code's `.mcp.json` byte for byte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    // The binary to spawn, resolved via PATH (or executed directly when it
    // contains a path separator).
    #[serde(default)]
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    // Per-call and initialize timeout, seconds. Defaults to 60.
    #[serde(rename = "timeoutSecs", default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    60
}

impl Default for McpServerConfig {
    fn default() -> Self {
        McpServerConfig {
            command: String::new(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

// The whole `mcpServers` section: name -> server config. A BTreeMap keeps
// spawn order deterministic across runs.
pub type McpServerMap = BTreeMap<String, McpServerConfig>;

// Merges the global `mcpServers` map with the project one. Project entries
// win on name collision; both sources may be missing entirely.
pub fn merge_mcp_servers(
    global: Option<&McpServerMap>,
    project: Option<&McpServerMap>,
) -> McpServerMap {
    let mut merged = McpServerMap::new();
    if let Some(global) = global {
        for (name, server) in global {
            merged.insert(name.clone(), server.clone());
        }
    }
    if let Some(project) = project {
        for (name, server) in project {
            merged.insert(name.clone(), server.clone());
        }
    }
    merged
}

// Parses a `mcpServers` JSON value (as found in the config file or a
// `.drip/mcp.json` body) into the name -> config map. Only a section that is
// not an object is an error. A bad *entry* — malformed shape, no `command`,
// or a name the `MCP__<server>__<tool>` scheme cannot carry — is skipped
// with a line pushed to `warnings`, so one remote or broken entry never
// takes its stdio siblings down with it.
pub fn parse_mcp_servers(
    value: Option<&serde_json::Value>,
    warnings: &mut Vec<String>,
) -> Result<McpServerMap, String> {
    let value = match value {
        None => return Ok(McpServerMap::new()),
        Some(value) => value,
    };
    let entries = value
        .as_object()
        .ok_or_else(|| "ignoring \"mcpServers\": expected an object keyed by server name".to_string())?;
    let mut servers = McpServerMap::new();
    for (name, entry) in entries {
        // The name is embedded in tool names as MCP__<server>__<tool> and
        // recovered by splitting on the first `__`, so it must not contain
        // one — a role granting `my__server` would otherwise never see its
        // tools, with nothing saying why.
        if name.trim().is_empty() || name.contains("__") {
            warnings.push(format!(
                "mcpServers: skipping server \"{name}\": names may not be empty or contain \"__\" (it separates MCP__<server>__<tool>)"
            ));
            continue;
        }
        let server = match serde_json::from_value::<McpServerConfig>(entry.clone()) {
            Ok(server) => server,
            Err(error) => {
                warnings.push(format!("mcpServers: skipping server \"{name}\": {error}"));
                continue;
            }
        };
        // Only stdio servers are supported: an entry without a `command` (for
        // example a Claude Code `{"type":"sse","url":...}` entry) would
        // otherwise parse silently and fail later as a confusing spawn error.
        if server.command.trim().is_empty() {
            warnings.push(format!(
                "mcpServers: skipping server \"{name}\": no command (only stdio servers are supported)"
            ));
            continue;
        }
        servers.insert(name.clone(), server);
    }
    Ok(servers)
}

// Reads `<cwd>/.drip/mcp.json`, whose body is `{ "mcpServers": { ... } }`.
// Missing file -> Ok(None); unreadable file or non-object section -> Err
// with the warning text; skipped entries land in `warnings`, prefixed with
// the file path. The caller degrades to an empty map on Err.
pub fn read_project_mcp_servers(
    cwd: &Path,
    warnings: &mut Vec<String>,
) -> Result<Option<McpServerMap>, String> {
    let path = cwd.join(".drip").join("mcp.json");
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    let parsed: serde_json::Value = serde_json::from_str(&content)
        .map_err(|error| format!("ignoring {}: {error}", path.display()))?;
    let mut entry_warnings = Vec::new();
    let servers = parse_mcp_servers(parsed.get("mcpServers"), &mut entry_warnings)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    warnings.extend(entry_warnings.into_iter().map(|warning| format!("{}: {warning}", path.display())));
    Ok(Some(servers))
}

// Loads the effective server map for a run: the already-parsed global
// `mcpServers` section merged with `<cwd>/.drip/mcp.json` (project wins on
// name collision). A malformed project file or entry warns on stderr and
// degrades to what did parse — MCP is opt-in and never fatal.
pub fn load_mcp_servers(global: &McpServerMap, cwd: &Path) -> McpServerMap {
    let mut warnings = Vec::new();
    let project = read_project_mcp_servers(cwd, &mut warnings).unwrap_or_else(|error| {
        warnings.push(error);
        None
    });
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    merge_mcp_servers(Some(global), project.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global_map(name: &str, command: &str) -> McpServerMap {
        let mut map = McpServerMap::new();
        map.insert(
            name.to_string(),
            McpServerConfig {
                command: command.to_string(),
                ..McpServerConfig::default()
            },
        );
        map
    }

    fn parse_ok(value: &serde_json::Value) -> McpServerMap {
        let mut warnings = Vec::new();
        let map = parse_mcp_servers(Some(value), &mut warnings).unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        map
    }

    fn parse_with_warnings(value: &serde_json::Value) -> (McpServerMap, Vec<String>) {
        let mut warnings = Vec::new();
        let map = parse_mcp_servers(Some(value), &mut warnings).unwrap();
        (map, warnings)
    }

    #[test]
    fn entry_without_command_is_skipped_with_a_warning_and_siblings_survive() {
        let (map, warnings) = parse_with_warnings(&serde_json::json!({
            "remote": {"type": "sse", "url": "http://x"},
            "blank": {"command": "  "},
            "local": {"command": "local-mcp"}
        }));
        assert_eq!(map.keys().collect::<Vec<_>>(), vec!["local"]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("\"remote\"") && w.contains("no command")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("\"blank\"")), "{warnings:?}");
    }

    #[test]
    fn server_names_that_break_the_tool_name_scheme_are_skipped() {
        let (map, warnings) = parse_with_warnings(&serde_json::json!({
            "my__server": {"command": "a"},
            "": {"command": "b"},
            "ok-server": {"command": "c"}
        }));
        assert_eq!(map.keys().collect::<Vec<_>>(), vec!["ok-server"]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("\"my__server\"") && w.contains("__")), "{warnings:?}");
    }

    #[test]
    fn parses_full_server_entry() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"fake":{"command":"fake-mcp","args":["--x"],"env":{"K":"V"},"timeoutSecs":7}}"#,
        )
        .unwrap();
        let map = parse_ok(&value);
        let server = map.get("fake").unwrap();
        assert_eq!(server.command, "fake-mcp");
        assert_eq!(server.args, vec!["--x".to_string()]);
        assert_eq!(server.env.get("K").map(String::as_str), Some("V"));
        assert_eq!(server.timeout_secs, 7);
    }

    #[test]
    fn defaults_args_env_and_timeout() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"bare":{"command":"bare-mcp"}}"#).unwrap();
        let map = parse_ok(&value);
        let server = map.get("bare").unwrap();
        assert!(server.args.is_empty());
        assert!(server.env.is_empty());
        assert_eq!(server.timeout_secs, 60);
    }

    #[test]
    fn missing_section_is_empty_map() {
        assert!(parse_mcp_servers(None, &mut Vec::new()).unwrap().is_empty());
        assert!(parse_ok(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn malformed_entry_is_skipped_and_non_object_section_is_an_error() {
        let (map, warnings) = parse_with_warnings(&serde_json::json!({
            "broken": {"args": "not-an-array"},
            "fine": {"command": "fine-mcp"}
        }));
        assert_eq!(map.keys().collect::<Vec<_>>(), vec!["fine"]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("\"broken\""), "{warnings:?}");
        let scalar = serde_json::json!(7);
        assert!(parse_mcp_servers(Some(&scalar), &mut Vec::new()).is_err());
    }

    #[test]
    fn project_overrides_global_on_name_collision() {
        let global: McpServerMap = parse_ok(&serde_json::json!({
            "shared": {"command": "global-cmd"},
            "onlyGlobal": {"command": "g"}
        }));
        let project: McpServerMap = parse_ok(&serde_json::json!({
            "shared": {"command": "project-cmd", "args": ["--p"]},
            "onlyProject": {"command": "p"}
        }));
        let merged = merge_mcp_servers(Some(&global), Some(&project));
        assert_eq!(merged.len(), 3);
        assert_eq!(
            merged.get("shared").unwrap().command,
            "project-cmd",
            "project wins the collision"
        );
        assert_eq!(merged.get("shared").unwrap().args, vec!["--p".to_string()]);
        assert!(merged.contains_key("onlyGlobal"));
        assert!(merged.contains_key("onlyProject"));
    }

    #[test]
    fn merge_with_missing_sides_keeps_the_present_side() {
        let global: McpServerMap =
            parse_ok(&serde_json::json!({"a": {"command": "a"}}));
        assert_eq!(
            merge_mcp_servers(Some(&global), None).len(),
            1,
            "global-only"
        );
        assert_eq!(
            merge_mcp_servers(None, Some(&global)).len(),
            1,
            "project-only"
        );
        assert!(merge_mcp_servers(None, None).is_empty());
    }

    #[test]
    fn project_file_is_merged_over_the_global_section() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".drip")).unwrap();
        std::fs::write(
            dir.path().join(".drip").join("mcp.json"),
            r#"{"mcpServers":{"proj":{"command":"proj-cmd"}}}"#,
        )
        .unwrap();
        let servers = load_mcp_servers(&global_map("glob", "glob-cmd"), dir.path());
        assert_eq!(servers.len(), 2);
        assert_eq!(servers.get("proj").unwrap().command, "proj-cmd");
        assert_eq!(servers.get("glob").unwrap().command, "glob-cmd");
    }

    #[test]
    fn malformed_project_file_degrades_to_empty_with_no_global_loss() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".drip")).unwrap();
        std::fs::write(dir.path().join(".drip").join("mcp.json"), "not json").unwrap();
        let servers = load_mcp_servers(&global_map("glob", "glob-cmd"), dir.path());
        assert_eq!(servers.len(), 1);
        assert_eq!(servers.get("glob").unwrap().command, "glob-cmd");
    }

    #[test]
    fn missing_project_file_and_missing_global_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let servers = load_mcp_servers(&McpServerMap::new(), dir.path());
        assert!(servers.is_empty());
    }

    #[test]
    fn project_file_without_mcp_servers_key_degrades_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".drip")).unwrap();
        std::fs::write(dir.path().join(".drip").join("mcp.json"), r#"{"other":true}"#).unwrap();
        let servers = load_mcp_servers(&McpServerMap::new(), dir.path());
        assert!(servers.is_empty());
    }
}
