// Runs a single tool call through the three lifecycle stages defined in
// types.rs (prepare → execute → complete), merging display tags along the
// way and naming every failure (missing tool, JSON-argument parse errors,
// stage errors) back to the model through a failed tool-call block.
//
// Adaptations (the drip stage closures are sync boxed fns returning
// Result<_, String>): a stage failure is `Err(String)`, and each stage call
// is an internal helper returning Result so `?` short-circuits: any Err
// falls into build_failure_result, like a catch block. A missing message
// collapses to `error.is_empty()` — stages always hand back a message.

use serde_json::Value;

use crate::chat::types::{
    ChatMessage, ChatMessageBlock, ChatRuntimeContext, ChatTag, ToolCallBlock, ToolCallStatus,
};
use crate::tools::types::{
    ChatAsyncToolJob, ChatToolCompletionResult, ChatToolDefinition, ChatToolPreparedInput,
    ChatToolResult, ChatToolRuntimeServices,
};

// Request shape for execute_tool_call: call id, conversation history, the
// triggering message, raw argument JSON, runtime context, runtime services,
// the resolved tool, and its name.
pub struct ToolExecutionContext<'a> {
    pub call_id: &'a str,
    pub history: &'a [ChatMessage],
    pub message: &'a ChatMessage,
    pub raw_input: &'a str,
    pub runtime_context: ChatRuntimeContext,
    pub services: ChatToolRuntimeServices,
    pub tool: Option<&'a ChatToolDefinition>,
    pub tool_name: &'a str,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExecutedToolCall {
    pub blocks: Vec<ChatMessageBlock>,
    pub tool_content: String,
}

// Normalizes a tool name into the "tool:<lowercased name>" tag.
pub fn normalize_tool_tag(tool_name: &str) -> String {
    format!("tool:{}", tool_name.trim().to_lowercase())
}

