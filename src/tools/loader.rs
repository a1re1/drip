// Tools loader: resolves the entry path of a tools directory and validates
// the exported tool list. drip only ships the built-in tool pack, so the
// import step is a parameter: callers hand in an already-materialized
// ChatToolsModule and load_chat_tools keeps the ordering (resolve the entry
// path first, then run get_chat_tools).

use std::path::{Path, PathBuf};

use super::types::{ChatToolDefinition, ChatToolsModule};

// Supported entry-file names for a tools directory.
const SUPPORTED_TOOL_ENTRIES: [&str; 4] = ["index.tsx", "index.ts", "index.jsx", "index.js"];

// Every property the guard checks (string name/description, object
// parameters, function-valued prepare/execute/complete) is enforced
// statically by ChatToolDefinition (the stages are required boxed closures),
// so the guard collapses to the type itself.
fn is_tool_definition(_tool: &ChatToolDefinition) -> bool {
    true
}

// True when every entry in the list is a valid tool definition.
fn is_tool_list(tool_list: &[ChatToolDefinition]) -> bool {
    tool_list.iter().all(is_tool_definition)
}

// Resolves the tools entry path: a direct file path wins; otherwise the
// supported entry filenames are probed in order under the directory form
// of the path.
pub fn resolve_tools_entry_path(tools_path: &str) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let absolute_path = resolve_from(&cwd, Path::new(tools_path));

    if absolute_path.is_file() {
        return Ok(absolute_path);
    }

    for filename in SUPPORTED_TOOL_ENTRIES {
        let next_candidate = absolute_path.join(filename);

        if next_candidate.exists() {
            return Ok(next_candidate);
        }
    }

    Err(format!(
        "No tools entry found for \"{}\". Expected one of {}.",
        tools_path,
        SUPPORTED_TOOL_ENTRIES.join(", ")
    ))
}

// Path resolution against the cwd: absolute inputs stay as-is; relative
// inputs join the cwd.
fn resolve_from(cwd: &Path, tools_path: &Path) -> PathBuf {
    if tools_path.is_absolute() {
        tools_path.to_path_buf()
    } else {
        cwd.join(tools_path)
    }
}

// ChatToolDefinition owns non-Clone boxed closures, so the module is
// consumed by value and the winning list is moved out.
pub fn get_chat_tools(tools_module: ChatToolsModule) -> Result<Vec<ChatToolDefinition>, String> {
    // The default export wins when it is a tool list; otherwise the named
    // "tools" export is used.
    let default_is_tool_list = tools_module
        .default
        .as_ref()
        .map(|tool_list| is_tool_list(tool_list))
        .unwrap_or(false);
    let named_is_tool_list = tools_module
        .tools
        .as_ref()
        .map(|tool_list| is_tool_list(tool_list))
        .unwrap_or(false);

    let tool_list = if default_is_tool_list {
        tools_module.default.unwrap_or_default()
    } else if named_is_tool_list {
        tools_module.tools.unwrap_or_default()
    } else {
        return Err(
            "The tools module must export a default array or a named \"tools\" array of tool definitions."
                .to_string(),
        );
    };

    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for tool in &tool_list {
        if seen_names.contains(&tool.name) {
            return Err(format!(
                "Duplicate tool name \"{}\" found in the loaded tools module.",
                tool.name
            ));
        }

        seen_names.insert(tool.name.clone());
    }

    Ok(tool_list)
}

// The module arrives already materialized — drip only loads the built-in
// pack — so the function keeps the ordering: entry resolution, then
// validation.
pub fn load_chat_tools(
    tools_path: &str,
    tools_module: ChatToolsModule,
) -> Result<Vec<ChatToolDefinition>, String> {
    resolve_tools_entry_path(tools_path)?;
    get_chat_tools(tools_module)
}

