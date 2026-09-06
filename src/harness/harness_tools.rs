// Harness tool definitions and argument parsing.
//
// This module holds HARNESS_TOOL_SPECS (as harness_tool_definitions),
// is_harness_tool, HarnessRoleGate, DEFAULT_MAX_REVIEW_ROUNDS, and the pure
// argument extraction/validation of a harness tool call
// (parse_harness_op): JSON-decoding plus per-branch field coercion.
// The state-mutating op handlers (add/drop/finish/... and their result_text
// strings) live below as apply_harness_op.
//
// The definitions are byte-identical to the fixture
// drip/tests/fixtures/harness-tools.json — see
// drip/tests/harness_tools_schema_parity.rs.

use std::collections::HashMap;

use serde_json::json;

/// Default cap on review rounds before a rejected task is blocked.
pub const DEFAULT_MAX_REVIEW_ROUNDS: u32 = 2;

/// A named role (see roles.rs), optionally one whose completed work must be
/// verified by another role.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HarnessRoleSpec {
    /// verifiedBy: id of the role whose review must confirm this role's work.
    pub verified_by: Option<String>,
}

/// The role configuration a run enforces.
#[derive(Debug, Clone, Default)]
pub struct HarnessRoleGate {
    pub roles: HashMap<String, HarnessRoleSpec>,
    pub max_review_rounds: Option<u32>,
    /// defaultTaskRole: role assumed for finished tasks that carry no role.
    pub default_task_role: Option<String>,
}

/// One planned-task entry as parsed from a plan_tasks call.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessTaskInput {
    pub title: String,
    pub role: Option<String>,
    pub depends_on: Vec<String>,
}

/// The `status` argument of finish_task ("completed" | "blocked").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishTaskStatus {
    Completed,
    Blocked,
}

impl FinishTaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FinishTaskStatus::Completed => "completed",
            FinishTaskStatus::Blocked => "blocked",
        }
    }
}

/// The `scope` argument of remember/forget ("session" | "repo").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    Session,
    Repo,
}

/// One parsed harness tool call: one variant per harness tool, carrying the
/// arguments applyHarnessToolCall's matching branch would use. Field
/// extraction is per-branch (`typeof input.x === "string" ? input.x :
/// <default>`); the state-dependent emptiness checks ("Provide the note
/// text.", ...) stay in the op handlers.
#[derive(Debug, Clone, PartialEq)]
pub enum HarnessOp {
    /// plan_tasks: entries from parsePlannedTasks, placement "next"|"end".
    PlanTasks {
        entries: Vec<HarnessTaskInput>,
        placement: String,
        /// Roles stripped because they are not in the role gate (reported back
        /// to the model by the handler).
        unknown_roles: Vec<String>,
    },
    /// drop_task
    DropTask { task_id: String, reason: String },
    /// revise_task
    ReviseTask { task_id: String, title: String },
    /// finish_task
    FinishTask {
        status: FinishTaskStatus,
        summary: String,
        task_id: Option<String>,
    },
    /// respond
    Respond { text: String },
    /// observe
    Observe { note: String, ttl: Option<f64> },
    /// recall
    Recall { tool_name: String, query: String },
    /// note_task
    NoteTask { note: String, task_id: Option<String> },
    /// remember
    Remember {
        scope: MemoryScope,
        note: String,
        topic: Option<String>,
        hook: Option<String>,
    },
    /// forget
    Forget { scope: MemoryScope, note_id: String },
}

