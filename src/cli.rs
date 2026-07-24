use std::env;

use crate::error::Result;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Attach {
        name: String,
        force: bool,
        create: bool,
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
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        return Ok(Command::Attach {
            name: "default".to_string(),
            force: false,
            create: true,
            ssh_target: None,
        });
    };

    match command.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "__daemon" => Ok(Command::Daemon),
        "__bridge" => {
            let start = match args.next().as_deref() {
                None => false,
                Some("--start") => {
                    if args.next().is_some() {
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
            name: args.next().unwrap_or_else(|| "default".to_string()),
        }),
        "attach" => {
            let mut name = None;
            let mut force = false;
            let mut create = false;
            let mut ssh_target = None;
            while let Some(argument) = args.next() {
                match argument.as_str() {
                    "-f" | "--force" => force = true,
                    "-c" | "--create" => create = true,
                    "--ssh" => {
                        ssh_target = Some(args.next().ok_or("--ssh requires a target")?);
                    }
                    _ if name.is_none() => name = Some(argument),
                    _ => return Err(format!("unexpected attach argument: {argument}").into()),
                }
            }
            Ok(Command::Attach {
                name: name.unwrap_or_else(|| "default".to_string()),
                force,
                create,
                ssh_target,
            })
        }
        "list" | "ls" => Ok(Command::List),
        "kill" => Ok(Command::Kill {
            name: args.next().unwrap_or_else(|| "default".to_string()),
        }),
        "stop" | "kill-server" => Ok(Command::Stop),
        "run" => {
            let command = args.collect::<Vec<_>>();
            let command = command
                .strip_prefix(&["--".to_string()])
                .unwrap_or(&command)
                .to_vec();
            if command.is_empty() {
                return Err("run requires a command after --".into());
            }
            Ok(Command::Run { command })
        }
        unknown => Err(format!("unknown command: {unknown}\n\n{}", usage()).into()),
    }
}

pub fn usage() -> &'static str {
    "Usage:\n  plux [attach <name>]\n  plux new [<name>]\n  plux attach [--create] [--force] [--ssh <target>] [<name>]\n  plux list\n  plux kill [<name>]\n  plux stop\n  plux run -- <command> [args...]\n  plux --help"
}

#[cfg(test)]
mod tests {
    use super::Command;

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
