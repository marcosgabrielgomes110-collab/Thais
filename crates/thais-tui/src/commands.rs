//! Slash-command registry and autocomplete helpers.

/// One user-facing slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    /// Command name without the slash.
    pub name: &'static str,
    /// Argument hint.
    pub args: &'static str,
    /// Short description shown by autocomplete and `/help`.
    pub description: &'static str,
}

/// Commands available in the TUI.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        args: "",
        description: "show commands and key bindings",
    },
    SlashCommand {
        name: "clear",
        args: "",
        description: "clear the transcript",
    },
    SlashCommand {
        name: "new",
        args: "",
        description: "create a new session in this workspace",
    },
    SlashCommand {
        name: "sessions",
        args: "",
        description: "list and resume sessions",
    },
    SlashCommand {
        name: "model",
        args: "[id]",
        description: "list models or change the default model",
    },
    SlashCommand {
        name: "history",
        args: "<query>",
        description: "search the session Event Log",
    },
    SlashCommand {
        name: "status",
        args: "",
        description: "show session, usage, and scroll state",
    },
    SlashCommand {
        name: "cancel",
        args: "",
        description: "cancel the active turn",
    },
    SlashCommand {
        name: "approve",
        args: "",
        description: "allow the pending approval",
    },
    SlashCommand {
        name: "deny",
        args: "",
        description: "deny the pending approval",
    },
    SlashCommand {
        name: "quit",
        args: "",
        description: "leave the TUI",
    },
];

/// Commands matching a partial slash input.
#[must_use]
pub fn matching(input: &str) -> Vec<&'static SlashCommand> {
    let Some(rest) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if rest.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(rest))
        .collect()
}

/// Complete the selected command with a trailing space.
#[must_use]
pub fn complete(input: &str, selected: usize) -> Option<String> {
    let matches = matching(input);
    let command = matches.get(selected).or_else(|| matches.first())?;
    Some(format!("/{} ", command.name))
}

/// Parse an exact slash command and its argument text.
#[must_use]
pub fn parse(input: &str) -> Option<(&'static SlashCommand, &str)> {
    let rest = input.trim().strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    let (name, argument) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(name, argument)| (name, argument.trim()));
    let command = COMMANDS.iter().find(|command| command.name == name)?;
    Some((command, argument))
}

/// Full help text.
#[must_use]
pub fn help_text() -> String {
    let mut output = String::from("Commands\n");
    for command in COMMANDS {
        let invocation = if command.args.is_empty() {
            format!("/{}", command.name)
        } else {
            format!("/{} {}", command.name, command.args)
        };
        output.push_str(&format!("  {invocation:<20} {}\n", command.description));
    }
    output.push_str(
        "\nKeys\n  Enter send        Tab complete      Up/Down choose\n  PgUp/PgDn scroll  Home/End jump      Ctrl+S sessions\n  Ctrl+C quit       Esc cancel/close\n",
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_and_completes_commands() {
        let matches = matching("/he");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "help");
        assert_eq!(complete("/he", 0).as_deref(), Some("/help "));
        assert!(matching("/model x").is_empty());
        assert!(matching("hello").is_empty());
        assert_eq!(matching("/").len(), COMMANDS.len());
    }

    #[test]
    fn parses_commands_and_arguments() {
        let (command, argument) = parse("/history error in tests").expect("command");
        assert_eq!(command.name, "history");
        assert_eq!(argument, "error in tests");
        assert_eq!(parse("/help").expect("help").1, "");
        assert!(parse("/missing").is_none());
        assert!(parse("plain text").is_none());
    }

    #[test]
    fn help_lists_every_command() {
        let help = help_text();
        for command in COMMANDS {
            assert!(help.contains(command.name));
        }
        assert!(help.contains("PgUp/PgDn"));
    }
}
