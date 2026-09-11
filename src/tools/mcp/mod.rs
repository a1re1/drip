// MCP tool adapter: turns the tool list each spawned server advertised into
// ChatToolDefinitions the harness can offer the model, and routes execute
// calls back through the client's `tools/call`. Tool names are namespaced as
// `MCP__<server>__<tool>` so they can never collide with builtins, and the
// `<server>` segment stays recoverable via `mcp_server_of` for per-role
// filtering.
//
// Like the client, everything here is best-effort: a tool-level `is_error`
// reply or a transport failure surfaces as a failed tool block, never as a
// run failure, and MCP tools never take part in the workspace stall
// accounting.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;

use crate::chat::types::ToolCallStatus;
use crate::tools::helpers::parse_tool_arguments;
use crate::tools::types::{
    define_sync_tool, ChatToolCompletionResult, ChatToolDefinition, ChatToolExecuteRequest,
    ChatToolMode, ChatToolParameters, ChatToolPreparedInput, ChatToolResult,
};

use self::client::{McpClient, McpToolInfo};

pub mod client;
pub mod config;

// One sync definition per advertised server tool. The name is namespaced,
// the description carries an `[mcp:<server>]` prefix so the model can tell
// which server a call will hit, and the parameters are the server's
// inputSchema normalized into the tool layer's shape.
pub fn mcp_tool_definitions(
    clients: &[Arc<Mutex<McpClient>>],
) -> Vec<ChatToolDefinition> {
    let mut definitions = Vec::new();
    for client in clients {
        // Snapshot under the lock, then release it: the std mutex is not
        // reentrant, and the per-tool builder must not lock it again.
        let (server, tools) = {
            let guard = client.lock().unwrap_or_else(PoisonError::into_inner);
            (guard.name().to_string(), guard.tools().to_vec())
        };
        for tool in &tools {
            definitions.push(mcp_tool_definition(client, &server, tool));
        }
    }
    definitions
}

// The `<server>` segment of an `MCP__<server>__<tool>` tool name, or None
// for any other name — including malformed MCP names with an empty server
// or tool segment.
pub fn mcp_server_of(tool_name: &str) -> Option<&str> {
    let rest = tool_name.strip_prefix("MCP__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some(server)
}

// Wraps one advertised tool as a sync ChatToolDefinition. The closures own
// an `Arc` clone of the client so `execute` can reach the live connection.
fn mcp_tool_definition(client: &Arc<Mutex<McpClient>>, server: &str, tool: &McpToolInfo) -> ChatToolDefinition {
    let (name, description, parameters) = mcp_tool_metadata(server, tool);
    let server_tool_name = tool.name.clone();
    let execute_client = Arc::clone(client);

    define_sync_tool(ChatToolDefinition {
        name,
        description,
        parameters,
        // MCP tools call out to a server process; they never mutate the
        // workspace, so they stay out of the stall accounting.
        mutates_workspace: false,
        mode: ChatToolMode::Sync,
        prepare: Box::new(move |request: crate::tools::types::ChatToolPrepareRequest<'_>| {
            // The raw input is the tool's JSON arguments object.
            let arguments = parse_tool_arguments(request.raw_input).map_err(|error| error.to_string())?;
            Ok(ChatToolPreparedInput {
                display_input: request.raw_input.to_string(),
                input: Value::Object(arguments),
                tags: None,
            })
        }),
        execute: Box::new(move |request: ChatToolExecuteRequest<'_>| {
            let arguments = request.prepared.input;
            let mut client = execute_client.lock().unwrap_or_else(PoisonError::into_inner);
            match client.call(&server_tool_name, arguments) {
                // A transport failure (dead server, timeout) throws, exactly
                // like a builtin failure thrown through outcome_to_result.
                Err(error) => Err(error),
                Ok(outcome) => {
                    if outcome.is_error {
                        // Tool-level failure: the server ran the tool and
                        // reported isError. Keep its text as the block's
                        // report and mark the block failed — the same shape
                        // a builtin gets when it finishes failed.
                        let text = if outcome.text.is_empty() {
                            "mcp tool reported an error".to_string()
                        } else {
                            outcome.text
                        };
                        Ok(ChatToolResult {
                            output_text: Some(text),
                            status: Some(ToolCallStatus::Failed),
                            ..ChatToolResult::default()
                        })
                    } else {
                        Ok(ChatToolResult {
                            output_text: Some(outcome.text),
                            ..ChatToolResult::default()
                        })
                    }
                }
            }
        }),
        complete: Box::new(|request: crate::tools::types::ChatToolCompleteRequest<'_>| {
            Ok(ChatToolCompletionResult {
                blocks: None,
                tool_content: request.result.output_text.clone(),
                tags: None,
            })
        }),
    })
}

