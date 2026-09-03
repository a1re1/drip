// Serde port of the tool-definition contract. TS field names are already
// camelCase; Rust fields are snake_case with #[serde(rename_all = "camelCase")]
// so the wire format matches the TS types byte for byte. The TS generics
// (TInput/TResult) are erased at runtime — `prepared.input` /
// `execute` `result.data` are `serde_json::Value`, and the closures the TS
// pack passes in are typed in Rust as Box<dyn FnMut(...)> with fallible
// returns (JS `throw` -> `Err(String)`; Rust cannot panic across a tool
// boundary the way a thrown Error surfaces as a failed block).
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chat::types::{
    ChatMessage, ChatMessageBlock, ChatRuntimeContext,
};

// export type ChatToolParameters = {
//   additionalProperties?: boolean; properties: Record<string, unknown>;
//   required?: string[]; type: "object";
// };
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatToolParameters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_properties: Option<bool>,
    pub properties: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
    #[serde(rename = "type")]
    pub tool_type: String,
}

impl ChatToolParameters {
    // Schema construction helper: `type: "object"` is the only allowed value,
    // so every TS literal fills it in identically.
    pub fn object() -> Self {
        Self {
            additional_properties: None,
            properties: BTreeMap::new(),
            required: None,
            tool_type: "object".to_string(),
        }
    }
}

// export type ChatToolMode = "async" | "sync";
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChatToolMode {
    #[serde(rename = "async")]
    Async,
    #[serde(rename = "sync")]
    Sync,
}

impl Default for ChatToolMode {
    fn default() -> Self {
        Self::Sync
    }
}

// export type ChatAsyncToolJobStatus = "completed" | "failed" | "running";
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChatAsyncToolJobStatus {
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "running")]
    Running,
}

// export type ChatAsyncToolJob = { command?, cwd, error?, exitCode?: number | null,
//   finishedAt?, id, logPath, startedAt, status, title, toolName } — camelCase wire format.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAsyncToolJob {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `exitCode?: number | null` — absent while running, null when the
    /// process ended without a status, a number otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<Option<i32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub id: String,
    pub log_path: String,
    pub started_at: String,
    pub status: ChatAsyncToolJobStatus,
    pub title: String,
    pub tool_name: String,
}

impl ChatAsyncToolJob {
    /// TS cloneJob(job) — a structural copy.
    pub fn clone_job(&self) -> Self {
        self.clone()
    }

    pub fn is_running(&self) -> bool {
        self.status == ChatAsyncToolJobStatus::Running
    }
}

// export type ChatAsyncToolTailResult = { job, lines, output }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAsyncToolTailResult {
    pub job: ChatAsyncToolJob,
    pub lines: i64,
    pub output: String,
}

// export type ChatAsyncToolWaitResult = { completed, job }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAsyncToolWaitResult {
    pub completed: bool,
    pub job: ChatAsyncToolJob,
}

// export type ChatAsyncToolLogger = { append(text), jobId, line(text), logPath, log(text) }
pub trait ChatAsyncToolLogger: Send + Sync {
    fn append(&self, text: &str) -> anyhow::Result<()>;
    fn line(&self, text: &str) -> anyhow::Result<()>;
    fn log(&self, text: &str) -> anyhow::Result<()>;
    fn job_id(&self) -> &str;
    fn log_path(&self) -> &str;
}

// export type ChatTmuxSession = { attachCommand, cwd, jobId, killCommand,
//   sessionName, startedAt, title, toolName }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatTmuxSession {
    pub attach_command: String,
    pub cwd: String,
    pub job_id: String,
    pub kill_command: String,
    pub session_name: String,
    pub started_at: String,
    pub title: String,
    pub tool_name: String,
}

// export type ChatTmuxSessionRuntime = { getSession, listSessions, registerSession }
pub trait ChatTmuxSessionRuntime: Send + Sync {
    fn get_session(&self, session_name: &str) -> Option<ChatTmuxSession>;
    fn list_sessions(&self) -> Vec<ChatTmuxSession>;
    fn register_session(&self, session: ChatTmuxSession);
}

