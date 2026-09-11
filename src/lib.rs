// drip — a headless-first coding agent harness. Headless surfaces are the
// primary scope; the ink terminal layer is out of scope. The browser UI
// (`drip --ui`) is a Bun app embedded under cli::ui that bridges to the same
// session files and CLI modes — no HTTP server lives in this crate.

pub mod chat;
pub mod cli;
pub mod core;
pub mod harness;
pub mod lib_fs;
pub mod tools;
pub mod tui;
pub mod watch;
