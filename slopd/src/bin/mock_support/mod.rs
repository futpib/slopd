//! Shared test-control protocol for the mock backends.
//!
//! Runtime controls use `::mock ...`; slash commands are reserved for commands
//! implemented by the real client being mocked. Synthetic launch options use
//! `--mock-*`, and each mock rejects unknown options in that namespace.

use std::time::Duration;

#[allow(dead_code)] // This shared module is compiled separately into each mock binary.
pub const CLAUDE_HELP: &str = "\
::mock help
::mock echo <text>
::mock sleep <duration>
::mock env <name>
::mock cwd
::mock process exit <code>
::mock input-mode <always-submit|alternating>
::mock busy <duration>
::mock permission [duration]
::mock hook <event>
::mock fail <once|always>
::mock fail-then-busy <duration>
::mock transport <disconnect|stall-hooks>
::mock spawn-pane";

#[allow(dead_code)] // This shared module is compiled separately into each mock binary.
pub const CODEX_HELP: &str = "\
::mock help
::mock active
::mock permission
::mock policy <show|restrict>";

#[allow(dead_code)] // This shared module is compiled separately into each mock binary.
pub const OPENCODE_HELP: &str = "\
::mock help
::mock switch-session
::mock fail once
::mock tool
::mock question
::mock subagent <normal|leak|retry>";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputMode {
    AlwaysSubmit,
    Alternating,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentMode {
    Normal,
    Leak,
    Retry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MockCommand<'a> {
    Help,
    Echo(&'a str),
    Sleep(Duration),
    Env(&'a str),
    Cwd,
    ProcessExit(i32),
    InputMode(InputMode),
    Busy(Duration),
    Permission(Option<Duration>),
    Hook(&'a str),
    FailOnce,
    FailAlways,
    FailThenBusy(Duration),
    TransportDisconnect,
    TransportStallHooks,
    SpawnPane,
    Active,
    PolicyShow,
    PolicyRestrict,
    SwitchSession,
    Tool,
    Question,
    Subagent(SubagentMode),
}

pub fn parse(input: &str) -> Result<Option<MockCommand<'_>>, String> {
    let input = input.trim();
    if !input.starts_with("::mock") {
        return Ok(None);
    }
    if input == "::mock" {
        return Err("missing command; use `::mock help`".to_string());
    }
    let Some(body) = input.strip_prefix("::mock ") else {
        return Err("expected a space after `::mock`".to_string());
    };
    let (name, args) = split_head(body);
    let command = match name {
        "help" => {
            require_no_args(name, args)?;
            MockCommand::Help
        }
        "echo" => MockCommand::Echo(require_args(name, args)?),
        "sleep" => MockCommand::Sleep(parse_duration(require_single_arg(name, args)?)?),
        "env" => MockCommand::Env(require_single_arg(name, args)?),
        "cwd" => {
            require_no_args(name, args)?;
            MockCommand::Cwd
        }
        "process" => {
            let (action, action_args) = split_head(require_args(name, args)?);
            if action != "exit" {
                return Err(format!(
                    "unknown process action `{action}`; expected `exit`"
                ));
            }
            let code = require_single_arg("process exit", action_args)?
                .parse::<i32>()
                .map_err(|_| "`::mock process exit` requires an integer exit code".to_string())?;
            MockCommand::ProcessExit(code)
        }
        "input-mode" => {
            let mode = match require_single_arg(name, args)? {
                "always-submit" => InputMode::AlwaysSubmit,
                "alternating" => InputMode::Alternating,
                other => {
                    return Err(format!(
                        "unknown input mode `{other}`; expected `always-submit` or `alternating`"
                    ));
                }
            };
            MockCommand::InputMode(mode)
        }
        "busy" => MockCommand::Busy(parse_duration(require_single_arg(name, args)?)?),
        "permission" => {
            let duration = if args.is_empty() {
                None
            } else {
                Some(parse_duration(require_single_arg(name, args)?)?)
            };
            MockCommand::Permission(duration)
        }
        "hook" => MockCommand::Hook(require_single_arg(name, args)?),
        "fail" => match require_single_arg(name, args)? {
            "once" => MockCommand::FailOnce,
            "always" => MockCommand::FailAlways,
            other => {
                return Err(format!(
                    "unknown failure mode `{other}`; expected `once` or `always`"
                ));
            }
        },
        "fail-then-busy" => {
            MockCommand::FailThenBusy(parse_duration(require_single_arg(name, args)?)?)
        }
        "transport" => match require_single_arg(name, args)? {
            "disconnect" => MockCommand::TransportDisconnect,
            "stall-hooks" => MockCommand::TransportStallHooks,
            other => {
                return Err(format!(
                    "unknown transport action `{other}`; expected `disconnect` or `stall-hooks`"
                ));
            }
        },
        "spawn-pane" => {
            require_no_args(name, args)?;
            MockCommand::SpawnPane
        }
        "active" => {
            require_no_args(name, args)?;
            MockCommand::Active
        }
        "policy" => match require_single_arg(name, args)? {
            "show" => MockCommand::PolicyShow,
            "restrict" => MockCommand::PolicyRestrict,
            other => {
                return Err(format!(
                    "unknown policy action `{other}`; expected `show` or `restrict`"
                ));
            }
        },
        "switch-session" => {
            require_no_args(name, args)?;
            MockCommand::SwitchSession
        }
        "tool" => {
            require_no_args(name, args)?;
            MockCommand::Tool
        }
        "question" => {
            require_no_args(name, args)?;
            MockCommand::Question
        }
        "subagent" => {
            let mode = match require_single_arg(name, args)? {
                "normal" => SubagentMode::Normal,
                "leak" => SubagentMode::Leak,
                "retry" => SubagentMode::Retry,
                other => {
                    return Err(format!(
                        "unknown subagent mode `{other}`; expected `normal`, `leak`, or `retry`"
                    ));
                }
            };
            MockCommand::Subagent(mode)
        }
        other => return Err(format!("unknown mock command `{other}`; use `::mock help`")),
    };
    Ok(Some(command))
}

pub fn parse_duration(value: &str) -> Result<Duration, String> {
    if let Some(milliseconds) = value.strip_suffix("ms") {
        return milliseconds
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| {
                format!("invalid duration `{value}`; expected a number followed by `ms` or `s`")
            });
    }
    if let Some(seconds) = value.strip_suffix('s') {
        return seconds
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| {
                format!("invalid duration `{value}`; expected a number followed by `ms` or `s`")
            });
    }
    Err(format!(
        "invalid duration `{value}`; expected an explicit `ms` or `s` suffix"
    ))
}

