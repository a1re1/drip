// port of src/cli/ui/theme.ts
//
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

pub const ACCENT_COLOR: &str = "cyan";
pub const DIM_COLOR: &str = "gray";
