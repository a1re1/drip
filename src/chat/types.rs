// port of src/chat/types.ts
//
// Only the types tools/types.ts and its sibling modules import are ported
// here (ChatRole, ChatTag, block/message/context types, ChatRuntimeContext).
// The helper functions at the bottom of the TS file (serializeChatMessage*,
// getChatMessageForContext, …) belong to the harness/UI layer and are
// ported when their consumers land.
//
// Field names serialize exactly as the TypeScript field names (already
// camelCase): Rust fields are snake_case + #[serde(rename_all =
// "camelCase")]. Optional fields are Option<T> + skip_serializing_if =
// "Option::is_none" — JSON.stringify drops `undefined` keys, and the port
// must match that. Fields typed `string | null` in TS keep their null
// (Option with #[serde(default)], no skip).

use serde::{Deserialize, Serialize};

// export type ChatRole = "assistant" | "system" | "tool" | "user"
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChatRole {
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "system")]
    System,
    #[serde(rename = "tool")]
    Tool,
    #[serde(rename = "user")]
    User,
}

// export type ToolCallStatus = "completed" | "failed" | "running"
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ToolCallStatus {
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "running")]
    Running,
}

// export type ChatTag = string | { group?, id, key?, label? }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatTag {
    Text(String),
    Structured {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

// export type ChatMessageContextState = { deleted?, editedText?, pinned? }
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessageContextState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
}

// export type ChatContextFileLineRange = { endLine, startLine }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatContextFileLineRange {
    pub end_line: usize,
    pub start_line: usize,
}

// export type ChatContextFile = {
//   content, lineRange?, mention, path, relativePath
// }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatContextFile {
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_range: Option<ChatContextFileLineRange>,
    pub mention: String,
    pub path: String,
    pub relative_path: String,
}

// export type TextBlock = { contextState?, tags?, text, type: "text" }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_state: Option<ChatMessageContextState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<ChatTag>>,
    pub text: String,
}

// export type ToolCallBlock = {
//   contextState?, input?, label?, output?, status, tags?, toolName, type: "tool-call"
// }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_state: Option<ChatMessageContextState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub status: ToolCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<ChatTag>>,
    pub tool_name: String,
}

// export type CompletionBlock = {
//   code, contextState?, description?, language?, path?, tags?, type: "completion"
// }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionBlock {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_state: Option<ChatMessageContextState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<ChatTag>>,
}

// export type ChatMessageBlock = CompletionBlock | TextBlock | ToolCallBlock
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

// export type ChatTransportToolCall = { id, input?, toolName }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatTransportToolCall {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    pub tool_name: String,
}

// export type ChatMessageTransportState = {
//   assistantToolCalls?, toolCallId?, toolContent?, toolName?, transportContent?
// }
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessageTransportState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_tool_calls: Option<Vec<ChatTransportToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_content: Option<String>,
}

// export type ChatMessage = {
//   blocks, contextFiles?, contextState?, createdAt?, failed?, id, pending?,
//   replyToMessageId?, role, tags?, transportState?
// }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub blocks: Vec<ChatMessageBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_files: Option<Vec<ChatContextFile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_state: Option<ChatMessageContextState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<bool>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    pub role: ChatRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<ChatTag>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_state: Option<ChatMessageTransportState>,
}

// export type WorkingFileContext = { exists, path, scope: "cwd" | "file", text }
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorkingFileScope {
    #[serde(rename = "cwd")]
    Cwd,
    #[serde(rename = "file")]
    File,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkingFileContext {
    pub exists: bool,
    pub path: String,
    pub scope: WorkingFileScope,
    // `text: string | null` is always present in TS — None serializes as null.
    #[serde(default)]
    pub text: Option<String>,
}

// export type ChatRuntimeContext = { cwd, workingFile }
// Runtime-only value (never serialized); plain struct.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatRuntimeContext {
    pub cwd: String,
    pub working_file: WorkingFileContext,
}