// String tags serialize as themselves, structured tags as their JSON form;
// this is the dedupe key for merge_tags.
fn serialize_tag(tag: &ChatTag) -> String {
    match tag {
        ChatTag::Text(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

// Merges the tag groups, deduplicating by the serialize_tag key in
// first-seen order; None when nothing survives.
pub fn merge_tags(tag_groups: &[Option<Vec<ChatTag>>]) -> Option<Vec<ChatTag>> {
    let mut merged_tags: Vec<ChatTag> = Vec::new();
    let mut seen_tags: std::collections::HashSet<String> = std::collections::HashSet::new();

    for tags in tag_groups {
        let tags = match tags {
            Some(tags) => tags,
            None => continue,
        };
        for tag in tags {
            let next_key = serialize_tag(tag);

            if !seen_tags.insert(next_key) {
                continue;
            }

            merged_tags.push(tag.clone());
        }
    }

    if merged_tags.is_empty() {
        None
    } else {
        Some(merged_tags)
    }
}

// Serializes blocks for display: each block's non-empty parts are joined
// with newlines, the block texts are joined with blank lines, and empty
// results are dropped.
fn serialize_blocks(blocks: &[ChatMessageBlock]) -> String {
    let block_texts: Vec<String> = blocks
        .iter()
        .filter_map(|block| {
            let text = match block {
                ChatMessageBlock::Text(block) => block.text.clone(),
                ChatMessageBlock::ToolCall(block) => {
                    let status = serde_json::to_string(&block.status)
                        .unwrap_or_default()
                        .trim_matches('"')
                        .to_string();
                    let parts = [
                        Some(block.tool_name.clone()),
                        Some(status),
                        Some(block.input.clone().unwrap_or_default()),
                        Some(block.output.clone().unwrap_or_default()),
                    ];
                    parts
                        .into_iter()
                        .flatten()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                ChatMessageBlock::Completion(block) => {
                    let parts = [
                        block.path.clone(),
                        block.description.clone(),
                        Some(block.code.clone()),
                    ];
                    parts
                        .into_iter()
                        .flatten()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            };
            if text.trim().is_empty() {
                None
            } else {
                Some(text)
            }
        })
        .collect();
    block_texts.join("\n\n")
}

// The job's wire status string, rendered after "Status: ".
fn async_job_status_text(job: &ChatAsyncToolJob) -> &'static str {
    match job.status {
        crate::tools::types::ChatAsyncToolJobStatus::Completed => "completed",
        crate::tools::types::ChatAsyncToolJobStatus::Failed => "failed",
        crate::tools::types::ChatAsyncToolJobStatus::Running => "running",
    }
}

// Formats the async-job acknowledgement shown to the model: job id, target
// tool, current status, log path, and how to follow along.
fn format_async_job_output(tool_name: &str, job: &ChatAsyncToolJob) -> String {
    [
        format!("Started async job {} for {}.", job.id, tool_name),
        format!("Status: {}", async_job_status_text(job)),
        format!("Log file: {}", job.log_path),
        "Use ASYNC_TAIL to inspect recent logs or ASYNC_WAIT to block until the job finishes."
            .to_string(),
    ]
    .join("\n")
}

// Builds the failed tool-call block reported back to the model.
fn build_failure_result(call_id: &str, error: &str, raw_input: &str, tool_name: &str) -> ExecutedToolCall {
    let output = if error.is_empty() {
        // Default branch: unreachable when stages always hand back a
        // message, kept as a stable fallback string.
        "Tool execution failed.".to_string()
    } else {
        error.to_string()
    };

    ExecutedToolCall {
        blocks: vec![ChatMessageBlock::ToolCall(ToolCallBlock {
            context_state: None,
            input: Some(raw_input.to_string()),
            label: Some(call_id.to_string()),
            output: Some(output.clone()),
            status: ToolCallStatus::Failed,
            tags: Some(vec![ChatTag::Text(normalize_tool_tag(tool_name))]),
            tool_name: tool_name.to_string(),
        })],
        tool_content: format!("ERROR: {}", output),
    }
}

// Assembles a tool-call block, merging the tool's tag with the stage tags.
#[allow(clippy::too_many_arguments)]
fn build_tool_call_block(
    call_id: &str,
    completion_tags: Option<Vec<ChatTag>>,
    display_input: &str,
    output_text: &str,
    status: Option<ToolCallStatus>,
    tags: Option<Vec<ChatTag>>,
    tool_name: &str,
) -> ToolCallBlock {
    ToolCallBlock {
        context_state: None,
        input: Some(display_input.to_string()),
        label: Some(call_id.to_string()),
        output: Some(output_text.to_string()),
        status: status.unwrap_or(ToolCallStatus::Completed),
        tags: merge_tags(&[
            Some(vec![ChatTag::Text(normalize_tool_tag(tool_name))]),
            tags,
            completion_tags,
        ]),
        tool_name: tool_name.to_string(),
    }
}

// Stamps every block with the tool's tag.
fn add_tool_tags(tool_name: &str, blocks: &[ChatMessageBlock]) -> Vec<ChatMessageBlock> {
    let tool_tag = normalize_tool_tag(tool_name);

    blocks
        .iter()
        .map(|block| {
            let mut block = block.clone();
            match &mut block {
                ChatMessageBlock::Text(inner) => {
                    inner.tags = merge_tags(&[Some(vec![ChatTag::Text(tool_tag.clone())]), inner.tags.clone()]);
                }
                ChatMessageBlock::ToolCall(inner) => {
                    inner.tags = merge_tags(&[Some(vec![ChatTag::Text(tool_tag.clone())]), inner.tags.clone()]);
                }
                ChatMessageBlock::Completion(inner) => {
                    inner.tags = merge_tags(&[Some(vec![ChatTag::Text(tool_tag.clone())]), inner.tags.clone()]);
                }
            }
            block
        })
        .collect()
}

// Stage call helpers — each runs one stage whose failure the surrounding
// dispatch turns into a failure result.
fn run_prepare(
    tool: &ChatToolDefinition,
    args: &ToolExecutionContext<'_>,
) -> Result<ChatToolPreparedInput, String> {
    (tool.prepare)(crate::tools::types::ChatToolPrepareRequest {
        call_id: args.call_id,
        history: args.history,
        message: args.message,
        raw_input: args.raw_input,
        runtime_context: args.runtime_context.clone(),
        services: args.services.clone(),
    })
}

fn run_execute(
    tool: &ChatToolDefinition,
    args: &ToolExecutionContext<'_>,
    prepared: ChatToolPreparedInput,
) -> Result<ChatToolResult, String> {
    let result = (tool.execute)(crate::tools::types::ChatToolExecuteRequest {
        call_id: args.call_id,
        history: args.history,
        message: args.message,
        prepared,
        runtime_context: args.runtime_context.clone(),
        services: args.services.clone(),
    })?;

    // An async tool must return async-job metadata from execute.
    if tool.mode == crate::tools::types::ChatToolMode::Async && result.async_job.is_none() {
        return Err(format!(
            "Async tool \"{}\" must return asyncJob metadata from execute().",
            tool.name
        ));
    }

    Ok(result)
}

fn run_complete(
    tool: &ChatToolDefinition,
    args: &ToolExecutionContext<'_>,
    prepared: &ChatToolPreparedInput,
    result: &ChatToolResult,
) -> Result<ChatToolCompletionResult, String> {
    (tool.complete)(crate::tools::types::ChatToolCompleteRequest {
        call_id: args.call_id,
        history: args.history,
        message: args.message,
        prepared,
        result,
        runtime_context: args.runtime_context.clone(),
        services: args.services.clone(),
    })
}

pub fn execute_tool_call(args: ToolExecutionContext<'_>) -> ExecutedToolCall {
    let Some(tool) = args.tool else {
        return build_failure_result(
            args.call_id,
            &format!(
                "Tool \"{}\" is not available in the loaded tools folder.",
                args.tool_name
            ),
            args.raw_input,
            args.tool_name,
        );
    };

    let result = (|| -> Result<ExecutedToolCall, String> {
        let prepared = run_prepare(tool, &args)?;
        let result = run_execute(tool, &args, prepared.clone())?;
        let completion = run_complete(tool, &args, &prepared, &result)?;

        let output_text = [
            result.output_text.clone().unwrap_or_default(),
            result
                .async_job
                .as_ref()
                .map(|job| format_async_job_output(&tool.name, job))
                .unwrap_or_default(),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

        let tool_call_block = build_tool_call_block(
            args.call_id,
            completion.tags.clone(),
            &prepared.display_input,
            &output_text,
            result
                .status
                .or(result
                    .async_job
                    .as_ref()
                    .map(|job| async_job_status_to_tool_call_status(job))),
            merge_tags(&[prepared.tags.clone(), result.tags.clone()]),
            &tool.name,
        );

        let completion_blocks = completion.blocks.clone().unwrap_or_default();
        let completion_blocks = add_tool_tags(&tool.name, &completion_blocks);
        let fallback_tool_content = [
            output_text.clone(),
            serialize_blocks(&completion_blocks),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

        Ok(ExecutedToolCall {
            blocks: [vec![ChatMessageBlock::ToolCall(tool_call_block)], completion_blocks].concat(),
            tool_content: completion.tool_content.clone().unwrap_or(fallback_tool_content),
        })
    })();

    match result {
        Ok(executed) => executed,
        Err(error) => build_failure_result(args.call_id, &error, args.raw_input, args.tool_name),
    }
}

// The tool-call block status falls back to the async job's status when the
// result carries none; the job's status maps onto ToolCallStatus directly.
fn async_job_status_to_tool_call_status(job: &ChatAsyncToolJob) -> ToolCallStatus {
    match job.status {
        crate::tools::types::ChatAsyncToolJobStatus::Running => ToolCallStatus::Running,
        crate::tools::types::ChatAsyncToolJobStatus::Failed => ToolCallStatus::Failed,
        crate::tools::types::ChatAsyncToolJobStatus::Completed => ToolCallStatus::Completed,
    }
}

// Unused when callers build their own inputs; kept so the JSON-argument
// parse-failure path has a named constructor with a stable message.
pub fn parse_tool_arguments(raw_input: &str) -> Result<Value, String> {
    serde_json::from_str::<Value>(raw_input)
        .map_err(|error| format!("Failed to parse tool arguments as JSON: {}", error))
}
