//! Slash command registry and parser, ported 1:1 from src/cli/commands.ts.

/// Static specification of a slash command.
pub struct SlashCommandSpec {
    pub args: Option<&'static str>,
    pub description: &'static str,
    pub name: &'static str,
}

/// The available slash commands, in the same order as the TypeScript source.
pub const SLASH_COMMANDS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        args: None,
        description: "Show the available commands and composer shortcuts.",
        name: "help",
    },
    SlashCommandSpec {
        args: None,
        description: "Pick the active model profile.",
        name: "model",
    },
    SlashCommandSpec {
        args: None,
        description: "Pick the tool-calling model profile (requests that expose tools).",
        name: "toolmodel",
    },
    SlashCommandSpec {
        args: None,
        description: "Pick the active system prompt persona.",
        name: "prompt",
    },
    SlashCommandSpec {
        args: None,
        description: "Start a fresh session in this directory.",
        name: "new",
    },
    SlashCommandSpec {
        args: Some("[id]"),
        description: "Resume a previous session (picker when no id).",
        name: "resume",
    },
    SlashCommandSpec {
        args: None,
        description: "List recent sessions for this directory.",
        name: "sessions",
    },
    SlashCommandSpec {
        args: None,
        description: "Show the harness state: tasks, memory, warm context.",
        name: "state",
    },
    SlashCommandSpec {
        args: None,
        description: "List available SKILL.md skills.",
        name: "skills",
    },
    SlashCommandSpec {
        args: Some("<name>"),
        description: "Toggle a skill on or off for upcoming runs.",
        name: "skill",
    },
    SlashCommandSpec {
        args: Some("[add <repo> [name] | remove <name> | update [name] | list]"),
        description: "Manage skill/plugin marketplaces (git repos or local dirs).",
        name: "marketplace",
    },
    SlashCommandSpec {
        args: Some("<enable|disable> <marketplace/plugin[/skill]>"),
        description: "Enable or disable a marketplace plugin or one of its skills.",
        name: "plugin",
    },
    SlashCommandSpec {
        args: None,
        description: "Show the resolved harness roles and loop bindings.",
        name: "roles",
    },
    SlashCommandSpec {
        args: None,
        description: "Show config file location and active profiles.",
        name: "config",
    },
    SlashCommandSpec {
        args: Some("[KEY=value]"),
        description:
            "List token env vars the model profiles use, or save one to the lci env.vars file.",
        name: "env",
    },
    SlashCommandSpec {
        args: None,
        description: "Exit lci.",
        name: "quit",
    },
];

/// A slash command parsed out of composer text.
pub struct ParsedSlashCommand {
    pub args: String,
    pub name: String,
}

/// Parse composer text as a slash command.
///
/// The text is trimmed and must start with "/". The name is the run of ASCII
/// letters and '-' right after the slash (case-insensitive, lower-cased on
/// return). After the name either the text ends, or whitespace separates the
/// name from the args (args trimmed; may span lines). Anything else returns
/// `None`.
pub fn parse_slash_command(text: &str) -> Option<ParsedSlashCommand> {
    let trimmed_text = text.trim();

    if !trimmed_text.starts_with('/') {
        return None;
    }

    let rest = &trimmed_text[1..];
    let name_end = rest
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_uppercase() || c == '-'))
        .unwrap_or(rest.len());

    if name_end == 0 {
        return None;
    }

    let name = rest[..name_end].to_ascii_lowercase();
    let after = &rest[name_end..];

    let args = if after.is_empty() {
        String::new()
    } else if after.starts_with(char::is_whitespace) {
        after.trim().to_string()
    } else {
        // A non-whitespace character right after the name, e.g. "/foo!x".
        return None;
    };

    Some(ParsedSlashCommand { args, name })
}

/// Suggestions stay open while the composer holds a lone "/token" with no arguments yet.
pub fn get_slash_command_suggestions(text: &str) -> Vec<&'static SlashCommandSpec> {
    if !text.starts_with('/') || text.trim().chars().any(char::is_whitespace) {
        return Vec::new();
    }

    let query = text[1..].to_ascii_lowercase();

    SLASH_COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(&query))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_args() {
        let parsed = parse_slash_command("/resume abc 123").expect("should parse");
        assert_eq!(parsed.name, "resume");
        assert_eq!(parsed.args, "abc 123");

        let parsed = parse_slash_command("/HELP").expect("should parse");
        assert_eq!(parsed.name, "help");
        assert_eq!(parsed.args, "");
    }

    #[test]
    fn rejects_non_commands() {
        assert!(parse_slash_command("hello").is_none());
        assert!(parse_slash_command("/").is_none());
        assert!(parse_slash_command("/foo!x").is_none());
    }

    #[test]
    fn suggestions_prefix_and_close_on_space() {
        let names = |text: &str| -> Vec<&'static str> {
            get_slash_command_suggestions(text)
                .iter()
                .map(|command| command.name)
                .collect()
        };

        assert_eq!(names("/mo"), vec!["model"]);
        assert_eq!(names("/s"), vec!["sessions", "state", "skills", "skill"]);
        assert!(names("/model ").is_empty());
        assert_eq!(names("/").len(), SLASH_COMMANDS.len());
        assert!(get_slash_command_suggestions("hello").is_empty());
        assert!(get_slash_command_suggestions("hello").is_empty());
    }
}
