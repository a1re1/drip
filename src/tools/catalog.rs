// port of src/tools/catalog.ts

use super::types::{ChatToolDefinition, ChatToolMode};

// function mergeToolDefinitions(...toolGroups: ChatToolDefinition[][])
pub fn merge_tool_definitions(tool_groups: Vec<Vec<ChatToolDefinition>>) -> Vec<ChatToolDefinition> {
    let mut merged_tools: Vec<ChatToolDefinition> = Vec::new();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for tool_group in tool_groups {
        for tool in tool_group {
            if seen_names.contains(&tool.name) {
                continue;
            }

            seen_names.insert(tool.name.clone());
            merged_tools.push(tool);
        }
    }

    merged_tools
}

// export async function resolveRuntimeToolCatalog(tools) — drip only ships the
// built-in pack, so the dynamic `import("./framework-tools")` becomes a plain
// parameter: the caller passes the framework definitions it already has (from
// the built-in tool pack once it is ported; drip/src/tools/builtin/ is still
// 1-line stubs). The TS Promise collapses to a synchronous return.
pub fn resolve_runtime_tool_catalog(
    tools: Vec<ChatToolDefinition>,
    framework_tool_definitions: Vec<ChatToolDefinition>,
) -> Vec<ChatToolDefinition> {
    if !tools.iter().any(|tool| tool.mode == ChatToolMode::Async) {
        return tools;
    }

    merge_tool_definitions(vec![tools, framework_tool_definitions])
}

// export function filterRuntimeToolCatalog(args)
pub struct FilterRuntimeToolCatalogArgs {
    pub tool_access: ToolAccess,
    pub tool_names: Option<Vec<String>>,
    pub tools: Vec<ChatToolDefinition>,
}

// toolAccess: "all" | "selected"
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolAccess {
    All,
    Selected,
}

pub fn filter_runtime_tool_catalog(args: FilterRuntimeToolCatalogArgs) -> Vec<ChatToolDefinition> {
    if args.tool_access != ToolAccess::Selected {
        return args.tools;
    }

    let allowed_tool_names: std::collections::HashSet<String> = (args.tool_names.unwrap_or_default())
        .iter()
        .map(|tool_name| tool_name.trim().to_string())
        .filter(|tool_name| !tool_name.is_empty())
        .collect();

    args.tools
        .into_iter()
        .filter(|tool| allowed_tool_names.contains(&tool.name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::types::{define_sync_tool, ChatToolDefinition, ChatToolMode, ChatToolParameters};
    use super::*;

    // const readTool — the shared base definition from test/tool-catalog.test.ts:
    // sync, prepare → empty display, execute → "read", complete → no blocks.
    fn read_tool() -> ChatToolDefinition {
        define_sync_tool(ChatToolDefinition {
            name: "READ".to_string(),
            description: "Read a file.".to_string(),
            parameters: ChatToolParameters::object(),
            mutates_workspace: false,
            mode: ChatToolMode::Sync,
            prepare: Box::new(|_| {
                Ok(super::super::types::ChatToolPreparedInput {
                    display_input: String::new(),
                    input: serde_json::json!({}),
                    tags: None,
                })
            }),
            execute: Box::new(|_| {
                Ok(super::super::types::ChatToolResult {
                    data: Some(serde_json::Value::Null),
                    output_text: Some("read".to_string()),
                    ..Default::default()
                })
            }),
            complete: Box::new(|_| Ok(super::super::types::ChatToolCompletionResult { blocks: None, tool_content: None, tags: None })),
        })
    }

    // const asyncTool = { ...readTool, mode: "async", name: "START_DEV" }
    fn async_tool() -> ChatToolDefinition {
        let mut tool = read_tool();
        tool.mode = ChatToolMode::Async;
        tool.name = "START_DEV".to_string();
        tool
    }

    // it("keeps the full catalog when tool access is set to all")
    #[test]
    fn keeps_the_full_catalog_when_tool_access_is_set_to_all() {
        let catalog = filter_runtime_tool_catalog(FilterRuntimeToolCatalogArgs {
            tool_access: ToolAccess::All,
            tool_names: Some(vec!["READ".to_string()]),
            tools: vec![read_tool()],
        });
        let names: Vec<&str> = catalog.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, vec!["READ"]);
    }

    // it("filters the catalog down to the selected tool names")
    #[test]
    fn filters_the_catalog_down_to_the_selected_tool_names() {
        let catalog = filter_runtime_tool_catalog(FilterRuntimeToolCatalogArgs {
            tool_access: ToolAccess::Selected,
            tool_names: Some(vec!["READ".to_string()]),
            tools: vec![read_tool(), async_tool()],
        });
        let names: Vec<&str> = catalog.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, vec!["READ"]);
    }

    // it("includes framework async helper tools when async tools are present")
    //
    // drip has no ported framework pack yet, so the test stands in for
    // getFrameworkToolDefinitions() with stub ASYNC_TAIL/ASYNC_WAIT definitions
    // carrying the same names.
    #[test]
    fn includes_framework_async_helper_tools_when_async_tools_are_present() {
        let framework_tool_definitions = vec![stub_framework_tool("ASYNC_TAIL"), stub_framework_tool("ASYNC_WAIT")];
        let catalog = resolve_runtime_tool_catalog(vec![async_tool()], framework_tool_definitions);
        let names: Vec<&str> = catalog.iter().map(|tool| tool.name.as_str()).collect();

        assert!(names.contains(&"START_DEV"));
        assert!(names.contains(&"ASYNC_TAIL"));
        assert!(names.contains(&"ASYNC_WAIT"));
    }

    fn stub_framework_tool(name: &str) -> ChatToolDefinition {
        let mut tool = read_tool();
        tool.name = name.to_string();
        tool
    }
}
