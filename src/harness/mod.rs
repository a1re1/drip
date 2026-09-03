// Anthropic provider transport: request/response translation for the
// harness model-call layer.
pub mod anthropic;
pub mod chat_types;
pub mod harness_tools;
// `loop` is a Rust keyword: the module name needs the raw identifier.
#[allow(non_snake_case)]
pub mod r#loop;
pub mod model_call;
pub mod prompt;
pub mod redact;
pub mod roles;
pub mod telemetry;
pub mod transport;
