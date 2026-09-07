// Event colours and labels shared by the transcript surfaces (the TUI
// timeline, dripw's transcript pane). The colour names are chalk/ink names;
// watch::ansi maps them onto SGR codes.

use crate::core::types::HarnessEventType;

pub fn event_color(kind: HarnessEventType) -> &'static str {
    match kind {
        HarnessEventType::ContextExpired => "gray",
        HarnessEventType::ContextPromoted => "yellow",
        HarnessEventType::ContextRefreshed => "yellow",
        HarnessEventType::HarnessOp => "cyan",
        HarnessEventType::Inference => "gray",
        HarnessEventType::IterationStart => "blue",
        HarnessEventType::LoopStart => "blueBright",
        HarnessEventType::ModelText => "white",
        HarnessEventType::OperatorMessage => "cyan",
        HarnessEventType::Question => "purple",
        HarnessEventType::RateLimited => "red",
        HarnessEventType::RunComplete => "green",
        HarnessEventType::RunSummary => "green",
        HarnessEventType::RunWarning => "red",
        HarnessEventType::StallRecovery => "yellow",
        HarnessEventType::TaskFinished => "green",
        HarnessEventType::ToolCall => "magenta",
        HarnessEventType::ToolResult => "gray",
    }
}

pub fn event_label(kind: HarnessEventType) -> &'static str {
    match kind {
        HarnessEventType::ContextExpired => "cool",
        HarnessEventType::ContextPromoted => "warm",
        HarnessEventType::ContextRefreshed => "live",
        HarnessEventType::HarnessOp => "op",
        HarnessEventType::Inference => "infer",
        HarnessEventType::IterationStart => "cycle",
        HarnessEventType::LoopStart => "loop",
        HarnessEventType::ModelText => "text",
        HarnessEventType::OperatorMessage => "steer",
        HarnessEventType::Question => "ask",
        HarnessEventType::RateLimited => "wait",
        HarnessEventType::RunComplete => "done",
        HarnessEventType::RunSummary => "summary",
        HarnessEventType::RunWarning => "warn",
        HarnessEventType::StallRecovery => "stall",
        HarnessEventType::TaskFinished => "task",
        HarnessEventType::ToolCall => "tool",
        HarnessEventType::ToolResult => "result",
    }
}

/// Ink colour name → ANSI painter (watch::ansi::c). Unknown names dim.
pub fn paint(color: &str) -> fn(&str) -> String {
    use crate::watch::ansi::c;
    match color {
        "gray" => c::gray,
        "yellow" => c::yellow,
        "cyan" => c::cyan,
        "blue" => c::blue,
        "blueBright" => c::cyan_bold,
        "red" => c::red,
        "green" => c::green,
        "white" => c::white,
        "magenta" | "purple" => c::magenta,
        _ => c::dim,
    }
}

/// The painter for an event kind (`paint(event_color(kind))`).
pub fn event_paint(kind: HarnessEventType) -> fn(&str) -> String {
    paint(event_color(kind))
}

pub const ACCENT_COLOR: &str = "cyan";
pub const DIM_COLOR: &str = "gray";
