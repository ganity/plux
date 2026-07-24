use std::env;

use crate::error::Result;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Enter {
        name: String,
        ssh_target: Option<String>,
    },
    New {
        name: String,
    },
    List,
    Kill {
        name: String,
    },
    Stop,
    Run {
        command: Vec<String>,
    },
    Daemon,
    Bridge {
        start: bool,
    },
    Help,
}

pub fn parse() -> Result<Command> {
    parse_args(env::args().skip(1).collect())
}

fn parse_args(args: Vec<String>) -> Result<Command> {
    let Some(command) = args.first() else {
        return Ok(Command::Enter {
            name: "default".to_string(),
            ssh_target: None,
        });
    };
    let mut rest = args[1..].iter().cloned();

    match command.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "__daemon" => Ok(Command::Daemon),
        "__bridge" => {
            let start = match rest.next().as_deref() {
                None => false,
                Some("--start") => {
                    if rest.next().is_some() {
                        return Err("unexpected __bridge argument".into());
                    }
                    true
                }
                Some(argument) => {
                    return Err(format!("unexpected __bridge argument: {argument}").into())
                }
            };
            Ok(Command::Bridge { start })
        }
        "new" => Ok(Command::New {
            name: rest.next().unwrap_or_else(|| "default".to_string()),
        }),
        "attach" => parse_enter(rest, true),
        "list" | "ls" => Ok(Command::List),
        "kill" => Ok(Command::Kill {
            name: rest.next().unwrap_or_else(|| "default".to_string()),
        }),
        "stop" | "kill-server" => Ok(Command::Stop),
        "run" => {
            let command = rest.collect::<Vec<_>>();
            let command = command
                .strip_prefix(&["--".to_string()])
                .unwrap_or(&command)
                .to_vec();
            if command.is_empty() {
                return Err("run requires a command after --".into());
            }
            Ok(Command::Run { command })
        }
        option if option.starts_with('-') => parse_enter(args.into_iter(), false),
        name if rest.next().is_none() => Ok(Command::Enter {
            name: name.to_string(),
            ssh_target: None,
        }),
        name => Err(format!("unexpected argument after session name: {name}").into()),
    }
}

fn parse_enter(args: impl Iterator<Item = String>, legacy: bool) -> Result<Command> {
    let mut args = args.peekable();
    let mut name = None;
    let mut ssh_target = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--ssh" => ssh_target = Some(args.next().ok_or("--ssh requires a target")?),
            "-f" | "--force" | "-c" | "--create" if legacy => {}
            option if option.starts_with('-') => {
                return Err(format!("unknown option: {option}").into())
            }
            _ if name.is_none() => name = Some(argument),
            _ => return Err(format!("unexpected session argument: {argument}").into()),
        }
    }
    Ok(Command::Enter {
        name: name.unwrap_or_else(|| "default".to_string()),
        ssh_target,
    })
}

pub fn usage() -> &'static str {
    "Usage:\n  plux [<name>]\n  plux --ssh <target> [<name>]\n  plux list\n  plux kill [<name>]\n  plux stop\n  plux run -- <command> [args...]\n  plux --help"
}

#[cfg(test)]
mod tests {
    use super::{parse_args, Command};

    fn parse(args: &[&str]) -> Command {
        parse_args(args.iter().map(|argument| argument.to_string()).collect()).unwrap()
    }

    #[test]
    fn session_name_enters_with_automatic_lifecycle() {
        assert_eq!(
            parse(&["work"]),
            Command::Enter {
                name: "work".to_string(),
                ssh_target: None,
            }
        );
    }

    #[test]
    fn ssh_target_is_a_top_level_option() {
        assert_eq!(
            parse(&["--ssh", "user@server", "work"]),
            Command::Enter {
                name: "work".to_string(),
                ssh_target: Some("user@server".to_string()),
            }
        );
    }

    #[test]
    fn legacy_attach_flags_remain_compatible() {
        assert_eq!(
            parse(&["attach", "--create", "--force", "work"]),
            parse(&["work"])
        );
    }

    #[test]
    fn run_strips_separator() {
        let args = ["echo".to_string(), "ok".to_string()];
        assert_eq!(args, ["echo".to_string(), "ok".to_string()]);
        assert_eq!(
            Command::Run {
                command: args.to_vec()
            },
            Command::Run {
                command: args.to_vec()
            }
        );
    }
}