/// The 10 harness (framework) tool definitions as OpenAI function-call
/// specs, in HARNESS_TOOL_SPECS order (alphabetical by function name — the
/// same order the fixture dumps them in).
pub fn harness_tool_definitions() -> Vec<serde_json::Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "plan_tasks",
                "description": "Add new tasks to the shared todo list. Use small, concrete tasks that a single task loop can finish.",
                "parameters": {
                    "properties": {
                        "placement": {
                            "description": "Where to insert the new tasks: \"next\" places them immediately after the current task, before other pending work; \"end\" (default) appends them.",
                            "enum": ["end", "next"],
                            "type": "string"
                        },
                        "tasks": {
                            "description": "Tasks to append to the todo list, in execution order.",
                            "items": {
                                "anyOf": [
                                    {
                                        "description": "A task title (worked in list order under the default role).",
                                        "type": "string"
                                    },
                                    {
                                        "additionalProperties": false,
                                        "properties": {
                                            "dependsOn": {
                                                "description": "Ids of tasks (e.g. task-2) that must complete or drop before this task becomes workable. Use for work that genuinely cannot start earlier — not for ordinary ordering, which list position already expresses.",
                                                "items": {
                                                    "type": "string"
                                                },
                                                "type": "array"
                                            },
                                            "title": {
                                                "type": "string"
                                            }
                                        },
                                        "required": ["title"],
                                        "type": "object"
                                    }
                                ]
                            },
                            "type": "array"
                        }
                    },
                    "required": ["tasks"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "drop_task",
                "description": "Drop a task from the todo list when new information makes it unnecessary, redundant, or wrong. The task stays visible as dropped with your reason.",
                "parameters": {
                    "properties": {
                        "reason": {
                            "description": "Why this task is no longer needed.",
                            "type": "string"
                        },
                        "taskId": {
                            "type": "string"
                        }
                    },
                    "required": ["taskId", "reason"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "revise_task",
                "description": "Rewrite a task's title when what you have learned changed what the task should actually do. Keeps its id, status, and notes.",
                "parameters": {
                    "properties": {
                        "taskId": {
                            "type": "string"
                        },
                        "title": {
                            "description": "The new task title.",
                            "type": "string"
                        }
                    },
                    "required": ["taskId", "title"],
                    "type": "object"
                }
            }
        }),        json!({
            "type": "function",
            "function": {
                "name": "finish_task",
                "description": "Finish the current task (or the task named by taskId). Use status completed with a summary of what was done, or status blocked with a summary of why and what is needed.",
                "parameters": {
                    "properties": {
                        "status": {
                            "enum": ["blocked", "completed"],
                            "type": "string"
                        },
                        "summary": {
                            "description": "What was done, or why the task is blocked and what would unblock it.",
                            "type": "string"
                        },
                        "taskId": {
                            "description": "Optional task id. Defaults to the current task.",
                            "type": "string"
                        }
                    },
                    "required": ["status", "summary"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "respond",
                "description": "Answer the goal directly and complete the run, when the goal is a question or asks for a status report and needs no workspace changes. The text is delivered to the user verbatim as the run's result. Not allowed while unfinished tasks exist.",
                "parameters": {
                    "properties": {
                        "text": {
                            "description": "The complete answer or report, in plain markdown.",
                            "type": "string"
                        }
                    },
                    "required": ["text"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "observe",
                "description": "Save a short-lived observation for the next few task loops: a failing check's detail, an in-flight hypothesis, a result being verified. Observations expire after a few loops unless re-observed (which refreshes their ttl). Use remember instead for durable facts.",
                "parameters": {
                    "properties": {
                        "note": {
                            "type": "string"
                        },
                        "ttl": {
                            "description": "Optional number of task loops to keep the observation alive (default 4, capped).",
                            "type": "number"
                        }
                    },
                    "required": ["note"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "recall",
                "description": "Recover the cached output of an earlier tool call this run (telemetry keeps an ends-kept copy of each call's last result) instead of re-running it. Match by tool name plus a fragment of the original input.",
                "parameters": {
                    "properties": {
                        "query": {
                            "description": "A fragment of the original call's input (a path, a command substring) to pick the right record.",
                            "type": "string"
                        },
                        "toolName": {
                            "description": "The workspace tool whose earlier result you need (e.g. READ, BASH, GREP).",
                            "type": "string"
                        }
                    },
                    "required": ["toolName", "query"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "note_task",
                "description": "Append a progress note to a task (defaults to the current task) without ending the loop — partial findings, decisions, or where you left off. Use finish_task when the task is actually done or blocked.",
                "parameters": {
                    "properties": {
                        "note": {
                            "type": "string"
                        },
                        "taskId": {
                            "description": "Optional task id. Defaults to the current task.",
                            "type": "string"
                        }
                    },
                    "required": ["note"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "remember",
                "description": "Save a note to shared memory so future task loops can see it. Use for durable facts, decisions, and discovered constraints that matter for the rest of the run; for short-lived findings use observe.\n\nOptional scope parameter:\n- 'session' (default): saves to in-memory state for this run only.\n- 'repo': persists the note into the project's memory bank stored under ~/.drip/projects/<project>/memory (outside the repo tree) so future sessions see it. Requires topic (a short title/slug for the page) and note. Optionally provide hook (one-line summary for the index). Use repo scope for durable, repo-specific knowledge: gotchas, key commands, architecture facts, persistent ideas.",
                "parameters": {
                    "properties": {
                        "note": {
                            "description": "The note text to save.",
                            "type": "string"
                        },
                        "scope": {
                            "description": "Where to save: 'session' (default, this run only) or 'repo' (persists to ~/.drip/projects/<project>/memory across sessions).",
                            "enum": ["session", "repo"],
                            "type": "string"
                        },
                        "topic": {
                            "description": "Required for scope=repo. Short title for the memory page (e.g. 'Build Commands', 'Architecture'). Used as the page filename slug.",
                            "type": "string"
                        },
                        "hook": {
                            "description": "Optional for scope=repo. One-line summary shown in the MEMORY.md index entry.",
                            "type": "string"
                        }
                    },
                    "required": ["note"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "forget",
                "description": "Remove a shared memory note by id when it is stale or wrong.\n\nOptional scope parameter:\n- 'session' (default): removes a note from in-memory state by noteId.\n- 'repo': removes a topic page from the project's memory bank stored under ~/.drip/projects/<project>/memory (outside the repo tree) by slug. Use when repo-scoped knowledge is no longer accurate.",
                "parameters": {
                    "properties": {
                        "noteId": {
                            "description": "For scope=session: the memory note id to remove. For scope=repo: the slug of the topic page to remove (e.g. 'build-commands').",
                            "type": "string"
                        },
                        "scope": {
                            "description": "Where to forget: 'session' (default) or 'repo'.",
                            "enum": ["session", "repo"],
                            "type": "string"
                        }
                    },
                    "required": ["noteId"],
                    "type": "object"
                }
            }
        }),

    ]
}

// Leading verbs that make a task title read as "change the code". Judged on
// the first word only, so "verify the added export" (verify) and "add a
// check for X" (add) land on the right side; nouns like "check" or "test"
// inside the title never count.
const BUILD_TASK_VERBS: &[&str] = &[
    "add", "append", "build", "change", "convert", "create", "delete", "extend", "extract", "fix",
    "implement", "introduce", "migrate", "move", "patch", "port", "refactor", "remove", "rename",
    "replace", "rewrite", "update", "wire", "write",
];

// Objects that make a build verb prose instead of code: "write summary",
// "update the plan" — those finish in the reply, not the workspace.
const PROSE_OBJECT_WORDS: &[&str] = &[
    "summary", "summaries", "report", "note", "notes", "answer", "reply", "response", "message",
    "findings", "plan", "user",
];

/// True when a task title starts with a code-changing verb aimed at the
/// workspace rather than at prose.
pub fn looks_like_build_task(title: &str) -> bool {
    let has_prose_object = title
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .any(|word| {
            let lower = word.to_ascii_lowercase();
            PROSE_OBJECT_WORDS.contains(&lower.as_str())
        });
    if has_prose_object {
        return false;
    }

    // Strip leading numbering/bullets, then take the first
    // whitespace-delimited word and keep its letters only.
    let stripped = title
        .trim()
        .trim_start_matches(|c: char| c.is_whitespace() || c.is_ascii_digit() || matches!(c, '.' | ')' | ':' | '(' | '-' | '*' | '•'));
    let first_word: String = stripped
        .split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_lowercase())
        .collect();

    !first_word.is_empty() && BUILD_TASK_VERBS.contains(&first_word.as_str())
}

pub fn is_harness_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "drop_task"
            | "finish_task"
            | "forget"
            | "note_task"
            | "observe"
            | "plan_tasks"
            | "recall"
            | "remember"
            | "respond"
            | "revise_task"
    )
}

// --- argument parsing (parseToolInput + the per-branch coercion) ---

/// A misleading missing-field error
/// sends weak models down the wrong repair path, so name the real problem.
fn not_valid_json(parse_error: &str) -> String {
    format!(
        "The tool arguments were not valid JSON ({parse_error}). Re-send the same call with well-formed JSON arguments."
    )
}

/// JSON-parse the raw arguments and require an object. Note: the embedded
/// parse-error detail comes from the JSON runtime, the wrapper text is ours.
fn parse_tool_input(raw_input: &str) -> Result<serde_json::Value, String> {
    match serde_json::from_str::<serde_json::Value>(raw_input) {
        Ok(parsed) if parsed.is_object() => Ok(parsed),
        Ok(_) => Err(not_valid_json("the arguments were not a JSON object")),
        Err(error) => Err(not_valid_json(&error.to_string())),
    }
}

/// Per-branch `typeof input.x === "string" ? input.x : default` coercion.
fn string_or_default(input: &serde_json::Value, key: &str, default: &str) -> String {
    match input.get(key) {
        Some(v) if v.is_string() => v.as_str().unwrap_or(default).to_string(),
        _ => default.to_string(),
    }
}

/// Optional-string coercion (`typeof input.x === "string" ? input.x : undefined`).
fn string_or_none(input: &serde_json::Value, key: &str) -> Option<String> {
    match input.get(key) {
        Some(v) if v.is_string() => Some(v.as_str().unwrap_or_default().to_string()),
        _ => None,
    }
}

/// Parses plan_tasks entries — accepts both shapes, bare
/// titles, and {title, role, dependsOn} objects when roles are configured.
/// Unknown role names are stripped (the task still lands, under the default
/// role) and reported back to the model via `unknown_roles`.
fn parse_planned_tasks(
    raw_tasks: Option<&serde_json::Value>,
    gate: Option<&HarnessRoleGate>,
) -> (Vec<HarnessTaskInput>, Vec<String>) {
    let mut entries: Vec<HarnessTaskInput> = Vec::new();
    let mut unknown_roles: Vec<String> = Vec::new();

    let Some(raw_tasks) = raw_tasks.and_then(|v| v.as_array()) else {
        return (entries, unknown_roles);
    };

    for raw_task in raw_tasks {
        if let Some(title) = raw_task.as_str() {
            entries.push(HarnessTaskInput {
                title: title.to_string(),
                role: None,
                depends_on: Vec::new(),
            });
            continue;
        }

        let Some(task_object) = raw_task.as_object() else {
            continue;
        };
        let Some(title) = task_object.get("title").and_then(|v| v.as_str()) else {
            continue;
        };

        let role = task_object
            .get("role")
            .and_then(|v| v.as_str())
            .map(|role| role.trim().to_string())
            .unwrap_or_default();
        let depends_on: Vec<String> = task_object
            .get("dependsOn")
            .and_then(|v| v.as_array())
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| id.as_str())
                    .filter(|id| !id.trim().is_empty())
                    .map(|id| id.to_string())
                    .collect()
            })
            .unwrap_or_default();

        if !role.is_empty() && !gate.map(|gate| gate.roles.contains_key(&role)).unwrap_or(false) {
            unknown_roles.push(role);
            entries.push(HarnessTaskInput {
                title: title.to_string(),
                role: None,
                depends_on,
            });
        } else {
            entries.push(HarnessTaskInput {
                title: title.to_string(),
                role: if role.is_empty() { None } else { Some(role) },
                depends_on,
            });
        }
    }

    (entries, unknown_roles)
}

/// The result of applying one harness tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessToolResult {
    pub result_text: String,
    pub state_changed: bool,
    pub task_finished: bool,
}

/// apply_review_verdict: finishing a review task is a verdict on the
/// reviewed work. Confirmation completes the review and annotates the
/// original; rejection completes the review too (its unit of work — judging —
/// is done) and sends the original back to the queue, or blocks it once its
/// review-round budget is exhausted so the run escalates to the user instead
/// of ping-ponging forever.
pub fn apply_review_verdict(
    state: &mut crate::core::types::HarnessState,
    review_task: &crate::core::types::HarnessTask,
    status: FinishTaskStatus,
    summary: &str,
    gate: Option<&HarnessRoleGate>,
) -> HarnessToolResult {
    use crate::core::state::{
        append_task_note, finish_task as finish_task_state, get_task_by_id,
        get_task_by_id_mut,
        reopen_task_for_rework, HarnessFinishArgs,
    };
    use crate::core::types::HarnessTaskStatus;

    let original = review_task
        .review_of
        .as_deref()
        .and_then(|id| get_task_by_id(state, id))
        .map(|task| (task.id.clone(), task.status, task.review_round));

    if status == FinishTaskStatus::Completed {
        finish_task_state(
            state,
            HarnessFinishArgs {
                status: HarnessTaskStatus::Completed,
                summary,
                task_id: Some(&review_task.id),
            },
        );

        let review_task_id = &review_task.id;
        if let Some((original_id, _, _)) = &original {
            if let Some(task) = get_task_by_id_mut(state, original_id) {
                append_task_note(task, &format!("Review {review_task_id} confirmed this task: {summary}"));
            }
        }

        return HarnessToolResult {
            result_text: format!(
                "Review {} confirmed {}.",
                review_task.id,
                original.as_ref().map(|(id, _, _)| id.as_str()).unwrap_or_else(|| review_task.review_of.as_deref().unwrap_or_default())
            ),
            state_changed: true,
            task_finished: true,
        };
    }

    finish_task_state(
        state,
        HarnessFinishArgs {
            status: HarnessTaskStatus::Completed,
            summary: &format!("Rejected {}: {summary}", review_task.review_of.as_deref().unwrap_or_default()),
            task_id: Some(&review_task.id),
        },
    );

    let Some((original_id, original_status, original_review_round)) = original else {
        return HarnessToolResult {
            result_text: format!(
                "Review {} recorded a rejection, but the reviewed task {} no longer exists to reopen.",
                review_task.id,
                review_task.review_of.as_deref().unwrap_or_default()
            ),
            state_changed: true,
            task_finished: true,
        };
    };

    if original_status == crate::core::types::HarnessTaskStatus::Dropped {
        return HarnessToolResult {
            result_text: format!(
                "Review {} recorded a rejection, but the reviewed task {} no longer exists to reopen.",
                review_task.id,
                review_task.review_of.as_deref().unwrap_or_default()
            ),
            state_changed: true,
            task_finished: true,
        };
    }

    let max_review_rounds = gate
        .and_then(|gate| gate.max_review_rounds)
        .unwrap_or(DEFAULT_MAX_REVIEW_ROUNDS);

    if original_review_round.unwrap_or(0) >= (max_review_rounds as i64) - 1 {
        let new_round = original_review_round.unwrap_or(0) + 1;
        // The state helper re-looks the task up by id to bump review_round.
        if let Some(task) = get_task_by_id_mut(state, &original_id) {
            task.review_round = Some(new_round);
        }
        finish_task_state(
            state,
            HarnessFinishArgs {
                status: HarnessTaskStatus::Blocked,
                summary: &format!(
                    "Review {} rejected the work (rejection {} of {}): {summary}",
                    review_task.id, new_round, max_review_rounds
                ),
                task_id: Some(&original_id),
            },
        );

        return HarnessToolResult {
            result_text: format!(
                "Review {} rejected {}. Its review budget is exhausted, so {} is now blocked — it needs a different approach or user input.",
                review_task.id, original_id, original_id
            ),
            state_changed: true,
            task_finished: true,
        };
    }

    let review_task_id = &review_task.id;
    if let Some(task) = get_task_by_id_mut(state, &original_id) {
        reopen_task_for_rework(
            task,
            &format!("Review {review_task_id} rejected the work: {summary}"),
        );
    }

    HarnessToolResult {
        result_text: format!(
            "Review {} rejected {}. The task was reopened with the review findings (rejection {} of {}).",
            review_task.id,
            original_id,
            original_review_round.unwrap_or(0) + 1,
            max_review_rounds
        ),
        state_changed: true,
        task_finished: true,
    }
}

/// The remember/forget scope coercion ("session" default).
fn parse_scope(input: &serde_json::Value) -> MemoryScope {
    if string_or_default(input, "scope", "session") == "repo" {
        MemoryScope::Repo
    } else {
        MemoryScope::Session
    }
}

/// Parse the raw JSON arguments of a harness tool call into a typed
/// HarnessOp. This is the no-role-gate entry point — plan_tasks role entries
/// are stripped into `unknown_roles` when no gate is configured. Handlers
/// with a gate should call [`parse_harness_op_with_gate`].
pub fn parse_harness_op(tool_name: &str, raw_input: &str) -> Result<HarnessOp, String> {
    parse_harness_op_with_gate(tool_name, raw_input, None)
}

/// parse_harness_op with a HarnessRoleGate (it shapes plan_tasks' entry
/// parsing).
pub fn parse_harness_op_with_gate(
    tool_name: &str,
    raw_input: &str,
    gate: Option<&HarnessRoleGate>,
) -> Result<HarnessOp, String> {
    let input = parse_tool_input(raw_input)?;

    match tool_name {
        "plan_tasks" => {
            let (entries, unknown_roles) = parse_planned_tasks(input.get("tasks"), gate);
            let placement = if string_or_default(&input, "placement", "end") == "next" {
                "next".to_string()
            } else {
                "end".to_string()
            };
            Ok(HarnessOp::PlanTasks {
                entries,
                placement,
                unknown_roles,
            })
        }
        "drop_task" => Ok(HarnessOp::DropTask {
            task_id: string_or_default(&input, "taskId", ""),
            reason: string_or_default(&input, "reason", ""),
        }),
        "revise_task" => Ok(HarnessOp::ReviseTask {
            task_id: string_or_default(&input, "taskId", ""),
            title: string_or_default(&input, "title", ""),
        }),
        "finish_task" => {
            let status = match input.get("status").and_then(|v| v.as_str()) {
                Some("blocked") => FinishTaskStatus::Blocked,
                Some("completed") => FinishTaskStatus::Completed,
                _ => {
                    return Err("The status field must be exactly \"completed\" or \"blocked\".".to_string());
                }
            };
            Ok(HarnessOp::FinishTask {
                status,
                summary: string_or_default(&input, "summary", ""),
                task_id: string_or_none(&input, "taskId"),
            })
        }
        "respond" => Ok(HarnessOp::Respond {
            text: string_or_default(&input, "text", ""),
        }),
        "observe" => Ok(HarnessOp::Observe {
            note: string_or_default(&input, "note", ""),
            ttl: match input.get("ttl") {
                Some(v) if v.is_number() => v.as_f64(),
                _ => None,
            },
        }),
        "recall" => Ok(HarnessOp::Recall {
            tool_name: string_or_default(&input, "toolName", ""),
            query: string_or_default(&input, "query", ""),
        }),
        "note_task" => Ok(HarnessOp::NoteTask {
            note: string_or_default(&input, "note", ""),
            task_id: string_or_none(&input, "taskId"),
        }),
        "remember" => Ok(HarnessOp::Remember {
            scope: parse_scope(&input),
            note: string_or_default(&input, "note", ""),
            topic: string_or_none(&input, "topic"),
            hook: string_or_none(&input, "hook"),
        }),
        "forget" => Ok(HarnessOp::Forget {
            scope: parse_scope(&input),
            note_id: string_or_default(&input, "noteId", ""),
        }),
        _ => Err(format!("Unknown harness tool \"{tool_name}\".")),
    }
}

// ---------------------------------------------------------------------------
// Repo memory bank helpers.
// File formats: the MEMORY.md index holds one line per page,
// `- [Title](slug.md) — hook`, and a topic page starts with `# <topic>` when
// first created (later notes are appended after a blank line).
// ---------------------------------------------------------------------------

use crate::core::state as core_state;

/// Filename of the memory bank index inside the memory dir.
const MEMORY_INDEX: &str = "MEMORY.md";

/// One index entry: `{ title, hook }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIndexEntry {
    pub title: String,
    pub hook: String,
}

/// Slugify a topic into a filename stem.
///
/// Lowercases, collapses every run of non-`[a-z0-9]` characters into a single
/// dash, trims leading/trailing dashes and caps the result at 80 characters
/// (falling back to "note"). "memory" is remapped to "memory-notes" because
/// "memory.md" would collide with MEMORY.md on case-insensitive filesystems.
pub fn slugify(topic: &str) -> String {
    // toLowerCase() then replace(/[^a-z0-9]+/g, "-")
    let mut replaced = String::new();
    let mut last_was_dash = true;
    for ch in topic.chars() {
        if ch.is_ascii_alphanumeric() {
            replaced.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            replaced.push('-');
            last_was_dash = true;
        }
    }
    // replace(/^-+|-+$/g, "")
    let trimmed = replaced.trim_matches('-');
    // slice(0, 80) — the slug is pure ASCII at this point, so code-unit and
    // character truncation agree.
    let mut slug: String = trimmed.chars().take(80).collect();
    if slug.is_empty() {
        slug = "note".to_string();
    }

    // "memory.md" matches MEMORY.md on case-insensitive filesystems, so that
    // slug would let a page write (or forget) clobber the index itself.
    if slug == "memory" {
        "memory-notes".to_string()
    } else {
        slug
    }
}

/// Parse the MEMORY.md index.
///
/// Parses MEMORY.md into an ordered map of filename → hook line. Lines are
/// expected to be `- [Title](slug.md) — hook`. A `Vec<(String, _)>` keeps the
/// Insertion order is kept (later duplicates update in place) so
/// re-serialisation is byte-identical.
pub fn parse_memory_index(content: &str) -> Vec<(String, MemoryIndexEntry)> {
    let mut entries: Vec<(String, MemoryIndexEntry)> = Vec::new();
    for line in content.split('\n') {
        // ^- \[([^\]]+)\]\(([^)]+\.md)\)(?:\s*—\s*(.*))?$
        let Some(rest) = line.strip_prefix("- [") else {
            continue;
        };
        let Some(title_end) = rest.find(']') else {
            continue;
        };
        if title_end == 0 {
            continue; // [^\]]+ needs at least one character
        }
        let title = &rest[..title_end];
        let Some(after_title) = rest[title_end + 1..].strip_prefix('(') else {
            continue;
        };
        // The filename runs to the first ')' (regex [^)]+), must end in ".md"
        // and carry at least one character before it.
        let Some(close) = after_title.find(')') else {
            continue;
        };
        let filename = &after_title[..close];
        if !filename.ends_with(".md") || filename.len() <= ".md".len() {
            continue;
        }
        // Optional ` — hook` tail, then the line must end.
        let tail = &after_title[close + 1..];
        let hook = if tail.is_empty() {
            String::new()
        } else {
            let Some(after_dash) = tail.trim_start().strip_prefix('—') else {
                continue;
            };
            after_dash.trim().to_string()
        };
        let entry = MemoryIndexEntry {
            title: title.trim().to_string(),
            hook,
        };
        // Map.set on an existing key keeps its original insertion position.
        if let Some(slot) = entries.iter_mut().find(|(name, _)| name == filename) {
            slot.1 = entry;
        } else {
            entries.push((filename.to_string(), entry));
        }
    }
    entries
}

/// Serialise the index map back to MEMORY.md content.
///
/// Serialises the index map back to MEMORY.md content: one line per entry,
/// `— hook` omitted when the hook is empty, newline-terminated when non-empty.
pub fn serialize_memory_index(entries: &[(String, MemoryIndexEntry)]) -> String {
    let lines: Vec<String> = entries
        .iter()
        .map(|(filename, entry)| {
            if entry.hook.is_empty() {
                format!("- [{}]({})", entry.title, filename)
            } else {
                format!("- [{}]({}) — {}", entry.title, filename, entry.hook)
            }
        })
        .collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

/// Upsert a topic page and keep MEMORY.md's index in sync.
///
/// Upserts a topic page and keeps MEMORY.md's index in sync:
/// - creates the memory dir if needed,
/// - appends the note to the topic page (creates it if missing),
/// - adds or updates the index entry (a `hook` argument overrides; otherwise
///   the existing hook is kept).
/// Returns the slug used.
pub fn write_repo_memory_note(
    memory_dir: &std::path::Path,
    topic: &str,
    note: &str,
    hook: Option<&str>,
) -> String {
    if !memory_dir.exists() {
        std::fs::create_dir_all(memory_dir).expect("failed to create repo memory dir");
    }

    let slug = slugify(topic);
    let filename = format!("{slug}.md");
    let page_path = memory_dir.join(&filename);
    let index_path = memory_dir.join(MEMORY_INDEX);

    // Build page content: append note to existing page or create fresh
    let existing_page_content = if page_path.exists() {
        std::fs::read_to_string(&page_path).unwrap_or_default()
    } else {
        String::new()
    };
    let new_page_content = if !existing_page_content.is_empty() {
        format!("{}\n\n{}\n", existing_page_content.trim_end(), note)
    } else {
        format!("# {topic}\n\n{note}\n")
    };
    std::fs::write(&page_path, new_page_content).expect("failed to write repo memory page");

    // Upsert index entry
    let index_content = if index_path.exists() {
        std::fs::read_to_string(&index_path).unwrap_or_default()
    } else {
        String::new()
    };
    let mut entries = parse_memory_index(&index_content);
    let existing_hook = entries
        .iter()
        .find(|(name, _)| name == &filename)
        .map(|(_, entry)| entry.hook.clone());
    let hook = hook
        .map(|hook| hook.to_string())
        .or(existing_hook)
        .unwrap_or_default();
    let entry = MemoryIndexEntry {
        title: topic.to_string(),
        hook,
    };
    if let Some(slot) = entries.iter_mut().find(|(name, _)| name == &filename) {
        slot.1 = entry;
    } else {
        entries.push((filename, entry));
    }
    std::fs::write(&index_path, serialize_memory_index(&entries))
        .expect("failed to write repo memory index");

    slug
}

/// Remove a topic page and its index entry from the memory bank.
///
/// Removes a topic page and its index entry from the memory bank. Re-slugifies
/// so a hostile or sloppy slug ("../notes", "MEMORY") can never resolve outside
/// the bank or onto the index file itself. Returns true if anything was
/// actually removed.
pub fn remove_repo_memory_page(memory_dir: &std::path::Path, slug: &str) -> bool {
    let filename = format!("{}.md", slugify(slug));
    let page_path = memory_dir.join(&filename);
    let index_path = memory_dir.join(MEMORY_INDEX);

    let mut removed = false;

    if page_path.exists() {
        std::fs::remove_file(&page_path).expect("failed to remove repo memory page");
        removed = true;
    }

    if index_path.exists() {
        let index_content = std::fs::read_to_string(&index_path).unwrap_or_default();
        let mut entries = parse_memory_index(&index_content);
        if entries.iter().any(|(name, _)| name == &filename) {
            entries.retain(|(name, _)| name != &filename);
            std::fs::write(&index_path, serialize_memory_index(&entries))
                .expect("failed to write repo memory index");
            removed = true;
        }
    }

    removed
}

#[cfg(test)]
mod memory_bank_tests {
    use super::*;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "drip-harness-tools-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn slugify_matches_ts() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("  --Parity Notes!! --"), "parity-notes");
        assert_eq!(slugify(""), "note");
        assert_eq!(slugify("///"), "note");
        // "memory" would collide with MEMORY.md on case-insensitive filesystems
        assert_eq!(slugify("memory"), "memory-notes");
        assert_eq!(slugify("MEMORY"), "memory-notes");
        let long = "a".repeat(100);
        assert_eq!(slugify(&long).len(), 80);
    }

    #[test]
    fn memory_index_round_trip() {
        let entries = vec![
            (
                "alpha.md".to_string(),
                MemoryIndexEntry {
                    title: "Alpha".to_string(),
                    hook: "first hook".to_string(),
                },
            ),
            (
                "beta.md".to_string(),
                MemoryIndexEntry {
                    title: "Beta".to_string(),
                    hook: String::new(),
                },
            ),
        ];
        let serialized = serialize_memory_index(&entries);
        assert_eq!(
            serialized,
            "- [Alpha](alpha.md) — first hook\n- [Beta](beta.md)\n"
        );
        assert_eq!(parse_memory_index(&serialized), entries);

        // parsing tolerates a missing hook and trims title whitespace
        let parsed = parse_memory_index("junk line\n- [  T ](t.md)\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "t.md");
        assert_eq!(parsed[0].1.title, "T");
        assert_eq!(parsed[0].1.hook, "");
    }

    #[test]
    fn write_repo_memory_note_upserts_page_and_index() {
        let dir = scratch_dir("write-note");
        let slug = write_repo_memory_note(&dir, "My Topic", "note one", Some("the hook"));
        assert_eq!(slug, "my-topic");
        assert_eq!(
            std::fs::read_to_string(dir.join("my-topic.md")).unwrap(),
            "# My Topic\n\nnote one\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("MEMORY.md")).unwrap(),
            "- [My Topic](my-topic.md) — the hook\n"
        );

        // second write appends to the page and keeps the existing hook
        assert_eq!(write_repo_memory_note(&dir, "My Topic", "note two", None), "my-topic");
        assert_eq!(
            std::fs::read_to_string(dir.join("my-topic.md")).unwrap(),
            "# My Topic\n\nnote one\n\nnote two\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("MEMORY.md")).unwrap(),
            "- [My Topic](my-topic.md) — the hook\n"
        );

        // an explicit hook (even empty) replaces the stored one
        write_repo_memory_note(&dir, "My Topic", "note three", Some(""));
        assert_eq!(
            std::fs::read_to_string(dir.join("MEMORY.md")).unwrap(),
            "- [My Topic](my-topic.md)\n"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_repo_memory_page_removes_page_and_index_entry() {
        let dir = scratch_dir("remove-page");
        write_repo_memory_note(&dir, "Keep Me", "keep note", Some("keep hook"));
        write_repo_memory_note(&dir, "Drop Me", "drop note", Some("drop hook"));

        assert!(remove_repo_memory_page(&dir, "drop-me"));
        assert!(!dir.join("drop-me.md").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("MEMORY.md")).unwrap(),
            "- [Keep Me](keep-me.md) — keep hook\n"
        );

        // re-slugify protects the bank: nothing outside resolves, nothing removed
        assert!(!remove_repo_memory_page(&dir, "../notes"));
        assert!(!remove_repo_memory_page(&dir, "MEMORY"));
        assert!(!remove_repo_memory_page(&dir, "missing-page"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// Repo memory config the op dispatcher needs.
#[derive(Debug, Clone, Default)]
pub struct RepoMemoryConfig {
    pub memory_dir: String,
    pub disabled: bool,
}

/// The model-visible outcome of one op.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessOpOutcome {
    pub text: String,
    pub state_changed: bool,
    pub task_finished: bool,
    pub ended_loop: bool,
    pub direct_response: Option<String>,
}

/// Context the op handlers read: the current loop's identity, the role gate
/// in force and the repo memory config (the `gate`/`repo_memory` args plus
/// the loop bookkeeping the per-op branches read).
#[derive(Debug, Clone, Default)]
pub struct HarnessOpContext {
    /// Current loop number (state.r#loop at call time).
    pub loop_number: u32,
    /// Id of the task the current loop is working (`currentTask.id`).
    pub current_task_id: Option<String>,
    /// Whether the loop has already ended (task-terminal calls are refused).
    pub loop_ended: bool,
    /// Role gate in force for this run (plan_tasks role filter, finish gates).
    pub gate: Option<HarnessRoleGate>,
    /// Repo memory bank directory + disabled flag (remember/forget repo scope).
    pub repo_memory: RepoMemoryConfig,
}

/// The raw status field as text ("completed", "dropped"), for messages like
/// "Task t-1 is already ${status}...".
fn harness_task_status_label(status: &crate::core::types::HarnessTaskStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{status:?}"))
}

/// Session-scope forget: removes the memory note with the given id from
/// `state.memory` and reports whether anything was removed.
fn remove_memory_note(state: &mut crate::core::types::HarnessState, note_id: &str) -> bool {
    match state.memory.iter().position(|note| note.id == note_id) {
        Some(index) => {
            state.memory.remove(index);
            true
        }
        None => false,
    }
}

/// The state-mutating half of a harness tool call (every HarnessOp variant
/// is handled here).
pub fn apply_harness_op(
    state: &mut crate::core::types::HarnessState,
    op: HarnessOp,
    ctx: &HarnessOpContext,
) -> HarnessOpOutcome {
    match op {
        HarnessOp::PlanTasks { entries, placement, unknown_roles } => {
            let placement = if placement == "next" {
                crate::core::state::HarnessTaskPlacement::Next
            } else {
                crate::core::state::HarnessTaskPlacement::End
            };
            // harness_tools::HarnessTaskInput -> core_state::HarnessTaskInput
            let state_entries = entries
                .into_iter()
                .map(|entry| core_state::HarnessTaskInput {
                    depends_on: if entry.depends_on.is_empty() { None } else { Some(entry.depends_on) },
                    review_of: None,
                    role: entry.role,
                    title: entry.title,
                })
                .collect::<Vec<_>>();
            let added = core_state::add_tasks(state, state_entries, placement);

            if added.is_empty() {
                return HarnessOpOutcome {
                    text: "No tasks were added. Provide a non-empty tasks array of task titles.".to_string(),
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            }

            let unknown_role_note = if unknown_roles.is_empty() {
                String::new()
            } else {
                format!(
                    " Unknown role(s) {} were ignored — those tasks use the default role.",
                    unknown_roles.join(", ")
                )
            };

            let list = added
                .iter()
                .map(|task| {
                    format!(
                        "{}: {}{}",
                        task.id,
                        task.title,
                        task.role
                            .as_ref()
                            .map(|role| format!(" [role: {}]", role))
                            .unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");

            HarnessOpOutcome {
                text: format!(
                    "Added {} task(s){}: {}.{}",
                    added.len(),
                    if placement == core_state::HarnessTaskPlacement::Next {
                        " ahead of the pending queue"
                    } else {
                        ""
                    },
                    list,
                    unknown_role_note
                ),
                state_changed: true,
                task_finished: false,
                ended_loop: false,
                direct_response: None,
            }
        }
        HarnessOp::DropTask { task_id, reason } => {
            let existing_status = core_state::get_task_by_id(state, &task_id).map(|task| task.status.clone());

            let Some(existing_status) = existing_status else {
                return HarnessOpOutcome {
                    text: if task_id.is_empty() {
                        "Provide the taskId of the task to drop.".to_string()
                    } else {
                        format!("No task with id {task_id} exists.")
                    },
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            };

            if existing_status == crate::core::types::HarnessTaskStatus::Completed
                || existing_status == crate::core::types::HarnessTaskStatus::Dropped
            {
                return HarnessOpOutcome {
                    text: format!(
                        "Task {task_id} is already {} and cannot be dropped.",
                        harness_task_status_label(&existing_status)
                    ),
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            }

            // captured before the drop: dropping the task the current loop is
            // working finishes that loop
            let was_current_task = existing_status == crate::core::types::HarnessTaskStatus::InProgress;

            let dropped_task = core_state::drop_task(state, &task_id, &reason);

            let Some(dropped_task) = dropped_task else {
                return HarnessOpOutcome {
                    text: format!("Task {task_id} could not be dropped."),
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            };

            let dropped_id = dropped_task.id.clone();
            let dropped_summary = dropped_task.summary.clone().unwrap_or_default();

            // A pending/in-progress review of the dropped task is now
            // orphaned — drop it too.
            let orphaned_review_ids: Vec<String> = state
                .tasks
                .iter()
                .filter(|task| {
                    task.review_of.as_deref() == Some(dropped_id.as_str())
                        && (task.status == crate::core::types::HarnessTaskStatus::Pending
                            || task.status == crate::core::types::HarnessTaskStatus::InProgress)
                })
                .map(|task| task.id.clone())
                .collect();

            for review_id in &orphaned_review_ids {
                core_state::drop_task(
                    state,
                    review_id,
                    &format!("The reviewed task {dropped_id} was dropped."),
                );
            }

            let mut text = format!("Task {dropped_id} dropped: {dropped_summary}");

            if !orphaned_review_ids.is_empty() {
                text.push_str(&format!(
                    " (also dropped its review task {})",
                    orphaned_review_ids.join(", ")
                ));
            }

            HarnessOpOutcome {
                text,
                state_changed: true,
                task_finished: was_current_task,
                ended_loop: false,
                direct_response: None,
            }
        }
        HarnessOp::ReviseTask { task_id, title } => {
            let existing_task = core_state::get_task_by_id(state, &task_id)
                .map(|task| (task.status.clone(), task.id.clone()));

            let Some((existing_status, existing_id)) = existing_task else {
                return HarnessOpOutcome {
                    text: if task_id.is_empty() {
                        "Provide the taskId of the task to revise.".to_string()
                    } else {
                        format!("No task with id {task_id} exists.")
                    },
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            };

            if existing_status == crate::core::types::HarnessTaskStatus::Completed
                || existing_status == crate::core::types::HarnessTaskStatus::Dropped
            {
                return HarnessOpOutcome {
                    text: format!(
                        "Task {existing_id} is already {}; finished tasks cannot be revised.",
                        harness_task_status_label(&existing_status)
                    ),
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            }

            let revised_task = core_state::revise_task(state, &task_id, &title);

            let Some(revised_task) = revised_task else {
                return HarnessOpOutcome {
                    text: "No revision was applied. Provide a non-empty title string.".to_string(),
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            };

            HarnessOpOutcome {
                text: format!("Task {} is now: {}", revised_task.id, revised_task.title),
                state_changed: true,
                task_finished: false,
                ended_loop: false,
                direct_response: None,
            }
        }
        HarnessOp::FinishTask { status, summary, task_id } => {
            use crate::core::types::HarnessTaskStatus;

            // Task-terminal calls are refused once the loop has ended (a
            // previous call in the same response finished the task).
            if ctx.loop_ended {
                return HarnessOpOutcome {
                    text: "The loop already ended (a previous call in this response finished the task) — finish_task was not executed. Re-issue it from the next loop if still needed.".to_string(),
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            }

            // An empty taskId falls back to the current task.
            let task_id = task_id.filter(|id| !id.is_empty());
            let target_id = match &task_id {
                Some(id) => match core_state::get_task_by_id(state, id) {
                    Some(task) => task.id.clone(),
                    None => {
                        return HarnessOpOutcome {
                            text: format!("No task with id {id} exists."),
                            state_changed: false,
                            task_finished: false,
                            ended_loop: false,
                            direct_response: None,
                        };
                    }
                },
                None => match core_state::get_current_task(state) {
                    Some(task) => task.id.clone(),
                    None => {
                        let blocked_ids: Vec<String> = state
                            .tasks
                            .iter()
                            .filter(|task| task.status == HarnessTaskStatus::Blocked)
                            .map(|task| task.id.clone())
                            .collect();
                        let text = if !blocked_ids.is_empty() {
                            format!(
                                "There is no current task — name the blocked task to resolve: finish_task {{\"taskId\": \"{}\", ...}}. Blocked: {}. If later work already satisfied one, complete it by id citing that evidence, or drop_task it with the reason.",
                                blocked_ids[0],
                                blocked_ids.join(", ")
                            )
                        } else {
                            "There is no current task to finish. Call plan_tasks first.".to_string()
                        };
                        return HarnessOpOutcome {
                            text,
                            state_changed: false,
                            task_finished: false,
                            ended_loop: false,
                            direct_response: None,
                        };
                    }
                },
            };

            // Snapshot the fields the gates read before any state mutation.
            let (target_status, target_activations, has_review_of, target_verify_nudged, target_footprint, target_edit_nudged, target_title) = {
                let task = core_state::get_task_by_id(state, &target_id).expect("target task exists");
                (
                    task.status.clone(),
                    task.activations,
                    task.review_of.is_some(),
                    task.verify_nudged,
                    task.footprint.clone(),
                    task.edit_nudged,
                    task.title.clone(),
                )
            };

            let current_task_id = ctx
                .current_task_id
                .clone()
                .or_else(|| core_state::get_current_task(state).map(|task| task.id.clone()));
            let is_current_task = current_task_id.as_deref() == Some(target_id.as_str());
            // Planning/replanning loops have no current task; finishing a task
            // by id there IS the loop's work. The gates below target drive-by
            // finishes from loops that have their own current task.
            let is_drive_by = current_task_id.is_some() && !is_current_task;
            let ends_loop = is_current_task || current_task_id.is_none();

            // A review verdict is only meaningful from the review task's own
            // loop — otherwise the worker whose output is under review can
            // confirm itself by naming the review task's id.
            if has_review_of {
                if is_drive_by {
                    return HarnessOpOutcome {
                        text: format!(
                            "Task {target_id} is a review task — its verdict must come from its own loop, not from another task's loop."
                        ),
                        state_changed: false,
                        task_finished: false,
                        ended_loop: false,
                        direct_response: None,
                    };
                }

                let review_task =
                    core_state::get_task_by_id(state, &target_id).expect("target task exists").clone();
                let verdict =
                    apply_review_verdict(state, &review_task, status, &summary, ctx.gate.as_ref());
                // The verdict comes from the review task's own loop, so
                // finishing it ends that loop exactly when the task finished.
                return HarnessOpOutcome {
                    text: verdict.result_text,
                    state_changed: verdict.state_changed,
                    task_finished: verdict.task_finished,
                    ended_loop: verdict.task_finished,
                    direct_response: None,
                };
            }

            // Completing a pending task that no loop has ever worked is a
            // plan edit masquerading as progress. Evidence must come from a
            // loop that actually ran the task — resolving a BLOCKED task by
            // id from a replanning loop stays legitimate.
            if status == FinishTaskStatus::Completed
                && target_status == HarnessTaskStatus::Pending
                && target_activations.unwrap_or(0) == 0
            {
                return HarnessOpOutcome {
                    text: format!(
                        "Task {target_id} is pending and has never been worked by a loop — it cannot be marked completed from here. Let its own loop do the work, or drop_task it with a reason if it is no longer needed."
                    ),
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            }

            // Edit honesty gate: a task whose title reads as a code change ("add …",
            // "fix …", "port …") completed while NOTHING in the run has edited the
            // workspace is the small-model false completion seen in delegation lanes
            // (finish_task completed, zero PATCH calls). ONE bounce names it; a
            // second unchanged finish_task is accepted — the change may already
            // exist, or the task turned out to be analysis only. Scoped to the whole
            // run, not the task: small models routinely do the work inside the
            // planning loop (no current task to footprint) and then walk the ledger
            // as formalities — bouncing those cost two extra calls per A/B run.
            if status == FinishTaskStatus::Completed
                && is_current_task
                && !target_edit_nudged.unwrap_or(false)
            {
                let edited_workspace = target_footprint
                    .iter()
                    .flatten()
                    .any(|entry| entry.starts_with("edited "))
                    || state.workspace_edits.unwrap_or(0) > 0;

                if !edited_workspace && looks_like_build_task(&target_title) {
                    if let Some(task) = core_state::get_task_by_id_mut(state, &target_id) {
                        task.edit_nudged = Some(true);
                    }
                    return HarnessOpOutcome {
                        text: format!(
                            "harness: not accepted yet — this task reads like a code change (\"{}\") but no workspace edit has landed in this run. Make the change now (PATCH), then finish_task. If the task genuinely needs no edit (already in place, or analysis only), call finish_task again unchanged and it will be accepted.",
                            crate::harness::telemetry::truncate_text(&target_title, 80)
                        ),
                        state_changed: true,
                        task_finished: false,
                        ended_loop: false,
                        direct_response: None,
                    };
                }
            }

            // Verification honesty gate: completing a task that edited the
            // workspace while the run's verification evidence is missing,
            // stale, or red gets ONE bounce-back naming the exact problem. A
            // second finish_task is accepted unchanged: the model may know
            // why no check applies here.
            if status == FinishTaskStatus::Completed
                && is_current_task
                && !target_verify_nudged.unwrap_or(false)
            {
                let edited_workspace = target_footprint
                    .iter()
                    .flatten()
                    .any(|entry| entry.starts_with("edited "));
                let verification = state.last_verification.clone();
                let stale_edits = state.mutations_since_verification.unwrap_or(0);
                let verification_problem: Option<String> = match &verification {
                    None => Some(
                        "no verification command (test/build/typecheck) has run at any point in this run"
                            .to_string(),
                    ),
                    Some(record) if record.failed => Some(format!(
                        "the most recent verification ({}) FAILED and nothing has passed since",
                        record.command
                    )),
                    Some(record) if record.ran_no_tests == Some(true) => Some(format!(
                        "the most recent verification ({}) exited green but executed zero tests — run the suite that actually covers this change",
                        record.command
                    )),
                    Some(record) if stale_edits > 0 => Some(format!(
                        "{} workspace edit(s) landed after the last verification ({})",
                        stale_edits, record.command
                    )),
                    Some(_) => None,
                };

                if edited_workspace {
                    if let Some(problem) = verification_problem {
                        if let Some(task) = core_state::get_task_by_id_mut(state, &target_id) {
                            task.verify_nudged = Some(true);
                        }
                        return HarnessOpOutcome {
                            text: format!(
                                "harness: not accepted yet — this task edited the workspace but {problem}. Run the check now (VERIFY, CHECK, the verification command the goal names, or the project's test/build command via BASH), then finish_task. If no check applies to this change, call finish_task again unchanged and it will be accepted."
                            ),
                            state_changed: true,
                            task_finished: false,
                            ended_loop: false,
                            direct_response: None,
                        };
                    }
                }
            }

            let core_status = match status {
                FinishTaskStatus::Completed => HarnessTaskStatus::Completed,
                FinishTaskStatus::Blocked => HarnessTaskStatus::Blocked,
            };
            let (finished_id, finished_title, finished_role, finished_status_label) =
                match core_state::finish_task(
                    state,
                    core_state::HarnessFinishArgs {
                        status: core_status,
                        summary: &summary,
                        task_id: Some(&target_id),
                    },
                ) {
                    Some(finished_task) => (
                        finished_task.id.clone(),
                        finished_task.title.clone(),
                        finished_task.role.clone(),
                        harness_task_status_label(&finished_task.status),
                    ),
                    None => {
                        return HarnessOpOutcome {
                            text: format!("Task {target_id} could not be finished."),
                            state_changed: false,
                            task_finished: false,
                            ended_loop: false,
                            direct_response: None,
                        };
                    }
                };

            // Verify gate: completed work by a role with a reviewer does not
            // pass unexamined — a review task under the reviewer role runs
            // next.
            if status == FinishTaskStatus::Completed {
                if let Some(gate) = ctx.gate.as_ref() {
                    let role_name = finished_role.clone().or_else(|| gate.default_task_role.clone());
                    let verifier = role_name
                        .as_deref()
                        .and_then(|role| gate.roles.get(role))
                        .and_then(|spec| spec.verified_by.clone());
                    let has_open_review = state.tasks.iter().any(|task| {
                        task.review_of.as_deref() == Some(finished_id.as_str())
                            && (task.status == HarnessTaskStatus::Pending
                                || task.status == HarnessTaskStatus::InProgress)
                    });

                    if let Some(verifier) = verifier {
                        if gate.roles.contains_key(&verifier) && !has_open_review {
                            let entries = vec![core_state::HarnessTaskInput {
                                depends_on: None,
                                review_of: Some(finished_id.clone()),
                                role: Some(verifier.clone()),
                                title: format!(
                                    "Review {finished_id} (\"{finished_title}\"): independently verify the completed work with your own tools, then finish_task completed to confirm it, or blocked with what is wrong to send it back."
                                ),
                            }];
                            let added = core_state::add_tasks(
                                state,
                                entries,
                                core_state::HarnessTaskPlacement::Next,
                            );
                            let review_task_id = added
                                .first()
                                .map(|task| task.id.clone())
                                .unwrap_or_default();

                            // The reviewer starts a fresh transcript: hand
                            // over the author's recorded footprint so
                            // verification starts from the actual changes,
                            // not a re-derivation of them.
                            let footprint = target_footprint.unwrap_or_default();
                            if !footprint.is_empty() {
                                if let Some(review_task) =
                                    core_state::get_task_by_id_mut(state, &review_task_id)
                                {
                                    crate::core::state::append_task_note(
                                        review_task,
                                        &format!(
                                            "author evidence (harness-recorded): {}",
                                            footprint.join("; ")
                                        ),
                                    );
                                }
                            }

                            return HarnessOpOutcome {
                                text: format!(
                                    "Task {finished_id} marked completed. Review task {review_task_id} (role {verifier}) was created — the goal cannot complete until the review confirms the work."
                                ),
                                state_changed: true,
                                task_finished: ends_loop,
                                ended_loop: ends_loop,
                                direct_response: None,
                            };
                        }
                    }
                }
            }

            // Finishing a sibling task from a working loop is bookkeeping;
            // only finishing the CURRENT task (or a resolution from a
            // planning loop) ends this loop.
            HarnessOpOutcome {
                text: format!(
                    "Task {finished_id} marked {finished_status_label}.{}",
                    if is_drive_by { " (The current task's loop continues.)" } else { "" }
                ),
                state_changed: true,
                task_finished: ends_loop,
                ended_loop: ends_loop,
                direct_response: None,
            }
        }
        HarnessOp::Respond { text } => {
            let text = text.trim().to_string();

            if text.is_empty() {
                return HarnessOpOutcome {
                    text: "Provide the answer text to respond with.".to_string(),
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            }

            let has_unfinished_tasks = state.tasks.iter().any(|task| {
                matches!(
                    task.status,
                    crate::core::types::HarnessTaskStatus::Pending
                        | crate::core::types::HarnessTaskStatus::InProgress
                        | crate::core::types::HarnessTaskStatus::Blocked
                )
            });

            if has_unfinished_tasks {
                return HarnessOpOutcome {
                    text: "Unfinished tasks exist — respond is only for goals that need no task work. Finish, drop, or complete the tasks first.".to_string(),
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            }

            // The answer completes the goal through the normal ledger: a synthetic
            // completed task keeps isGoalComplete/history/summary semantics intact.
            let mut answer_tasks = core_state::add_tasks(
                state,
                vec![core_state::HarnessTaskInput {
                    depends_on: None,
                    review_of: None,
                    role: None,
                    title: "Answer the goal directly".to_string(),
                }],
                core_state::HarnessTaskPlacement::End,
            );
            let answer_task = match answer_tasks.pop() {
                Some(task) => task,
                None => {
                    return HarnessOpOutcome {
                        text: "Answer recorded — the run will report it to the user.".to_string(),
                        state_changed: false,
                        task_finished: false,
                        ended_loop: false,
                        direct_response: None,
                    };
                }
            };

            let _finished = core_state::finish_task(
                state,
                core_state::HarnessFinishArgs {
                    status: crate::core::types::HarnessTaskStatus::Completed,
                    summary: &crate::harness::telemetry::truncate_text(&text, 400),
                    task_id: Some(&answer_task.id),
                },
            );
            state.direct_response = Some(crate::core::types::HarnessDirectResponse {
                created_at_iteration: state.iteration,
                text: text.clone(),
            });

            HarnessOpOutcome {
                text: "Answer recorded — the run will report it to the user.".to_string(),
                state_changed: true,
                task_finished: true,
                ended_loop: true,
                direct_response: Some(text),
            }
        }
        HarnessOp::Recall { tool_name, query } => {
            let requested_tool = tool_name.trim().to_string();
            let query = query.trim().to_string();

            if requested_tool.is_empty() || query.is_empty() {
                return HarnessOpOutcome {
                    text: "Provide toolName and query (a fragment of the original call's input).".to_string(),
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            }

            let key_prefix = format!("{}:", requested_tool);
            let mut candidates: Vec<&crate::core::types::ToolTelemetryRecord> = state
                .telemetry
                .values()
                .filter(|record| {
                    record.key.starts_with(&key_prefix)
                        && (record.raw_input.contains(&query) || record.input_preview.contains(&query))
                })
                .collect();
            candidates.sort_by(|a, b| b.last_used_iteration.cmp(&a.last_used_iteration));
            let matched = candidates.first().copied();

            let matched = match matched {
                Some(record) => record,
                None => {
                    return HarnessOpOutcome {
                        text: format!(
                            "No cached {} result matches \"{}\". Run the call itself if the information is still needed.",
                            requested_tool, query
                        ),
                        state_changed: false,
                        task_finished: false,
                        ended_loop: false,
                        direct_response: None,
                    };
                }
            };

            let failed_note = if matched.last_failed.unwrap_or(false) {
                ", FAILED when last run"
            } else {
                ""
            };

            HarnessOpOutcome {
                text: format!(
                    "Cached result of {} (from loop {}{}):\n\n{}",
                    matched.input_preview, matched.last_used_iteration, failed_note, matched.last_output
                ),
                state_changed: false,
                task_finished: false,
                ended_loop: false,
                direct_response: None,
            }
        }
        HarnessOp::Observe { note, ttl } => {
            let saved = crate::core::state::add_observation(
                state,
                crate::core::state::AddObservationArgs {
                    text: note,
                    ttl: ttl.map(|ttl| ttl as i64),
                },
                &crate::core::types::DEFAULT_TELEMETRY_CONFIG,
            );
            let saved_any = saved.is_some();

            let text = match saved {
                None => "No observation was saved. Provide a non-empty note string.".to_string(),
                Some((observation, refreshed)) => {
                    if refreshed {
                        format!("Refreshed observation {} (ttl {}).", observation.id, observation.ttl)
                    } else {
                        format!(
                            "Saved observation {} (expires in {} task loop(s) unless re-observed).",
                            observation.id, observation.ttl
                        )
                    }
                }
            };
            HarnessOpOutcome {
                text,
                state_changed: saved_any,
                task_finished: false,
                ended_loop: false,
                direct_response: None,
            }
        }
        HarnessOp::NoteTask { note, task_id } => {
            let note = note.trim().to_string();

            if note.is_empty() {
                return HarnessOpOutcome {
                    text: "Provide the note text.".to_string(),
                    state_changed: false,
                    task_finished: false,
                    ended_loop: false,
                    direct_response: None,
                };
            }

            let trimmed_note = note;
            let target_task = match task_id.as_deref() {
                Some(task_id) => {
                    let resolved = crate::core::state::get_task_by_id(state, task_id).map(|task| task.id.clone());
                    match resolved {
                        Some(id) => Some((id, task_id.to_string())),
                        None => None,
                    }
                }
                None => crate::core::state::get_current_task(state).map(|task| (task.id.clone(), String::new())),
            };

            let (target_id, _requested_task_id) = match target_task {
                Some(pair) => pair,
                None => {
                    let text = match task_id.as_deref() {
                        Some(task_id) => format!("No task with id {task_id} exists."),
                        None => "There is no current task to annotate. Use remember for run-wide notes.".to_string(),
                    };
                    return HarnessOpOutcome {
                        text,
                        state_changed: false,
                        task_finished: false,
                        ended_loop: false,
                        direct_response: None,
                    };
                }
            };

            let truncated = crate::harness::telemetry::truncate_text(&trimmed_note, 400);
            if let Some(task) = crate::core::state::get_task_by_id_mut(state, &target_id) {
                crate::core::state::append_task_note(task, &truncated);
            }

            HarnessOpOutcome {
                text: format!("Noted on {target_id}. The loop continues."),
                state_changed: true,
                task_finished: false,
                ended_loop: false,
                direct_response: None,
            }
        }
        HarnessOp::Remember { scope, note, topic, hook } => {
            let text;
            let mut state_changed = false;

            if matches!(scope, MemoryScope::Repo) {
                let topic = topic.as_deref().map(str::trim).unwrap_or("");
                let note_text = note.trim();

                if topic.is_empty() || note_text.is_empty() {
                    text = "Repo-scoped remember requires both a non-empty 'topic' and a non-empty 'note'.".to_string();
                } else if ctx.repo_memory.memory_dir.is_empty() {
                    text = "Repo memory is unavailable (no memory directory configured for this run).".to_string();
                } else if ctx.repo_memory.disabled {
                    text = "Repo memory is disabled for this run (--no-repo-memory flag). Note not saved.".to_string();
                } else {
                    let slug = write_repo_memory_note(
                        std::path::Path::new(&ctx.repo_memory.memory_dir),
                        topic,
                        note_text,
                        hook.as_deref().map(str::trim),
                    );
                    text = format!("Saved repo memory note to page '{slug}.md' (topic: {topic}).");
                }
            } else {
                // session scope (default)
                match crate::core::state::add_memory_note(state, &note) {
                    None => {
                        text = "No note was saved. Provide a non-empty note string.".to_string();
                    }
                    Some(saved) => {
                        text = format!("Saved memory note {}.", saved.id);
                        state_changed = true;
                    }
                }
            }

            HarnessOpOutcome {
                text,
                state_changed,
                task_finished: false,
                ended_loop: false,
                direct_response: None,
            }
        }
        HarnessOp::Forget { scope, note_id } => {
            let text;
            let mut state_changed = false;

            if matches!(scope, MemoryScope::Repo) {
                let slug = note_id.trim();

                if slug.is_empty() {
                    text = "Repo-scoped forget requires a non-empty 'noteId' (the page slug, e.g. 'my-topic').".to_string();
                } else if ctx.repo_memory.memory_dir.is_empty() {
                    text = "Repo memory is unavailable (no memory directory configured for this run).".to_string();
                } else if ctx.repo_memory.disabled {
                    text = "Repo memory is disabled for this run (--no-repo-memory flag). Nothing removed.".to_string();
                } else {
                    let removed =
                        remove_repo_memory_page(std::path::Path::new(&ctx.repo_memory.memory_dir), slug);
                    text = if removed {
                        format!("Removed repo memory page '{slug}.md' and its index entry.")
                    } else {
                        format!("No repo memory page found for slug '{slug}'. Nothing removed.")
                    };
                }
            } else {
                // session scope (default)
                let removed = remove_memory_note(state, &note_id);
                state_changed = removed;
                text = if removed {
                    format!("Removed memory note {note_id}.")
                } else {
                    format!("No memory note with id {note_id} exists.")
                };
            }

            HarnessOpOutcome {
                text,
                state_changed,
                task_finished: false,
                ended_loop: false,
                direct_response: None,
            }
        }
    }
}

#[cfg(test)]
mod apply_harness_op_tests {
    use super::*;
    use crate::core::state::create_harness_state;

    /// The plan_tasks branch: two planned tasks are appended with ids
    /// task-1/task-2 and the exact model-visible result text
    /// (`Added N task(s): id: title; id: title.`).
    #[test]
    fn plan_tasks_adds_two_tasks_with_ids_and_exact_result_text() {
        let mut state = create_harness_state("test goal: plan two tasks");
        let raw = r#"{"tasks": [{"title": "First task"}, {"title": "Second task"}], "placement": "end"}"#;
        let op = parse_harness_op("plan_tasks", raw).expect("plan_tasks input parses");
        let ctx = HarnessOpContext::default();
        let outcome = apply_harness_op(&mut state, op, &ctx);

        assert_eq!(state.tasks.len(), 2);
        assert_eq!(state.tasks[0].id, "task-1");
        assert_eq!(state.tasks[0].title, "First task");
        assert_eq!(state.tasks[1].id, "task-2");
        assert_eq!(state.tasks[1].title, "Second task");
        assert_eq!(
            outcome.text,
            "Added 2 task(s): task-1: First task; task-2: Second task."
        );
        assert!(outcome.state_changed);
        assert!(!outcome.task_finished);
        assert!(!outcome.ended_loop);
        assert_eq!(outcome.direct_response, None);

        // placement "next" inserts ahead of the pending queue and adds the
        // " ahead of the pending queue" phrase to the result text.
        let raw_next = r#"{"tasks": [{"title": "Urgent task"}], "placement": "next"}"#;
        let op_next = parse_harness_op("plan_tasks", raw_next).expect("plan_tasks next parses");
        let outcome_next = apply_harness_op(&mut state, op_next, &ctx);
        assert_eq!(state.tasks[0].id, "task-3");
        assert_eq!(
            outcome_next.text,
            "Added 1 task(s) ahead of the pending queue: task-3: Urgent task."
        );
    }

    /// The empty-tasks array path returns the refusal text and changes no
    /// state.
    #[test]
    fn plan_tasks_with_empty_tasks_array_is_refused() {
        let mut state = create_harness_state("test goal: empty plan");
        let raw = r#"{"tasks": []}"#;
        let op = parse_harness_op("plan_tasks", raw).expect("plan_tasks empty parses");
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(
            outcome.text,
            "No tasks were added. Provide a non-empty tasks array of task titles."
        );
        assert!(!outcome.state_changed);
        assert!(state.tasks.is_empty());
    }


    /// The respond branch: empty text is refused verbatim; unfinished
    /// tasks block the answer; otherwise a synthetic completed
    /// "Answer the goal directly" task is added, the direct response is
    #[test]
    fn looks_like_build_task_keys_on_the_leading_verb_and_a_workspace_object() {
        assert!(looks_like_build_task("add the widget export"));
        assert!(looks_like_build_task("2. Fix src/parser.ts trailing newline"));
        assert!(looks_like_build_task("port loop.ts to Rust"));
        assert!(!looks_like_build_task("write summary"));
        assert!(!looks_like_build_task("update the plan"));
        assert!(!looks_like_build_task("verify the added export"));
        assert!(!looks_like_build_task("inspect module a"));
        assert!(!looks_like_build_task("run the tests"));
    }

    /// The edit gate: a build-shaped task completed without any workspace
    /// edit bounces once, then the unchanged retry is accepted.
    #[test]
    fn finish_task_bounces_a_build_task_without_edits_once() {
        let mut state = create_harness_state("goal");
        let op = parse_harness_op("plan_tasks", r#"{"tasks": ["add the widget export to src/index.ts"]}"#)
            .expect("plan_tasks input parses");
        apply_harness_op(&mut state, op, &HarnessOpContext::default());
        {
            let task = &mut state.tasks[0];
            task.status = crate::core::types::HarnessTaskStatus::InProgress;
            task.activations = Some(1);
            task.footprint = Some(vec!["ran bun run test -> passed".to_string()]);
        }
        let ctx = HarnessOpContext {
            current_task_id: Some("task-1".to_string()),
            ..HarnessOpContext::default()
        };

        let op = parse_harness_op("finish_task", r#"{"status":"completed","summary":"done"}"#).unwrap();
        let bounced = apply_harness_op(&mut state, op, &ctx);
        assert!(!bounced.task_finished);
        assert!(bounced.text.contains("no workspace edit has landed in this run"));
        assert_eq!(state.tasks[0].edit_nudged, Some(true));

        let op = parse_harness_op("finish_task", r#"{"status":"completed","summary":"already in place"}"#).unwrap();
        let accepted = apply_harness_op(&mut state, op, &ctx);
        assert!(accepted.task_finished);
        assert_eq!(state.tasks[0].status, crate::core::types::HarnessTaskStatus::Completed);
    }

    /// The unverified-edit bounce names every accepted route, including the
    /// verification command the goal itself declares.
    #[test]
    fn finish_task_bounce_names_the_goal_declared_verification_route() {
        let mut state = create_harness_state("goal");
        let op = parse_harness_op("plan_tasks", r#"{"tasks": ["write NOTES.md"]}"#).expect("plan_tasks input parses");
        apply_harness_op(&mut state, op, &HarnessOpContext::default());
        state.tasks[0].status = crate::core::types::HarnessTaskStatus::InProgress;
        state.tasks[0].activations = Some(1);
        state.tasks[0].footprint = Some(vec!["edited via shell: cat > NOTES.md <<'EOF'".to_string()]);
        state.workspace_edits = Some(1);
        state.mutations_since_verification = Some(1);
        let ctx = HarnessOpContext {
            current_task_id: Some("task-1".to_string()),
            ..HarnessOpContext::default()
        };

        let op = parse_harness_op("finish_task", r#"{"status":"completed","summary":"done"}"#).unwrap();
        let bounced = apply_harness_op(&mut state, op, &ctx);
        assert!(!bounced.task_finished);
        assert!(bounced.text.contains("this task edited the workspace but"));
        assert!(bounced.text.contains("the verification command the goal names"), "{}", bounced.text);
    }

    /// Planning-loop work: the run edited the workspace before the task was
    /// active, so the task has no footprint but is accepted without a bounce.
    #[test]
    fn finish_task_accepts_a_build_task_when_the_run_edited_elsewhere() {
        let mut state = create_harness_state("goal");
        let op = parse_harness_op("plan_tasks", r#"{"tasks": ["fix the clamp bound in src/lib.rs"]}"#)
            .expect("plan_tasks input parses");
        apply_harness_op(&mut state, op, &HarnessOpContext::default());
        state.tasks[0].status = crate::core::types::HarnessTaskStatus::InProgress;
        state.tasks[0].activations = Some(1);
        state.workspace_edits = Some(1);
        let ctx = HarnessOpContext {
            current_task_id: Some("task-1".to_string()),
            ..HarnessOpContext::default()
        };

        let op = parse_harness_op("finish_task", r#"{"status":"completed","summary":"fixed during planning"}"#).unwrap();
        let accepted = apply_harness_op(&mut state, op, &ctx);
        assert!(accepted.task_finished);
        assert_eq!(state.tasks[0].edit_nudged, None);
    }

    /// recorded on state, and the loop ends.
    #[test]
    fn respond_records_the_direct_answer_and_ends_the_loop() {
        let mut state = create_harness_state("test goal: respond");

        let op = HarnessOp::Respond {
            text: "   ".to_string(),
        };
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(outcome.text, "Provide the answer text to respond with.");
        assert!(!outcome.state_changed);
        assert!(!outcome.task_finished);

        let op = parse_harness_op("plan_tasks", r#"{"tasks": ["Unfinished work"]}"#)
            .expect("plan_tasks input parses");
        apply_harness_op(&mut state, op, &HarnessOpContext::default());

        let op = HarnessOp::Respond {
            text: "too soon".to_string(),
        };
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(
            outcome.text,
            "Unfinished tasks exist — respond is only for goals that need no task work. Finish, drop, or complete the tasks first."
        );
        assert!(!outcome.state_changed);

        let op = parse_harness_op("drop_task", r#"{"taskId": "task-1"}"#).expect("drop input parses");
        apply_harness_op(&mut state, op, &HarnessOpContext::default());

        let op = HarnessOp::Respond {
            text: "  the final answer  ".to_string(),
        };
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(outcome.text, "Answer recorded — the run will report it to the user.");
        assert!(outcome.state_changed);
        assert!(outcome.task_finished);
        assert!(outcome.ended_loop);
        assert_eq!(outcome.direct_response.as_deref(), Some("the final answer"));
        let recorded = state
            .direct_response
            .as_ref()
            .expect("direct response recorded on state");
        assert_eq!(recorded.text, "the final answer");
        assert_eq!(recorded.created_at_iteration, state.iteration);
        let answer_task = state
            .tasks
            .iter()
            .find(|task| task.title == "Answer the goal directly")
            .expect("synthetic answer task exists");
        assert_eq!(answer_task.status, crate::core::types::HarnessTaskStatus::Completed);
    }

    /// The recall branch: re-surfaces the most recent matching
    /// telemetry record by toolName + query fragment, echoing the cached
    /// output verbatim (failed runs carry the FAILED note).
    #[test]
    fn recall_surfaces_the_cached_tool_result() {
        let mut state = create_harness_state("test goal: recall found");
        state.telemetry.insert(
            "READ:src/harness/harness-tools.ts:854".to_string(),
            crate::core::types::ToolTelemetryRecord {
                call_count: 1,
                input_preview: "READ {\"offset\":854,\"path\":\"src/harness/harness-tools.ts\"}"
                    .to_string(),
                iterations_used: vec![3],
                key: "READ:src/harness/harness-tools.ts:854".to_string(),
                last_failed: Some(false),
                last_output: "Read lines 854-922 of 1099 from src/harness/harness-tools.ts."
                    .to_string(),
                last_used_iteration: 3,
                raw_input: "{\"offset\":854,\"path\":\"src/harness/harness-tools.ts\"}".to_string(),
                reinforcements: 0,
                tool_name: "READ".to_string(),
            },
        );

        let op = parse_harness_op(
            "recall",
            r#"{"toolName": "READ", "query": "harness-tools"}"#,
        )
        .expect("recall input parses");
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(
            outcome.text,
            "Cached result of READ {\"offset\":854,\"path\":\"src/harness/harness-tools.ts\"} (from loop 3):\n\nRead lines 854-922 of 1099 from src/harness/harness-tools.ts."
        );
        assert!(!outcome.state_changed);
        assert!(!outcome.task_finished);
        assert!(!outcome.ended_loop);
    }

    /// The recall branch: no telemetry record matching the query
    /// yields the verbatim not-found text.
    #[test]
    fn recall_without_a_match_reports_it() {
        let mut state = create_harness_state("test goal: recall miss");
        let op = parse_harness_op(
            "recall",
            r#"{"toolName": "READ", "query": "nothing-here"}"#,
        )
        .expect("recall input parses");
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(
            outcome.text,
            "No cached READ result matches \"nothing-here\". Run the call itself if the information is still needed."
        );
        assert!(!outcome.state_changed);
        assert!(!outcome.task_finished);
    }

    /// The drop_task branch: dropping a pending task reports its new summary
    /// (core_state::drop_task sets the summary to the reason), marks the task
    /// dropped, and does not finish the loop (the task was not the loop's
    /// in-progress task). A second drop is refused with the terminal-status
    /// text.
    #[test]
    fn drop_task_drops_a_pending_task_and_reports_the_summary() {
        let mut state = create_harness_state("test goal: drop pending");
        let raw = r#"{"tasks": [{"title": "First task"}, {"title": "Second task"}]}"#;
        let op = parse_harness_op("plan_tasks", raw).expect("plan_tasks input parses");
        apply_harness_op(&mut state, op, &HarnessOpContext::default());

        let op = parse_harness_op(
            "drop_task",
            r#"{"taskId": "task-1", "reason": "no longer needed"}"#,
        )
        .expect("drop_task input parses");
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());

        assert_eq!(outcome.text, "Task task-1 dropped: no longer needed");
        assert!(outcome.state_changed);
        assert!(!outcome.task_finished);
        assert!(!outcome.ended_loop);
        assert_eq!(outcome.direct_response, None);
        assert_eq!(
            state.tasks[0].status,
            crate::core::types::HarnessTaskStatus::Dropped
        );
        assert_eq!(state.tasks[0].summary, Some("no longer needed".to_string()));

        // dropping a task that is already dropped is refused
        let again = parse_harness_op(
            "drop_task",
            r#"{"taskId": "task-1", "reason": "again"}"#,
        )
        .expect("drop_task again parses");
        let outcome = apply_harness_op(&mut state, again, &HarnessOpContext::default());
        assert_eq!(
            outcome.text,
            "Task task-1 is already dropped and cannot be dropped."
        );
        assert!(!outcome.state_changed);
        assert!(!outcome.task_finished);
    }

    /// The drop_task branch refusals: an unknown id and an empty taskId
    /// return the refusal texts verbatim and change no state.
    #[test]
    fn drop_task_unknown_id_returns_the_ts_refusal_text() {
        let mut state = create_harness_state("test goal: drop unknown");

        let op = parse_harness_op("drop_task", r#"{"taskId": "task-99"}"#)
            .expect("drop_task unknown parses");
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(outcome.text, "No task with id task-99 exists.");
        assert!(!outcome.state_changed);
        assert!(!outcome.task_finished);

        // empty taskId → the "provide the taskId" guidance text
        let op = HarnessOp::DropTask { task_id: String::new(), reason: String::new() };
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(outcome.text, "Provide the taskId of the task to drop.");
        assert!(!outcome.state_changed);
        assert!(!outcome.task_finished);
    }

    /// The revise_task branch: a successful retitle reports the new
    /// title, updates the task, and (via core_state::revise_task) appends the
    /// "Retitled from" note.
    #[test]
    fn revise_task_retitles_the_task_and_appends_the_retitle_note() {
        let mut state = create_harness_state("test goal: revise");
        let raw = r#"{"tasks": [{"title": "First task"}]}"#;
        let op = parse_harness_op("plan_tasks", raw).expect("plan_tasks input parses");
        apply_harness_op(&mut state, op, &HarnessOpContext::default());

        let op = parse_harness_op(
            "revise_task",
            r#"{"taskId": "task-1", "title": "Renamed task"}"#,
        )
        .expect("revise_task input parses");
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());

        assert_eq!(outcome.text, "Task task-1 is now: Renamed task");
        assert!(outcome.state_changed);
        assert!(!outcome.task_finished);
        assert!(!outcome.ended_loop);
        assert_eq!(state.tasks[0].title, "Renamed task");
        assert_eq!(state.tasks[0].notes, ["Retitled from \"First task\"."]);
    }

    /// The note_task branch: with no explicit taskId the note is appended to
    /// the current task and the loop continues; an unknown id and a state
    /// with no current task return the refusal texts verbatim.
    #[test]
    fn note_task_appends_the_note_and_reports_the_target() {
        let mut state = create_harness_state("test goal: note task");
        let raw = r#"{"tasks": [{"title": "First task"}]}"#;
        let op = parse_harness_op("plan_tasks", raw).expect("plan_tasks input parses");
        apply_harness_op(&mut state, op, &HarnessOpContext::default());

        let op = HarnessOp::NoteTask {
            note: "  partial finding: the parser needs a second pass  ".to_string(),
            task_id: None,
        };
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(outcome.text, "Noted on task-1. The loop continues.");
        assert!(outcome.state_changed);
        assert!(!outcome.task_finished);
        assert!(!outcome.ended_loop);
        assert_eq!(state.tasks[0].notes, ["partial finding: the parser needs a second pass"]);

        // unknown explicit taskId → the verbatim not-found text, no state change
        let op = HarnessOp::NoteTask {
            note: "miss".to_string(),
            task_id: Some("task-99".to_string()),
        };
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(outcome.text, "No task with id task-99 exists.");
        assert!(!outcome.state_changed);
        assert!(!outcome.task_finished);

        // no tasks at all → the verbatim no-current-task text
        let mut empty = create_harness_state("test goal: note without tasks");
        let op = HarnessOp::NoteTask {
            note: "orphan note".to_string(),
            task_id: None,
        };
        let outcome = apply_harness_op(&mut empty, op, &HarnessOpContext::default());
        assert_eq!(
            outcome.text,
            "There is no current task to annotate. Use remember for run-wide notes."
        );
        assert!(!outcome.state_changed);
        assert!(!outcome.task_finished);
    }

    /// The observe branch: a new note is saved with the base ttl and
    /// the verbatim saved text; re-observing the same text refreshes the
    /// existing observation instead of duplicating it; a non-string/empty
    /// note is refused verbatim.
    #[test]
    fn observe_saves_and_refreshes_observations_with_the_verbatim_texts() {
        let mut state = create_harness_state("test goal: observe");

        let op = HarnessOp::Observe {
            note: "the failing check is the settings test".to_string(),
            ttl: None,
        };
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(
            outcome.text,
            "Saved observation obs-1 (expires in 4 task loop(s) unless re-observed)."
        );
        assert!(outcome.state_changed);
        assert!(!outcome.task_finished);
        assert!(!outcome.ended_loop);
        assert_eq!(state.observations.len(), 1);
        assert_eq!(state.observations[0].id, "obs-1");
        assert_eq!(state.observations[0].ttl, 4);

        // re-observing the same text refreshes the stored observation
        let op = HarnessOp::Observe {
            note: "  the failing check is the settings test  ".to_string(),
            ttl: None,
        };
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(outcome.text, "Refreshed observation obs-1 (ttl 4).");
        assert!(outcome.state_changed);
        assert_eq!(state.observations.len(), 1);

        // an empty note is refused with the verbatim text, no state change
        let op = HarnessOp::Observe {
            note: "   ".to_string(),
            ttl: None,
        };
        let outcome = apply_harness_op(&mut state, op, &HarnessOpContext::default());
        assert_eq!(
            outcome.text,
            "No observation was saved. Provide a non-empty note string."
        );
        assert!(!outcome.state_changed);
        assert!(!outcome.task_finished);
    }
}
