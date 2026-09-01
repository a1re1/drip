// drip — Rust port of lci (see drip/PLAN.md). Module tree from PLAN.md's
// Layout section; each module names the TS file it ports in its header
// comment. Unported modules are empty files carrying
// `// port of src/<path> — TODO`.

pub mod cli;
pub mod core;
pub mod harness;
pub mod migrate;
pub mod tools;
pub mod tui;
pub mod watch;
