use std::{
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};

use crate::{
    config::Config,
    error::Result,
    protocol::{read_message, write_message, ClientMessage},
    socket::connect,
};

const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const SSH_STDERR_LIMIT: u64 = 8 * 1024;
const REMOTE_BRIDGE_COMMAND: &str = r#"PATH="$HOME/.cargo/bin:$PATH" exec plux __bridge"#;

pub struct Connection {
    pub reader: Box<dyn Read + Send>,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Option<Child>,
    stderr: Option<Arc<Mutex<String>>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl Connection {
    pub fn local(config: &Config) -> Result<Self> {
        let stream = connect(config)?;
        stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
        Self::from_unix_stream(stream)
    }

    pub fn ssh(target: &str, start: bool) -> Result<Self> {
        let mut command = Command::new("ssh");
        command
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=4",
                "-o",
                "ConnectTimeout=10",
                target,
                REMOTE_BRIDGE_COMMAND,
            ])
            .args(start.then_some("--start"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let writer = match child.stdin.take() {
            Some(writer) => writer,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("ssh stdin pipe was not created".into());
            }
        };
        let reader = match child.stdout.take() {
            Some(reader) => reader,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("ssh stdout pipe was not created".into());
            }
        };
        let mut stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("ssh stderr pipe was not created".into());
            }
        };
        let stderr_output = Arc::new(Mutex::new(String::new()));
        let output = Arc::clone(&stderr_output);
        let stderr_thread = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr
                .by_ref()
                .take(SSH_STDERR_LIMIT)
                .read_to_end(&mut bytes);
            if let Ok(mut message) = output.lock() {
                *message = String::from_utf8_lossy(&bytes).trim().to_string();
            }
        });
        Ok(Self {
            reader: Box::new(reader),
            writer: Arc::new(Mutex::new(Box::new(writer))),
            child: Some(child),
            stderr: Some(stderr_output),
            stderr_thread: Some(stderr_thread),
        })
    }

    pub fn close(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }

    pub fn take_reader(&mut self) -> Box<dyn Read + Send> {
        std::mem::replace(&mut self.reader, Box::new(io::empty()))
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child
            .as_mut()
            .map_or(Ok(None), |child| Ok(child.try_wait()?))
    }

    pub fn stderr_message(&self) -> Option<String> {
        self.stderr
            .as_ref()
            .and_then(|stderr| stderr.lock().ok().map(|message| message.clone()))
            .filter(|message| !message.is_empty())
    }

    fn from_unix_stream(stream: UnixStream) -> Result<Self> {
        Ok(Self {
            reader: Box::new(stream.try_clone()?),
            writer: Arc::new(Mutex::new(Box::new(stream))),
            child: None,
            stderr: None,
            stderr_thread: None,
        })
    }
}

pub fn bridge(config: &Config, start: bool) -> Result<()> {
    let first_message = {
        let mut stdin = io::stdin().lock();
        read_message::<_, ClientMessage>(&mut stdin)?
    };
    let Some(first_message) = first_message else {
        return Ok(());
    };
    let stream = if start {
        crate::socket::connect_or_start(config)?
    } else {
        connect(config)?
    };
    let mut socket_writer = stream.try_clone()?;
    write_message(&mut socket_writer, &first_message)?;
    let mut socket_reader = stream;
    let socket_shutdown = socket_reader.try_clone()?;
    let stdin_thread = std::thread::Builder::new()
        .name("plux-bridge-stdin".to_string())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let _ = io::copy(&mut stdin, &mut socket_writer);
            let _ = socket_writer.shutdown(Shutdown::Write);
        })?;
    let mut stdout = io::stdout().lock();
    let mut buffer = [0_u8; 8 * 1024];
    let result = loop {
        match socket_reader.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(size) => {
                stdout.write_all(&buffer[..size])?;
                stdout.flush()?;
            }
            Err(error) => break Err(error),
        }
    };
    if result.is_err() {
        let _ = socket_shutdown.shutdown(Shutdown::Both);
    } else {
        let _ = socket_shutdown.shutdown(Shutdown::Write);
    }
    stdin_thread
        .join()
        .map_err(|_| "bridge stdin thread panicked")?;
    result.map_err(Into::into)
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::{Connection, REMOTE_BRIDGE_COMMAND};

    #[test]
    fn remote_bridge_finds_cargo_installed_plux() {
        assert_eq!(
            REMOTE_BRIDGE_COMMAND,
            r#"PATH="$HOME/.cargo/bin:$PATH" exec plux __bridge"#
        );
    }

    #[test]
    fn local_connection_interface_is_available() {
        assert!(std::mem::size_of::<Connection>() > 0);
    }
}