#[cfg(test)]
mod tests {
    use super::super::types::{
        define_sync_tool, ChatToolCompletionResult, ChatToolDefinition, ChatToolMode,
        ChatToolParameters, ChatToolPreparedInput, ChatToolResult,
    };
    use super::*;

    // A sync DEMO test tool: prepare → "{}", execute → "done", complete →
    // no blocks.
    fn demo_tool() -> ChatToolDefinition {
        define_sync_tool(ChatToolDefinition {
            name: "DEMO".to_string(),
            description: "demo".to_string(),
            parameters: ChatToolParameters::object(),
            mutates_workspace: false,
            mode: ChatToolMode::Sync,
            prepare: Box::new(|_| {
                Ok(ChatToolPreparedInput {
                    display_input: "{}".to_string(),
                    input: serde_json::json!({}),
                    tags: None,
                })
            }),
            execute: Box::new(|_| {
                Ok(ChatToolResult {
                    data: Some(serde_json::Value::Null),
                    output_text: Some("done".to_string()),
                    ..Default::default()
                })
            }),
            complete: Box::new(|_| {
                Ok(ChatToolCompletionResult {
                    blocks: Some(Vec::new()),
                    tool_content: None,
                 tags: None })
            }),
        })
    }

    #[test]
    fn prefers_a_default_export_when_present() {
        let tools = get_chat_tools(ChatToolsModule {
            default: Some(vec![demo_tool()]),
            tools: None,
        })
        .expect("a default export is preferred");

        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, vec!["DEMO"]);
        assert_eq!(tools[0].description, "demo");
    }

    #[test]
    fn falls_back_to_a_named_tools_export() {
        let tools = get_chat_tools(ChatToolsModule {
            default: None,
            tools: Some(vec![demo_tool()]),
        })
        .expect("the named export is used when no default exists");

        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, vec!["DEMO"]);
        assert_eq!(tools[0].description, "demo");
    }

    #[test]
    fn resolves_an_index_ts_file_inside_the_provided_tools_directory() {
        let tools_dir = tempfile::tempdir().expect("tempdir");
        let entry_path = tools_dir.path().join("index.ts");

        std::fs::write(&entry_path, "export default [];\n").expect("write entry file");

        let resolved = resolve_tools_entry_path(tools_dir.path().to_str().expect("utf-8 tempdir"))
            .expect("index.ts inside the tools directory resolves");

        assert_eq!(resolved, entry_path);
    }

    // Covers the first refusal site (error string verbatim): a module with
    // neither export is refused.
    #[test]
    fn modules_without_a_tool_list_are_refused() {
        let error = get_chat_tools(ChatToolsModule {
            default: None,
            tools: None,
        })
        .expect_err("an empty tools module is refused");

        assert_eq!(
            error,
            "The tools module must export a default array or a named \"tools\" array of tool definitions."
        );
    }

    // Covers the second refusal site (error string verbatim): duplicate tool
    // names are refused.
    #[test]
    fn duplicate_tool_names_are_refused() {
        let error = get_chat_tools(ChatToolsModule {
            default: Some(vec![demo_tool(), demo_tool()]),
            tools: None,
        })
        .expect_err("duplicate tool names are refused");

        assert_eq!(
            error,
            "Duplicate tool name \"DEMO\" found in the loaded tools module."
        );
    }

    // Covers the resolve_tools_entry_path refusal site (error string
    // verbatim, including the supported-entry roster).
    #[test]
    fn no_tools_entry_error_lists_supported_entries() {
        let empty_dir = tempfile::tempdir().expect("tempdir");
        let tools_path = empty_dir
            .path()
            .to_str()
            .expect("utf-8 tempdir")
            .to_string();

        let error =
            resolve_tools_entry_path(&tools_path).expect_err("a missing entry reports an error");

        assert_eq!(
            error,
            format!(
                "No tools entry found for \"{}\". Expected one of index.tsx, index.ts, index.jsx, index.js.",
                tools_path
            )
        );
    }
}
