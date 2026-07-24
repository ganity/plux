use std::{
    env,
    io::{Read, Write},
    path::PathBuf,
    sync::{mpsc::Sender, Arc, Condvar, Mutex},
    thread,
};

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::{config::Config, error::Result, terminal::TerminalState};

pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;

#[derive(Debug)]
pub enum PaneEvent {
    Output { pane_id: u64, bytes: Vec<u8> },
    Exited { pane_id: u64, status: String },
    ReaderError { pane_id: u64, error: String },
}

type ExitState = Arc<(Mutex<Option<String>>, Condvar)>;

pub struct Pane {
    pub terminal: TerminalState,
    pub unread_output: usize,
    pub command: Vec<String>,
    pub process_id: Option<u32>,
    pub cwd: PathBuf,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl Pane {
    pub fn spawn_with_session(
        id: u64,
        config: &Config,
        rows: u16,
        cols: u16,
        session_name: &str,
        command: Option<Vec<String>>,
        events: Sender<PaneEvent>,
    ) -> Result<Self> {
        let rows = rows.max(2);
        let cols = cols.max(1);
        let command_metadata = command
            .clone()
            .filter(|command| !command.is_empty())
            .unwrap_or_else(|| vec![config.default_shell.clone()]);
        let cwd = env::current_dir()?;
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut command_builder = match command {
            Some(command) if !command.is_empty() => {
                let mut builder = CommandBuilder::new(&command[0]);
                builder.args(&command[1..]);
                builder
            }
            _ => CommandBuilder::new(&config.default_shell),
        };
        command_builder.env("TERM", "xterm-256color");
        command_builder.env("COLORTERM", "truecolor");
        command_builder.env("PLUX", "1");
        command_builder.env("PLUX_SESSION", session_name);
        command_builder.env("PLUX_PANE", id.to_string());
        let child = pair.slave.spawn_command(command_builder)?;
        let child = Arc::new(Mutex::new(child));
        let killer = child
            .lock()
            .map_err(|_| "PTY child lock poisoned")?
            .clone_killer();
        let process_id = child
            .lock()
            .map_err(|_| "PTY child lock poisoned")?
            .process_id();
        let master = Arc::new(Mutex::new(pair.master));
        let reader = master
            .lock()
            .map_err(|_| "PTY master lock poisoned")?
            .try_clone_reader()?;
        let writer = master
            .lock()
            .map_err(|_| "PTY master lock poisoned")?
            .take_writer()?;

        let exit_state = Arc::new((Mutex::new(None), Condvar::new()));
        spawn_reader(id, reader, events.clone(), exit_state.clone());
        spawn_waiter(id, child.clone(), exit_state);

        Ok(Self {
            terminal: TerminalState::with_limits(
                rows,
                cols,
                config.scrollback_lines,
                config.scrollback_bytes,
            ),
            unread_output: 0,
            command: command_metadata,
            process_id,
            cwd,
            master,
            writer,
            killer,
        })
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn process_output(&mut self, bytes: &[u8]) {
        let was_scrolled = self.terminal.is_scrolled();
        self.terminal.process(bytes);
        if was_scrolled {
            let lines = bytes.iter().filter(|byte| **byte == b'\n').count().max(1);
            self.unread_output = self.unread_output.saturating_add(lines);
        } else {
            self.unread_output = 0;
        }
    }

    pub fn clear_unread(&mut self) {
        self.unread_output = 0;
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .lock()
            .map_err(|_| "PTY master lock poisoned")?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        self.terminal.resize(rows, cols);
        Ok(())
    }

    pub fn kill(&mut self) -> Result<()> {
        self.killer.kill()?;
        Ok(())
    }
}

fn spawn_reader(
    pane_id: u64,
    mut reader: Box<dyn Read + Send>,
    events: Sender<PaneEvent>,
    exit_state: ExitState,
) {
    thread::Builder::new()
        .name(format!("plux-pty-reader-{pane_id}"))
        .spawn(move || {
            let mut buffer = [0_u8; 32 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        send_exit_event(pane_id, &events, &exit_state);
                        return;
                    }
                    Ok(size) => {
                        if events
                            .send(PaneEvent::Output {
                                pane_id,
                                bytes: buffer[..size].to_vec(),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = events.send(PaneEvent::ReaderError {
                            pane_id,
                            error: error.to_string(),
                        });
                        send_exit_event(pane_id, &events, &exit_state);
                        return;
                    }
                }
            }
        })
        .expect("failed to spawn PTY reader thread");
}

fn spawn_waiter(
    pane_id: u64,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    exit_state: ExitState,
) {
    thread::Builder::new()
        .name(format!("plux-pty-waiter-{pane_id}"))
        .spawn(move || {
            let status = child
                .lock()
                .map_err(|_| "PTY child lock poisoned".to_string())
                .and_then(|mut child| child.wait().map_err(|error| error.to_string()));
            let status = match status {
                Ok(status) => status.to_string(),
                Err(error) => format!("wait failed: {error}"),
            };
            let (lock, wake) = &*exit_state;
            let mut stored = lock.lock().unwrap_or_else(|error| error.into_inner());
            *stored = Some(status);
            wake.notify_one();
        })
        .expect("failed to spawn PTY waiter thread");
}

fn send_exit_event(pane_id: u64, events: &Sender<PaneEvent>, exit_state: &ExitState) {
    let (lock, wake) = &**exit_state;
    let mut status = lock.lock().unwrap_or_else(|error| error.into_inner());
    while status.is_none() {
        status = wake.wait(status).unwrap_or_else(|error| error.into_inner());
    }
    if let Some(status) = status.take() {
        let _ = events.send(PaneEvent::Exited { pane_id, status });
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use super::{Pane, PaneEvent};
    use crate::config::Config;

    #[test]
    fn shell_writes_to_terminal() {
        let (events_tx, events_rx) = mpsc::channel();
        let config = Config::default();
        let mut pane = Pane::spawn_with_session(1, &config, 8, 80, "", None, events_tx).unwrap();
        pane.write_input(b"printf 'pane-ok\\n'\r").unwrap();

        let mut output = Vec::new();
        for _ in 0..10 {
            match events_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
                PaneEvent::Output { bytes, .. } => {
                    output.extend(bytes);
                    pane.terminal.process(&output);
                    if pane.terminal.contents().contains("pane-ok") {
                        return;
                    }
                    output.clear();
                }
                PaneEvent::ReaderError { error, .. } => panic!("PTY reader failed: {error}"),
                PaneEvent::Exited { .. } => {}
            }
        }
        panic!("shell output did not arrive");
    }

    #[test]
    fn kill_does_not_wait_for_the_child_waiter_lock() {
        let (events_tx, events_rx) = mpsc::channel();
        let mut pane =
            Pane::spawn_with_session(2, &Config::default(), 8, 80, "", None, events_tx).unwrap();
        pane.kill().unwrap();
        assert!(matches!(
            events_rx.recv_timeout(Duration::from_secs(2)),
            Ok(PaneEvent::Exited { .. })
        ));
    }

    #[test]
    fn exit_event_follows_final_output() {
        let (events_tx, events_rx) = mpsc::channel();
        let _pane = Pane::spawn_with_session(
            3,
            &Config::default(),
            8,
            80,
            "",
            Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "printf final-marker".to_string(),
            ]),
            events_tx,
        )
        .unwrap();

        let mut output = Vec::new();
        loop {
            match events_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                PaneEvent::Output { bytes, .. } => output.extend(bytes),
                PaneEvent::ReaderError { error, .. } => panic!("PTY reader failed: {error}"),
                PaneEvent::Exited { .. } => {
                    assert!(String::from_utf8_lossy(&output).contains("final-marker"));
                    break;
                }
            }
        }
    }
}
