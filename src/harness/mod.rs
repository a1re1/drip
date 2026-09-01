// port of src/harness — TODO
//
// anthropic/transport port src/chat/{anthropic,transport}.ts (the harness
// model-call layer in TS lives in src/chat/).
pub mod anthropic;
pub mod chat_types;
pub mod harness_tools;
// `loop` is a Rust keyword: the module keeps PLAN.md's name as a raw identifier.
#[allow(non_snake_case)]
pub mod r#loop;
pub mod model_call;
pub mod prompt;
pub mod roles;
pub mod telemetry;
pub mod transport;