// export type ChatAsyncToolCommandRequest = { args?, command, cwd?, env?, title?, toolName }
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatAsyncToolCommandRequest {
    pub args: Option<Vec<String>>,
    pub command: String,
    pub cwd: Option<String>,
    pub env: Option<std::collections::BTreeMap<String, String>>,
    pub title: Option<String>,
    pub tool_name: String,
}

// export type ChatAsyncToolTaskRequest = { cwd?, run(logger), title?, toolName }
pub struct ChatAsyncToolTaskRequest {
    pub cwd: Option<String>,
    pub run: Box<dyn FnOnce(&dyn ChatAsyncToolLogger) -> anyhow::Result<()> + Send>,
    pub title: Option<String>,
    pub tool_name: String,
}

// export type ChatAsyncToolRuntime = { getJob, startCommand, startTask, tailJob, waitForJob }
pub trait ChatAsyncToolRuntime: Send + Sync {
    fn get_job(&self, job_id: &str) -> Option<ChatAsyncToolJob>;
    fn start_command(&self, request: ChatAsyncToolCommandRequest) -> anyhow::Result<ChatAsyncToolJob>;
    fn start_task(&self, request: ChatAsyncToolTaskRequest) -> anyhow::Result<ChatAsyncToolJob>;
    /// tailJob(jobId, lines = 60)
    fn tail_job(&self, job_id: &str, lines: Option<i64>) -> anyhow::Result<ChatAsyncToolTailResult>;
    /// waitForJob(jobId, timeoutMs = 60_000)
    fn wait_for_job(&self, job_id: &str, timeout_ms: Option<i64>) -> anyhow::Result<ChatAsyncToolWaitResult>;
}

// export type ChatToolRuntimeServices = { asyncJobs, tmuxSessions }
#[derive(Clone)]
pub struct ChatToolRuntimeServices {
    pub async_jobs: std::sync::Arc<dyn ChatAsyncToolRuntime>,
    pub tmux_sessions: std::sync::Arc<dyn ChatTmuxSessionRuntime>,
}

// export type ChatToolPreparedInput<TInput> = { displayInput, input, tags? }
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatToolPreparedInput {
    pub display_input: String,
    pub input: Value,
    /// tags?: ChatTag[] — optional display tags attached by prepare.
    pub tags: Option<Vec<crate::chat::types::ChatTag>>,
}

// export type ChatToolResult<TResult> = { asyncJob?, data?, error?, outputText?,
//   status?, tags? }
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatToolResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub async_job: Option<ChatAsyncToolJob>,
    pub data: Option<Value>,
    pub error: Option<String>,
    pub output_text: Option<String>,
    /// status?: ToolCallStatus — optional override surfaced on the tool-call block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::chat::types::ToolCallStatus>,
    /// tags?: ChatTag[] — merged with prepared.tags by the execute stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<crate::chat::types::ChatTag>>,
}

// export type ChatToolCompletionResult = { blocks?, toolContent? }
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatToolCompletionResult {
    pub blocks: Option<Vec<ChatMessageBlock>>,
    pub tool_content: Option<String>,
    pub tags: Option<Vec<crate::chat::types::ChatTag>>,
}

// export type ChatToolPrepareRequest = {
//   callId, history, message, rawInput, runtimeContext, services }
pub struct ChatToolPrepareRequest<'a> {
    pub call_id: &'a str,
    pub history: &'a [ChatMessage],
    pub message: &'a ChatMessage,
    pub raw_input: &'a str,
    pub runtime_context: ChatRuntimeContext,
    pub services: ChatToolRuntimeServices,
}

// export type ChatToolExecuteRequest<TInput> = {
//   callId, history, message, prepared, runtimeContext, services }
pub struct ChatToolExecuteRequest<'a> {
    pub call_id: &'a str,
    pub history: &'a [ChatMessage],
    pub message: &'a ChatMessage,
    pub prepared: ChatToolPreparedInput,
    pub runtime_context: ChatRuntimeContext,
    pub services: ChatToolRuntimeServices,
}

// export type ChatToolCompleteRequest<TInput, TResult> = {
//   callId, history, message, prepared, result, runtimeContext, services }
pub struct ChatToolCompleteRequest<'a> {
    pub call_id: &'a str,
    pub history: &'a [ChatMessage],
    pub message: &'a ChatMessage,
    pub prepared: &'a ChatToolPreparedInput,
    pub result: &'a ChatToolResult,
    pub runtime_context: ChatRuntimeContext,
    pub services: ChatToolRuntimeServices,
}