// The name, description, and schema of one `MCP__<server>__<tool>`
// definition, derived purely from the server name and the advertised tool —
// unit-testable without a live connection.
fn mcp_tool_metadata(server: &str, tool: &McpToolInfo) -> (String, String, ChatToolParameters) {
    let name = format!("MCP__{server}__{}", tool.name);
    let description = if tool.description.is_empty() {
        format!("[mcp:{server}]")
    } else {
        format!("[mcp:{server}] {}", tool.description)
    };
    let parameters = parameters_from_schema(&tool.input_schema);
    (name, description, parameters)
}

// Converts a server's raw inputSchema into the tool layer's schema shape. A
// missing or non-object schema degrades to an empty object schema (the
// model can call the tool with `{}`); loose `properties` / `required`
// members are kept only when well-formed.
fn parameters_from_schema(schema: &Value) -> ChatToolParameters {
    let object = match schema.as_object() {
        Some(object) => object,
        None => return ChatToolParameters::object(),
    };

    let mut properties = BTreeMap::new();
    if let Some(schema_properties) = object.get("properties").and_then(Value::as_object) {
        for (key, value) in schema_properties {
            properties.insert(key.clone(), value.clone());
        }
    }

    let required = object.get("required").and_then(Value::as_array).and_then(|entries| {
        let names: Vec<String> = entries
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        // Keep the list only when every entry was a string.
        if !names.is_empty() && names.len() == entries.len() {
            Some(names)
        } else {
            None
        }
    });

    ChatToolParameters {
        additional_properties: None,
        properties,
        required,
        tool_type: "object".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_info(name: &str, description: &str, schema: Value) -> McpToolInfo {
        McpToolInfo {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: schema,
        }
    }

    #[test]
    fn mcp_server_of_extracts_the_server_segment() {
        assert_eq!(mcp_server_of("MCP__fake__echo"), Some("fake"));
        // Only the leading segment is the server.
        assert_eq!(mcp_server_of("MCP__a__b__c"), Some("a"));
        assert_eq!(mcp_server_of("READ"), None);
        assert_eq!(mcp_server_of("mcp"), None);
        assert_eq!(mcp_server_of("MCP__solo"), None);
        assert_eq!(mcp_server_of("MCP____tool"), None);
        assert_eq!(mcp_server_of(""), None);
    }

    #[test]
    fn parameters_keep_properties_and_required() {
        let parameters = parameters_from_schema(&json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        }));
        assert_eq!(parameters.tool_type, "object");
        assert_eq!(parameters.properties.len(), 1);
        assert!(parameters.properties.contains_key("text"));
        assert_eq!(parameters.required, Some(vec!["text".to_string()]));
    }

    #[test]
    fn missing_or_non_object_schemas_become_empty_object_schemas() {
        for schema in [Value::Null, json!(["nope"]), json!("nope"), json!(7)] {
            let parameters = parameters_from_schema(&schema);
            assert!(parameters.properties.is_empty());
            assert_eq!(parameters.required, None);
            assert_eq!(parameters.tool_type, "object");
        }
    }

    #[test]
    fn loose_schema_members_are_dropped() {
        let parameters = parameters_from_schema(&json!({
            "type": "object",
            "properties": "not-an-object",
            "required": [1, 2],
        }));
        assert!(parameters.properties.is_empty());
        assert_eq!(parameters.required, None);
    }

    #[test]
    fn definitions_are_namespaced_prefixed_and_read_only() {
        // The pure metadata helper covers naming/prefix/schema without a
        // live connection; the full definition (mode, mutates_workspace,
        // execute) is exercised by the integration test's real client.
        let (name, description, parameters) = mcp_tool_metadata(
            "fake",
            &tool_info(
                "echo",
                "Echoes text back",
                json!({"type": "object", "properties": {}, "required": []}),
            ),
        );
        assert_eq!(name, "MCP__fake__echo");
        assert_eq!(description, "[mcp:fake] Echoes text back");
        assert_eq!(parameters.tool_type, "object");
        assert_eq!(parameters.required, None);
    }

    #[test]
    fn a_missing_description_still_carries_the_server_prefix() {
        let (name, description, parameters) =
            mcp_tool_metadata("fake", &tool_info("bare", "", Value::Null));
        assert_eq!(name, "MCP__fake__bare");
        assert_eq!(description, "[mcp:fake]");
        // Null schema degraded to an empty object schema.
        assert!(parameters.properties.is_empty());
        assert!(parameters.required.is_none());
    }

    #[test]
    fn an_empty_client_list_yields_no_definitions() {
        assert!(mcp_tool_definitions(&[]).is_empty());
    }
}
