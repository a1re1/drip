// drip — Rust port of lci (see drip/PLAN.md). Module tree from PLAN.md's
// Layout section; each module names the TS file it ports in its header
// comment. The port is complete for every headless surface (parity suite:
// drip/parity); the web UI and ink terminal layer are out of scope.

pub mod chat;
pub mod cli;
pub mod core;
pub mod harness;
pub mod lib_fs;
pub mod migrate;
pub mod tools;
pub mod tui;
pub mod watch;