// export type ChatToolDefinition<TInput, TResult> = { ... } — the three
// lifecycle stages are boxed closures over `Value`, erasing the TS generics.
// TS may hand back a Promise from any stage; Rust always returns fallibly, so
// the harness treats `Err` exactly like a JS throw and `Ok` like a return
// value (async tools may additionally spawn work via services.asyncJobs).
impl std::fmt::Debug for ChatToolDefinition {
    // The stage closures are opaque; name + mode are what tests and logs need.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatToolDefinition")
            .field("name", &self.name)
            .field("mutates_workspace", &self.mutates_workspace)
            .finish_non_exhaustive()
    }
}

pub struct ChatToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: ChatToolParameters,
    /// mutatesWorkspace?: boolean — declares progress for stall accounting.
    pub mutates_workspace: bool,
    pub mode: ChatToolMode,
    pub prepare:
        Box<dyn Fn(ChatToolPrepareRequest<'_>) -> Result<ChatToolPreparedInput, String>>,
    pub execute: Box<
        dyn Fn(ChatToolExecuteRequest<'_>) -> Result<ChatToolResult, String>,
    >,
    pub complete: Box<
        dyn Fn(ChatToolCompleteRequest<'_>) -> Result<ChatToolCompletionResult, String>,
    >,
}

// export function defineSyncTool(definition) — normalizes mode to "sync".
pub fn define_sync_tool(definition: ChatToolDefinition) -> ChatToolDefinition {
    let mut definition = definition;
    definition.mode = ChatToolMode::Sync;
    definition
}

// export function defineAsyncTool(definition) — normalizes mode to "async".
pub fn define_async_tool(definition: ChatToolDefinition) -> ChatToolDefinition {
    let mut definition = definition;
    definition.mode = ChatToolMode::Async;
    definition
}

// export type ChatToolsModule = { default?, tools? } — drip only loads the
// built-in pack, so the dynamic module shape is an (Option, Option) pair.
pub struct ChatToolsModule {
    pub default: Option<Vec<ChatToolDefinition>>,
    pub tools: Option<Vec<ChatToolDefinition>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object_parameters() -> ChatToolParameters {
        let mut parameters = ChatToolParameters::object();
        parameters.properties.insert(
            "label".to_string(),
            serde_json::json!({ "type": "string" }),
        );
        parameters.required = Some(vec!["label".to_string()]);
        parameters
    }

    // Every definition carries the shared field set from types.ts:145-151
    // (mutatesWorkspace comment), name, parameters, prepare.
    fn stub_definition() -> ChatToolDefinition {
        ChatToolDefinition {
            name: "STUB".to_string(),
            description: "Stub tool.".to_string(),
            parameters: object_parameters(),
            mutates_workspace: false,
            mode: ChatToolMode::Sync,
            prepare: Box::new(|request| {
                Ok(ChatToolPreparedInput {
                    display_input: request.raw_input.to_string(),
                    input: serde_json::from_str(request.raw_input).unwrap_or(Value::Null),
                    ..Default::default()
                })
            }),
            execute: Box::new(|request| {
                Ok(ChatToolResult {
                    data: Some(request.prepared.input.clone()),
                    error: None,
                    output_text: Some(request.prepared.display_input.clone()),
                    async_job: None,
                    ..Default::default()
                })
            }),
            complete: Box::new(|request| {
                Ok(ChatToolCompletionResult {
                    tags: None,
                    blocks: Some(vec![]),
                    tool_content: request
                        .result
                        .output_text
                        .clone(),
                })
            }),
        }
    }

    #[test]
    fn chat_tool_parameters_round_trips_the_wire_shape() {
        let parameters = object_parameters();
        let encoded = serde_json::to_value(&parameters).unwrap();

        // The TS type literally spells `type: "object"` (types.ts:8); the
        // serde rename means the serialized shape is identical.
        assert_eq!(
            encoded,
            serde_json::json!({
                "properties": { "label": { "type": "string" } },
                "required": ["label"],
                "type": "object"
            })
        );

        let decoded: ChatToolParameters = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, parameters);
        assert!(decoded.additional_properties.is_none());
    }

    #[test]
    fn chat_async_tool_job_round_trips_optional_fields() {
        let job = ChatAsyncToolJob {
            command: None,
            cwd: "/tmp/project".to_string(),
            error: None,
            exit_code: None,
            finished_at: None,
            id: "job-1".to_string(),
            log_path: "/tmp/logs/job-1.log".to_string(),
            started_at: "2026-09-01T00:00:00.000Z".to_string(),
            status: ChatAsyncToolJobStatus::Running,
            title: "demo".to_string(),
            tool_name: "DEMO_ASYNC".to_string(),
        };

        let encoded = serde_json::to_value(&job).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "cwd": "/tmp/project",
                "id": "job-1",
                "logPath": "/tmp/logs/job-1.log",
                "startedAt": "2026-09-01T00:00:00.000Z",
                "status": "running",
                "title": "demo",
                "toolName": "DEMO_ASYNC"
            })
        );
        assert_eq!(job.clone_job(), job);
        // A fresh job with no settled fields is still running.
        assert!(job.is_running());
    }

    #[test]
    fn chat_async_tool_job_status_serializes_as_the_ts_union() {
        for (status, encoded) in [
            (ChatAsyncToolJobStatus::Completed, "\"completed\""),
            (ChatAsyncToolJobStatus::Failed, "\"failed\""),
            (ChatAsyncToolJobStatus::Running, "\"running\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), encoded);
            let decoded: ChatAsyncToolJobStatus =
                serde_json::from_str(encoded).unwrap();
            assert_eq!(decoded, status);
        }
    }

    #[test]
    fn define_sync_tool_normalizes_the_mode() {
        // harness-test-utils.ts builds every fixture with defineSyncTool;
        // mode must come out "sync" even if the struct was built otherwise.
        let mut definition = stub_definition();
        definition.mode = ChatToolMode::Async;
        let definition = define_sync_tool(definition);

        assert_eq!(definition.mode, ChatToolMode::Sync);
        assert_eq!(
            serde_json::to_string(&definition.mode).unwrap(),
            "\"sync\""
        );
    }

    #[test]
    fn define_async_tool_normalizes_the_mode() {
        let definition = define_async_tool(stub_definition());

        assert_eq!(definition.mode, ChatToolMode::Async);
    }

    #[test]
    fn sync_tool_stages_run_prepare_execute_complete() {
        // Mirrors inspectTool in test/harness-test-utils.ts:7 — the three
        // stages pass prepared input through execute to complete.
        let mut definition = stub_definition();
        definition.name = "INSPECT".to_string();
        definition.parameters = {
            let mut parameters = ChatToolParameters::object();
            parameters.properties.insert(
                "path".to_string(),
                serde_json::json!({ "type": "string" }),
            );
            parameters.required = Some(vec!["path".to_string()]);
            parameters
        };
        definition.prepare = Box::new(|request| {
            let input: Value = serde_json::from_str(request.raw_input)
                .map_err(|error| error.to_string())?;
            Ok(ChatToolPreparedInput {
                display_input: input["path"].as_str().unwrap_or_default().to_string(),
                input,
                ..Default::default()
            })
        });

        let services = ChatToolRuntimeServices {
            async_jobs: std::sync::Arc::from(StubAsyncJobs),
            tmux_sessions: std::sync::Arc::from(StubTmuxSessions),
        };
        let message = ChatMessage {
            blocks: vec![],
            context_files: None,
            context_state: None,
            created_at: None,
            failed: None,
            id: "message-1".to_string(),
            pending: None,
            reply_to_message_id: None,
            role: crate::chat::types::ChatRole::User,
            tags: None,
            transport_state: None,
        };
        let runtime_context = ChatRuntimeContext {
            cwd: "/tmp/project".to_string(),
            working_file: crate::chat::types::WorkingFileContext {
                exists: false,
                path: "/tmp/project".to_string(),
                scope: crate::chat::types::WorkingFileScope::Cwd,
                text: None,
            },
        };

        let prepared = (definition.prepare)(ChatToolPrepareRequest {
            call_id: "call-1",
            history: &[],
            message: &message,
            raw_input: "{\"path\":\"src/a.ts\"}",
            runtime_context: runtime_context.clone(),
            services: services.clone(),
        })
        .unwrap();
        assert_eq!(prepared.display_input, "src/a.ts");

        let result = (definition.execute)(ChatToolExecuteRequest {
            call_id: "call-1",
            history: &[],
            message: &message,
            prepared: prepared.clone(),
            runtime_context: runtime_context.clone(),
            services: services.clone(),
        })
        .unwrap();
        assert_eq!(result.output_text.as_deref(), Some("src/a.ts"));

        let completion = (definition.complete)(ChatToolCompleteRequest {
            call_id: "call-1",
            history: &[],
            message: &message,
            prepared: &prepared,
            result: &result,
            runtime_context,
            services,
        })
        .unwrap();
        assert_eq!(completion.tool_content.as_deref(), Some("src/a.ts"));
        assert!(completion.blocks.unwrap().is_empty());
    }

    #[test]
    fn a_throwing_stage_surfaces_as_an_error_result() {
        // Mirrors failingTool in test/harness-test-utils.ts:87 — execute
        // throws; the Rust port surfaces the same shape via Err.
        let mut definition = stub_definition();
        definition.name = "FAILS".to_string();
        definition.execute = Box::new(|_request| {
            Err("tool exploded".to_string())
        });

        let prepared = ChatToolPreparedInput {
            display_input: "{}".to_string(),
            input: serde_json::json!({}),
            ..Default::default()
        };

        let error = (definition.execute)(ChatToolExecuteRequest {
            call_id: "call-1",
            history: &[],
            message: &ChatMessage {
                blocks: vec![],
                context_files: None,
                context_state: None,
                created_at: None,
                failed: None,
                id: "message-1".to_string(),
                pending: None,
                reply_to_message_id: None,
                role: crate::chat::types::ChatRole::User,
                tags: None,
                transport_state: None,
            },
            prepared,
            runtime_context: ChatRuntimeContext {
                cwd: "/tmp/project".to_string(),
                working_file: crate::chat::types::WorkingFileContext {
                    exists: false,
                    path: "/tmp/project".to_string(),
                    scope: crate::chat::types::WorkingFileScope::Cwd,
                    text: None,
                },
            },
            services: ChatToolRuntimeServices {
                async_jobs: std::sync::Arc::from(StubAsyncJobs),
                tmux_sessions: std::sync::Arc::from(StubTmuxSessions),
            },
        })
        .unwrap_err();

        assert_eq!(error, "tool exploded");
    }

    #[test]
    fn chat_tools_module_holds_default_and_named_tool_lists() {
        // ChatToolsModule = { default?, tools? } (types.ts:183-186); drip
        // only loads the built-in pack, so both arms stay optional.
        let module = ChatToolsModule {
            default: None,
            tools: Some(vec![]),
        };

        assert!(module.default.is_none());
        assert!(module.tools.unwrap().is_empty());
    }

    struct StubAsyncJobs;

    impl ChatAsyncToolRuntime for StubAsyncJobs {
        fn get_job(&self, _job_id: &str) -> Option<ChatAsyncToolJob> {
            None
        }

        fn start_command(
            &self,
            _request: ChatAsyncToolCommandRequest,
        ) -> anyhow::Result<ChatAsyncToolJob> {
            anyhow::bail!("stub async runtime has no jobs")
        }

        fn start_task(
            &self,
            _request: ChatAsyncToolTaskRequest,
        ) -> anyhow::Result<ChatAsyncToolJob> {
            anyhow::bail!("stub async runtime has no jobs")
        }

        fn tail_job(&self, _job_id: &str, _lines: Option<i64>) -> anyhow::Result<ChatAsyncToolTailResult> {
            anyhow::bail!("stub async runtime has no jobs")
        }

        fn wait_for_job(&self, _job_id: &str, _timeout_ms: Option<i64>) -> anyhow::Result<ChatAsyncToolWaitResult> {
            anyhow::bail!("stub async runtime has no jobs")
        }
    }

    struct StubTmuxSessions;

    impl ChatTmuxSessionRuntime for StubTmuxSessions {
        fn get_session(&self, _session_name: &str) -> Option<ChatTmuxSession> {
            None
        }

        fn list_sessions(&self) -> Vec<ChatTmuxSession> {
            Vec::new()
        }

        fn register_session(&self, _session: ChatTmuxSession) {}
    }
}