pub fn reject_unknown_mock_option(backend: &str, option: &str) -> ! {
    eprintln!("{backend}: unknown mock option `{option}`");
    std::process::exit(2);
}

fn split_head(input: &str) -> (&str, &str) {
    let input = input.trim();
    if let Some(index) = input.find(char::is_whitespace) {
        (&input[..index], input[index..].trim())
    } else {
        (input, "")
    }
}

fn require_args<'a>(command: &str, args: &'a str) -> Result<&'a str, String> {
    if args.is_empty() {
        Err(format!("`::mock {command}` requires an argument"))
    } else {
        Ok(args)
    }
}

fn require_single_arg<'a>(command: &str, args: &'a str) -> Result<&'a str, String> {
    let args = require_args(command, args)?;
    if args.split_whitespace().count() != 1 {
        Err(format!("`::mock {command}` requires exactly one argument"))
    } else {
        Ok(args)
    }
}

fn require_no_args(command: &str, args: &str) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("`::mock {command}` does not accept arguments"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_regular_and_slash_commands() {
        assert_eq!(parse("hello").unwrap(), None);
        assert_eq!(parse("/compact").unwrap(), None);
    }

    #[test]
    fn parses_structured_commands() {
        assert_eq!(
            parse("::mock fail once").unwrap(),
            Some(MockCommand::FailOnce)
        );
        assert_eq!(
            parse("::mock subagent retry").unwrap(),
            Some(MockCommand::Subagent(SubagentMode::Retry))
        );
        assert_eq!(
            parse("::mock busy 250ms").unwrap(),
            Some(MockCommand::Busy(Duration::from_millis(250)))
        );
    }

    #[test]
    fn requires_exact_commands_and_duration_units() {
        assert!(parse("::mockery tool").is_err());
        assert!(parse("::mock busy 2").is_err());
        assert!(parse("::mock subagent retry later").is_err());
        assert!(parse("please use a tool").unwrap().is_none());
    }
}
