// port of src/chat/types.ts
//
// Only the types + serialization helpers that chat/transport.ts (and the
// harness model-call layer) need are ported. The remaining types.ts surface
// (pinned/deleted/reply/retry context helpers, ChatRuntime,
// getChatContextTargetId, editable-text accessors) is UI-facing — belongs to
// the chat runtime, not the model transport layer; not ported here. All JSON
// field names serialize camelCase-identical to the TS types.
use serde::{Deserialize, Serialize};

pub type ChatRole = ChatRoleTag;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRoleTag {
    Assistant,
    System,
    Tool,
    User,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatTag {
    Text(String),
    Detailed(ChatTagDetails),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatTagDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatContextFileLineRange {
    pub end_line: i64,
    pub start_line: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatContextFile {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_range: Option<ChatContextFileLineRange>,
    pub mention: String,
    pub path: String,
    pub relative_path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallStatus {
    Completed,
    Failed,
    Running,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessageContextState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_state: Option<ChatMessageContextState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<ChatTag>>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_state: Option<ChatMessageContextState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub status: ToolCallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<ChatTag>>,
    pub tool_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionBlock {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_state: Option<ChatMessageContextState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<ChatTag>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChatMessageBlock {
    #[serde(rename = "text")]
    Text(TextBlock),
    #[serde(rename = "tool-call")]
    ToolCall(ToolCallBlock),
    #[serde(rename = "completion")]
    Completion(CompletionBlock),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessageTransportState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_tool_calls: Option<Vec<ChatTransportToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_content: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatTransportToolCall {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    pub tool_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub blocks: Vec<ChatMessageBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_files: Option<Vec<ChatContextFile>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_state: Option<ChatMessageContextState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<bool>,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    pub role: ChatRoleTag,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<ChatTag>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_state: Option<ChatMessageTransportState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkingFileScope {
    Cwd,
    File,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkingFileContext {
    pub exists: bool,
    pub path: String,
    pub scope: WorkingFileScope,
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRuntimeContext {
    pub cwd: String,
    pub working_file: WorkingFileContext,
}

pub fn create_message(
    id: &str,
    role: ChatRoleTag,
    blocks: Vec<ChatMessageBlock>,
    pending: bool,
    tags: Option<Vec<ChatTag>>,
) -> ChatMessage {
    ChatMessage {
        blocks,
        context_files: None,
        context_state: None,
        created_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        failed: None,
        id: id.to_string(),
        pending: Some(pending),
        reply_to_message_id: None,
        role,
        tags,
        transport_state: None,
    }
}

pub fn serialize_chat_message_block(block: &ChatMessageBlock) -> String {
    match block {
        ChatMessageBlock::Text(block) => block.text.clone(),
        ChatMessageBlock::ToolCall(block) => {
            let lines = [
                Some(format!("tool {} {}", block.tool_name, tool_call_status_string(block.status))),
                block
                    .input
                    .as_ref()
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("input: {value}")),
                block
                    .output
                    .as_ref()
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("output: {value}")),
            ];
            lines.into_iter().flatten().collect::<Vec<_>>().join("\n")
        }
        ChatMessageBlock::Completion(block) => {
            let lines = [
                Some(format!("completion {}", block.path.as_deref().unwrap_or("")).trim().to_string()),
                block.description.clone().filter(|value| !value.is_empty()),
                Some(block.code.clone()).filter(|value| !value.is_empty()),
            ];
            lines.into_iter().flatten().collect::<Vec<_>>().join("\n")
        }
    }
}

pub fn tool_call_status_string(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
        ToolCallStatus::Running => "running",
    }
}

pub fn serialize_chat_message_blocks(blocks: &[ChatMessageBlock]) -> String {
    let sections: Vec<String> = blocks.iter().map(serialize_chat_message_block).collect();

    sections.join("\n\n").trim().to_string()
}

pub fn format_chat_context_file_label(file: &ChatContextFile) -> String {
    let Some(line_range) = &file.line_range else {
        return file.relative_path.clone();
    };

    let range_label = if line_range.start_line == line_range.end_line {
        format!("{}", line_range.start_line)
    } else {
        format!("{}:{}", line_range.start_line, line_range.end_line)
    };

    format!("{}#{}", file.relative_path, range_label)
}

pub fn serialize_chat_context_file(file: &ChatContextFile) -> String {
    [
        format!("context_file: {}", format_chat_context_file_label(file)),
        format!("context_file_text:\n{}", file.content),
    ]
    .join("\n")
}

pub fn serialize_chat_message_for_context(message: &ChatMessage) -> String {
    let mut sections = vec![serialize_chat_message_blocks(&message.blocks)];

    if let Some(context_files) = &message.context_files {
        for file in context_files {
            sections.push(serialize_chat_context_file(file));
        }
    }

    let sections: Vec<String> = sections
        .into_iter()
        .filter(|section| !section.trim().is_empty())
        .collect();

    sections.join("\n\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_tool_call_block_like_ts() {
        let block = ChatMessageBlock::ToolCall(ToolCallBlock {
            context_state: None,
            input: Some("{\"path\":\"a.txt\"}".to_string()),
            label: None,
            output: Some("ok".to_string()),
            status: ToolCallStatus::Completed,
            tags: None,
            tool_name: "READ".to_string(),
        });

        assert_eq!(
            serialize_chat_message_block(&block),
            "tool READ completed\ninput: {\"path\":\"a.txt\"}\noutput: ok"
        );
    }

    #[test]
    fn serializes_message_with_context_files() {
        let message = ChatMessage {
            blocks: vec![ChatMessageBlock::Text(TextBlock {
                context_state: None,
                tags: None,
                text: "hello".to_string(),
            })],
            context_files: Some(vec![ChatContextFile {
                content: "line1".to_string(),
                line_range: Some(ChatContextFileLineRange { end_line: 4, start_line: 2 }),
                mention: "@src/a.ts".to_string(),
                path: "/abs/src/a.ts".to_string(),
                relative_path: "src/a.ts".to_string(),
            }]),
            context_state: None,
            created_at: None,
            failed: None,
            id: "m1".to_string(),
            pending: None,
            reply_to_message_id: None,
            role: ChatRoleTag::User,
            tags: None,
            transport_state: None,
        };

        assert_eq!(
            serialize_chat_message_for_context(&message),
            "hello\n\ncontext_file: src/a.ts#2:4\ncontext_file_text:\nline1"
        );
    }
}
