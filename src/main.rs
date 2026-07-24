mod cli;
mod client;
mod config;
mod daemon;
mod error;
mod layout;
mod pane;
mod protocol;
mod session;
mod socket;
mod terminal;
mod transport;

use cli::Command;
use config::Config;
use error::Result;

fn main() -> Result<()> {
    let command = cli::parse()?;
    if command == Command::Help {
        println!("{}", cli::usage());
        return Ok(());
    }

    let config = Config::load()?;
    match command {
        Command::Attach {
            name,
            force,
            create,
            ssh_target,
        } => client::attach(&config, name, force, create, ssh_target.as_deref())?,
        Command::New { name } => match client::request_or_start(
            &config,
            protocol::ClientMessage::Create {
                name: name.clone(),
                rows: pane::DEFAULT_ROWS,
                cols: pane::DEFAULT_COLS,
                command: None,
                temporary: false,
            },
        )? {
            protocol::ServerMessage::Created { .. } => println!("created session: {name}"),
            protocol::ServerMessage::Error { message } => return Err(message.into()),
            response => println!("{response:?}"),
        },
        Command::List => match client::request(&config, protocol::ClientMessage::List)? {
            protocol::ServerMessage::Sessions { names } => {
                for name in names {
                    println!("{name}");
                }
            }
            protocol::ServerMessage::Error { message } => return Err(message.into()),
            response => println!("{response:?}"),
        },
        Command::Kill { name } => match client::request(
            &config,
            protocol::ClientMessage::Kill { name: name.clone() },
        )? {
            protocol::ServerMessage::Ok => println!("killed session: {name}"),
            protocol::ServerMessage::Error { message } => return Err(message.into()),
            response => println!("{response:?}"),
        },
        Command::Stop => match client::request(&config, protocol::ClientMessage::Shutdown)? {
            protocol::ServerMessage::Ok => println!("stopped plux daemon"),
            protocol::ServerMessage::Error { message } => return Err(message.into()),
            response => println!("{response:?}"),
        },
        Command::Run { command } => client::run(&config, command)?,
        Command::Daemon => daemon::run(config)?,
        Command::Bridge { start } => transport::bridge(&config, start)?,
        Command::Help => unreachable!(),
    }
    Ok(())
}
