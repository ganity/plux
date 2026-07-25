use std::{
    collections::VecDeque,
    env,
    io::{Read, Write},
    path::PathBuf,
    sync::{mpsc::SyncSender, Arc, Condvar, Mutex},
    thread,
};

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::{config::Config, error::Result, terminal::TerminalState};

pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;
const MAX_INPUT_BACKLOG: usize = 1024 * 1024;

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
    input: Arc<PaneInput>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

struct PaneInputState {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
}

struct PaneInput {
    state: Mutex<PaneInputState>,
    wake: Condvar,
}

impl PaneInput {
    fn spawn(writer: Box<dyn Write + Send>) -> Result<Arc<Self>> {
        let input = Arc::new(Self {
            state: Mutex::new(PaneInputState {
                queue: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            wake: Condvar::new(),
        });
        let worker = input.clone();
        thread::Builder::new()
            .name("plux-pane-writer".to_string())
            .spawn(move || pane_input_writer(worker, writer))?;
        Ok(input)
    }

    fn enqueue(&self, bytes: Vec<u8>) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().map_err(|_| "pane input lock poisoned")?;
        if state.closed {
            return Err("pane input writer is closed".into());
        }
        if state.bytes.saturating_add(bytes.len()) > MAX_INPUT_BACKLOG {
            return Err("pane input backlog exceeded".into());
        }
        state.bytes += bytes.len();
        state.queue.push_back(bytes);
        self.wake.notify_one();
        Ok(())
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.queue.clear();
            state.bytes = 0;
            self.wake.notify_all();
        }
    }
}

impl Pane {
    pub fn spawn_with_session(
        id: u64,
        config: &Config,
        rows: u16,
        cols: u16,
        session_name: &str,
        command: Option<Vec<String>>,
        events: SyncSender<PaneEvent>,
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
        let input = PaneInput::spawn(writer)?;

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
            input,
            killer,
        })
    }

    pub fn write_input_owned(&mut self, bytes: Vec<u8>) -> Result<()> {
        self.input.enqueue(bytes)
    }

    pub fn process_output(&mut self, bytes: &[u8]) {
        let was_scrolled = self.terminal.is_scrolled();
        self.terminal.process(bytes);
        for reply in self.terminal.take_replies() {
            let _ = self.write_input_owned(reply);
        }
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

impl Drop for Pane {
    fn drop(&mut self) {
        self.input.close();
    }
}

fn pane_input_writer(input: Arc<PaneInput>, mut writer: Box<dyn Write + Send>) {
    loop {
        let bytes = {
            let Ok(mut state) = input.state.lock() else {
                return;
            };
            while state.queue.is_empty() && !state.closed {
                state = input
                    .wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.closed && state.queue.is_empty() {
                return;
            }
            let bytes = state
                .queue
                .pop_front()
                .expect("pane input queue is non-empty");
            state.bytes = state.bytes.saturating_sub(bytes.len());
            bytes
        };
        if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
            input.close();
            return;
        }
    }
}

fn spawn_reader(
    pane_id: u64,
    mut reader: Box<dyn Read + Send>,
    events: SyncSender<PaneEvent>,
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

fn send_exit_event(pane_id: u64, events: &SyncSender<PaneEvent>, exit_state: &ExitState) {
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
        let (events_tx, events_rx) = mpsc::sync_channel(128);
        let config = Config::default();
        let mut pane = Pane::spawn_with_session(1, &config, 8, 80, "", None, events_tx).unwrap();
        pane.write_input_owned(b"printf 'pane-ok\\n'\r".to_vec())
            .unwrap();

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
    fn terminal_query_reply_reaches_child_pty() {
        let (events_tx, events_rx) = mpsc::sync_channel(128);
        let command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            r#"stty raw -echo min 1 time 0; printf '\033[5n'; expected=$(printf '\033[0n'); response=$(for i in 1 2 3 4; do dd bs=1 count=1 2>/dev/null; done); stty sane; if [ "$response" = "$expected" ]; then printf 'DSR_OK\n'; fi"#.to_string(),
        ];
        let mut pane =
            Pane::spawn_with_session(3, &Config::default(), 8, 80, "", Some(command), events_tx)
                .unwrap();

        for _ in 0..10 {
            match events_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap_or_else(|error| panic!("PTY event stream closed: {error}"))
            {
                PaneEvent::Output { bytes, .. } => {
                    pane.process_output(&bytes);
                    if pane.terminal.contents().contains("DSR_OK") {
                        return;
                    }
                }
                PaneEvent::ReaderError { error, .. } => panic!("PTY reader failed: {error}"),
                PaneEvent::Exited { status, .. } => {
                    panic!("child exited before terminal query reply: {status}")
                }
            }
        }
        panic!("terminal query reply did not reach child");
    }

    #[test]
    fn kill_does_not_wait_for_the_child_waiter_lock() {
        let (events_tx, events_rx) = mpsc::sync_channel(128);
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
        let (events_tx, events_rx) = mpsc::sync_channel(128);
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
