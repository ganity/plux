use std::{
    env,
    fmt::Write as _,
    fs::File,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    process::{Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    execute,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGTERM},
    iterator::Signals,
};

use crate::{
    config::Config,
    error::Result,
    protocol::{read_message, write_message, ClientMessage, CopyMode, ServerMessage},
    socket::{connect, connect_or_start},
    transport::Connection,
};

const MOUSE_SCROLL_LINES: i32 = 3;
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const CLIENT_EVENT_CAPACITY: usize = 128;
type ClientWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub fn request(config: &Config, message: ClientMessage) -> Result<ServerMessage> {
    request_with_stream(connect(config)?, message)
}

pub fn request_or_start(config: &Config, message: ClientMessage) -> Result<ServerMessage> {
    request_with_stream(connect_or_start(config)?, message)
}

fn request_with_stream(mut stream: UnixStream, message: ClientMessage) -> Result<ServerMessage> {
    stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
    write_message(&mut stream, &message)?;
    read_message(&mut stream)?
        .ok_or_else(|| "daemon closed the connection; restart the plux daemon and retry".into())
}

fn attach_stream(
    config: &Config,
    name: &str,
    force: bool,
    rows: u16,
    cols: u16,
    client_token: &str,
    ssh_target: Option<&str>,
) -> Result<Connection> {
    let mut connection = match ssh_target {
        Some(target) => Connection::ssh(target, false)?,
        None => Connection::local(config).map_err(|error| {
            format!(
                "plux daemon is not running or unavailable ({error}); use `plux attach --create {name}` to create a session"
            )
        })?,
    };
    let message = if force {
        ClientMessage::Takeover {
            name: name.to_string(),
            rows,
            cols,
            client_token: client_token.to_string(),
        }
    } else {
        ClientMessage::Attach {
            name: name.to_string(),
            rows,
            cols,
            client_token: client_token.to_string(),
        }
    };
    {
        let mut writer = connection
            .writer
            .lock()
            .map_err(|_| "client writer lock poisoned")?;
        write_message(&mut *writer, &message)?;
    }
    let response = match read_message::<_, ServerMessage>(&mut connection.reader) {
        Ok(response) => response,
        Err(error) => {
            return Err(connection_failure(&mut connection, error.to_string()).into());
        }
    };
    match response {
        Some(ServerMessage::Attached { .. }) => {}
        Some(ServerMessage::Error { message }) => return Err(message.into()),
        Some(response) => return Err(format!("unexpected attach response: {response:?}").into()),
        None => {
            return Err(connection_failure(
                &mut connection,
                "daemon closed the connection during attach; restart the plux daemon and retry",
            )
            .into())
        }
    }
    Ok(connection)
}

fn connection_failure(connection: &mut Connection, message: impl Into<String>) -> String {
    let message = message.into();
    connection.close();
    match connection.stderr_message() {
        Some(stderr) => format!("{message}: {stderr}"),
        None => message,
    }
}

pub fn attach(
    config: &Config,
    name: String,
    force: bool,
    create: bool,
    ssh_target: Option<&str>,
) -> Result<()> {
    let prefix_byte = config.prefix_byte()?;
    let (mut cols, mut rows) = terminal_size()?;
    if create {
        match ssh_target {
            Some(target) => create_session_if_missing_ssh(target, &name, rows, cols)?,
            None => create_session_if_missing(config, &name, rows, cols)?,
        }
    }
    let client_token = generate_client_token()?;
    let mut connection =
        attach_stream(config, &name, force, rows, cols, &client_token, ssh_target)?;
    let mut writer = connection.writer.clone();
    let (events_tx, events_rx) = mpsc::sync_channel(CLIENT_EVENT_CAPACITY);
    let mut connection_generation = 1;
    spawn_server_reader(
        connection.take_reader(),
        events_tx.clone(),
        connection_generation,
    );

    let _guard = TerminalGuard::enter(config.mouse)?;
    spawn_stdin_reader(events_tx.clone());
    spawn_signal_reader(events_tx.clone());

    let mut stdout = io::stdout();
    let mut input = InputState::new(rows, cols, prefix_byte);
    let mut view = TerminalView::new(rows, cols);
    let mut connected = true;
    let mut waiting_snapshot = false;
    let mut next_reconnect = Instant::now();
    let mut reconnect_backoff = Duration::from_millis(500);
    let mut last_heartbeat_sent = Instant::now();
    let mut last_heartbeat_ack = Instant::now();
    loop {
        let (next_cols, next_rows) = terminal_size()?;
        if (next_rows, next_cols) != (rows, cols) {
            rows = next_rows;
            cols = next_cols;
            input.set_size(rows, cols);
            view.set_size(rows, cols);
            if connected {
                if let Err(error) = send(&writer, &ClientMessage::Resize { rows, cols }) {
                    mark_reconnecting(
                        &mut connection,
                        &mut connected,
                        &mut waiting_snapshot,
                        &mut next_reconnect,
                        Instant::now(),
                    );
                    draw_connection_status(
                        &mut stdout,
                        rows,
                        &format!("connection lost: {error}; reconnecting"),
                    )?;
                }
            }
        }

        let now = Instant::now();
        if connected {
            if connection.try_wait()?.is_some() {
                mark_reconnecting(
                    &mut connection,
                    &mut connected,
                    &mut waiting_snapshot,
                    &mut next_reconnect,
                    now,
                );
                draw_connection_status(&mut stdout, rows, "connection exited; reconnecting")?;
            } else if last_heartbeat_ack.elapsed() >= Duration::from_secs(30) {
                mark_reconnecting(
                    &mut connection,
                    &mut connected,
                    &mut waiting_snapshot,
                    &mut next_reconnect,
                    now,
                );
                draw_connection_status(&mut stdout, rows, "heartbeat timeout; reconnecting")?;
            } else if last_heartbeat_sent.elapsed() >= Duration::from_secs(5) {
                if let Err(error) = send(&writer, &ClientMessage::Heartbeat) {
                    mark_reconnecting(
                        &mut connection,
                        &mut connected,
                        &mut waiting_snapshot,
                        &mut next_reconnect,
                        now,
                    );
                    draw_connection_status(
                        &mut stdout,
                        rows,
                        &format!("heartbeat failed: {error}; reconnecting"),
                    )?;
                } else {
                    last_heartbeat_sent = now;
                }
            }
        } else if !waiting_snapshot && now >= next_reconnect {
            connection.close();
            draw_connection_status(&mut stdout, rows, "reconnecting...")?;
            match attach_stream(config, &name, force, rows, cols, &client_token, ssh_target) {
                Ok(mut new_connection) => {
                    connection_generation += 1;
                    writer = new_connection.writer.clone();
                    spawn_server_reader(
                        new_connection.take_reader(),
                        events_tx.clone(),
                        connection_generation,
                    );
                    connection = new_connection;
                    waiting_snapshot = true;
                    last_heartbeat_sent = now;
                    last_heartbeat_ack = now;
                    draw_connection_status(
                        &mut stdout,
                        rows,
                        "reconnecting; waiting for terminal snapshot",
                    )?;
                }
                Err(error) => {
                    if !is_retryable_reconnect_error(&error.to_string()) {
                        return Err(error);
                    }
                    draw_connection_status(
                        &mut stdout,
                        rows,
                        &format!("reconnect failed: {error}"),
                    )?;
                    next_reconnect = now + reconnect_backoff;
                    reconnect_backoff = (reconnect_backoff * 2).min(Duration::from_secs(5));
                }
            }
        } else if waiting_snapshot && last_heartbeat_ack.elapsed() >= Duration::from_secs(30) {
            mark_reconnecting(
                &mut connection,
                &mut connected,
                &mut waiting_snapshot,
                &mut next_reconnect,
                now,
            );
            draw_connection_status(&mut stdout, rows, "snapshot timeout; reconnecting")?;
        }

        match events_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(ClientEvent::Input(bytes)) => {
                if !connected {
                    if bytes.contains(&0x1b) {
                        break;
                    }
                    continue;
                }
                let previous_selection = input.selection_coordinates_if_active();
                let mut reconnect_error = None;
                for action in input.feed(&bytes) {
                    match handle_action(&writer, action) {
                        Ok(true) => {
                            let _ = send(&writer, &ClientMessage::Detach);
                            return Ok(());
                        }
                        Ok(false) => {}
                        Err(error) => {
                            reconnect_error = Some(error.to_string());
                            break;
                        }
                    }
                }
                if let Some(error) = reconnect_error {
                    mark_reconnecting(
                        &mut connection,
                        &mut connected,
                        &mut waiting_snapshot,
                        &mut next_reconnect,
                        Instant::now(),
                    );
                    draw_connection_status(
                        &mut stdout,
                        rows,
                        &format!("input failed: {error}; reconnecting"),
                    )?;
                    continue;
                }
                let current_selection = input.selection_coordinates_if_active();
                if previous_selection != current_selection {
                    view.redraw_selection(&mut stdout, previous_selection, current_selection)?;
                }
                draw_input_status(&mut stdout, &mut input, &view)?;
            }
            Ok(ClientEvent::Server {
                generation,
                message,
            }) if generation == connection_generation => {
                match handle_server_message(config, &mut input, &mut view, &mut stdout, message)? {
                    ServerAction::Detached | ServerAction::SessionFinished => break,
                    ServerAction::HeartbeatAck => last_heartbeat_ack = Instant::now(),
                    ServerAction::Snapshot if waiting_snapshot => {
                        waiting_snapshot = false;
                        connected = true;
                        reconnect_backoff = Duration::from_millis(500);
                        last_heartbeat_ack = Instant::now();
                    }
                    ServerAction::Snapshot | ServerAction::Continue => {}
                }
            }
            Ok(ClientEvent::Server { .. }) => {}
            Ok(ClientEvent::InputClosed) => {
                if connected {
                    let _ = send(&writer, &ClientMessage::Detach);
                }
                break;
            }
            Ok(ClientEvent::ServerClosed {
                generation,
                message,
            }) if generation == connection_generation => {
                mark_reconnecting(
                    &mut connection,
                    &mut connected,
                    &mut waiting_snapshot,
                    &mut next_reconnect,
                    Instant::now(),
                );
                draw_connection_status(&mut stdout, rows, &format!("{message}; reconnecting"))?;
            }
            Ok(ClientEvent::ServerClosed { .. }) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if connected {
                    if let Some(action) = input.tick() {
                        match handle_action(&writer, action) {
                            Ok(true) => {
                                let _ = send(&writer, &ClientMessage::Detach);
                                return Ok(());
                            }
                            Ok(false) => {}
                            Err(error) => {
                                mark_reconnecting(
                                    &mut connection,
                                    &mut connected,
                                    &mut waiting_snapshot,
                                    &mut next_reconnect,
                                    Instant::now(),
                                );
                                draw_connection_status(
                                    &mut stdout,
                                    rows,
                                    &format!("input failed: {error}; reconnecting"),
                                )?;
                            }
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("client event channel closed".into())
            }
        }
    }
    Ok(())
}

enum ServerAction {
    Continue,
    Snapshot,
    HeartbeatAck,
    Detached,
    SessionFinished,
}

fn handle_server_message(
    config: &Config,
    input: &mut InputState,
    view: &mut TerminalView,
    stdout: &mut impl Write,
    message: ServerMessage,
) -> Result<ServerAction> {
    match message {
        ServerMessage::Snapshot {
            rows,
            cols,
            data,
            mouse_enabled,
            alternate_screen,
        } => {
            input.set_size(rows, cols);
            input.set_mouse_enabled(mouse_enabled);
            input.set_alternate_screen(alternate_screen);
            view.set_size(rows, cols);
            view.process(&data);
            stdout.write_all(data.replace("\x1b[2J", "").as_bytes())?;
            view.redraw_selection(stdout, None, input.selection_coordinates_if_active())?;
            stdout.flush()?;
            draw_input_status(stdout, input, view)?;
            Ok(ServerAction::Snapshot)
        }
        ServerMessage::Detached => Ok(ServerAction::Detached),
        ServerMessage::ProcessExited {
            status,
            session_finished,
            ..
        } => {
            writeln!(io::stderr(), "process exited: {status}")?;
            if session_finished {
                Ok(ServerAction::SessionFinished)
            } else {
                Ok(ServerAction::Continue)
            }
        }
        ServerMessage::SearchResult { found: false } => {
            stdout.write_all(b"\x07")?;
            stdout.flush()?;
            Ok(ServerAction::Continue)
        }
        ServerMessage::SearchResult { found: true } => Ok(ServerAction::Continue),
        ServerMessage::Copied { ref text } if copy_to_clipboard(config, text).is_err() => {
            stdout.write_all(b"\x07")?;
            stdout.flush()?;
            Ok(ServerAction::Continue)
        }
        ServerMessage::Copied { .. } => Ok(ServerAction::Continue),
        ServerMessage::HeartbeatAck => Ok(ServerAction::HeartbeatAck),
        ServerMessage::Error { message } => Err(message.into()),
        _ => Ok(ServerAction::Continue),
    }
}

fn mark_reconnecting(
    connection: &mut Connection,
    connected: &mut bool,
    waiting_snapshot: &mut bool,
    next_reconnect: &mut Instant,
    now: Instant,
) {
    connection.close();
    *connected = false;
    *waiting_snapshot = false;
    *next_reconnect = now;
}

fn is_retryable_reconnect_error(message: &str) -> bool {
    !message.contains("another client is already attached")
        && !message.contains("client token does not match")
        && !message.contains("session does not exist")
        && !message.contains("invalid client token")
}

fn draw_connection_status(stdout: &mut impl Write, rows: u16, message: &str) -> Result<()> {
    execute!(
        stdout,
        MoveTo(0, rows.saturating_sub(1)),
        Clear(ClearType::CurrentLine)
    )?;
    write!(stdout, "\x1b[7m {message} \x1b[0m")?;
    stdout.flush()?;
    Ok(())
}

fn draw_input_status(
    stdout: &mut impl Write,
    input: &mut InputState,
    view: &TerminalView,
) -> Result<()> {
    let status = input.status_text();
    if status.is_none() && !input.status_drawn {
        return Ok(());
    }
    if status.is_none() {
        let row = input.rows.saturating_sub(1);
        view.redraw_selection(stdout, Some((row, 0, row, input.cols)), None)?;
        input.status_drawn = false;
        return Ok(());
    }
    execute!(
        stdout,
        MoveTo(0, input.rows.saturating_sub(1)),
        Clear(ClearType::CurrentLine)
    )?;
    if let Some(status) = status {
        stdout.write_all(b"\x1b[?25h")?;
        write!(stdout, "\x1b[7m {status} \x1b[0m")?;
        input.status_drawn = true;
    } else {
        input.status_drawn = false;
    }
    stdout.flush()?;
    Ok(())
}

pub fn run(config: &Config, command: Vec<String>) -> Result<()> {
    config.prefix_byte()?;
    let name = format!("run-{}", std::process::id());
    let (cols, rows) = terminal_size()?;
    match request_or_start(
        config,
        ClientMessage::Create {
            name: name.clone(),
            rows,
            cols,
            command: Some(command),
            temporary: true,
        },
    )? {
        ServerMessage::Created { .. } => {}
        ServerMessage::Error { message } => return Err(message.into()),
        response => return Err(format!("unexpected run response: {response:?}").into()),
    }
    let result = attach(config, name.clone(), false, false, None);
    let _ = request(config, ClientMessage::Kill { name });
    result
}

fn create_session_if_missing(config: &Config, name: &str, rows: u16, cols: u16) -> Result<()> {
    let exists = match request_or_start(config, ClientMessage::List)? {
        ServerMessage::Sessions { names } => names.iter().any(|session| session == name),
        ServerMessage::Error { message } => return Err(message.into()),
        response => return Err(format!("unexpected session list response: {response:?}").into()),
    };
    if exists {
        return Ok(());
    }
    match request_or_start(
        config,
        ClientMessage::Create {
            name: name.to_string(),
            rows,
            cols,
            command: None,
            temporary: false,
        },
    )? {
        ServerMessage::Created { .. } => Ok(()),
        ServerMessage::Error { message } => Err(message.into()),
        response => Err(format!("unexpected create response: {response:?}").into()),
    }
}

fn create_session_if_missing_ssh(target: &str, name: &str, rows: u16, cols: u16) -> Result<()> {
    let exists = match remote_request(target, true, ClientMessage::List)? {
        ServerMessage::Sessions { names } => names.iter().any(|session| session == name),
        ServerMessage::Error { message } => return Err(message.into()),
        response => return Err(format!("unexpected session list response: {response:?}").into()),
    };
    if exists {
        return Ok(());
    }
    match remote_request(
        target,
        true,
        ClientMessage::Create {
            name: name.to_string(),
            rows,
            cols,
            command: None,
            temporary: false,
        },
    )? {
        ServerMessage::Created { .. } => Ok(()),
        ServerMessage::Error { message } => Err(message.into()),
        response => Err(format!("unexpected create response: {response:?}").into()),
    }
}

fn remote_request(target: &str, start: bool, message: ClientMessage) -> Result<ServerMessage> {
    let mut connection = Connection::ssh(target, start)?;
    {
        let mut writer = connection
            .writer
            .lock()
            .map_err(|_| "client writer lock poisoned")?;
        write_message(&mut *writer, &message)?;
    }
    match read_message(&mut connection.reader) {
        Ok(Some(response)) => Ok(response),
        Ok(None) => {
            Err(connection_failure(&mut connection, "remote bridge closed the connection").into())
        }
        Err(error) => Err(connection_failure(&mut connection, error.to_string()).into()),
    }
}

fn terminal_size() -> Result<(u16, u16)> {
    let (cols, rows) = terminal::size()?;
    Ok((cols.max(1), rows.max(2)))
}

fn generate_client_token() -> Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut token = String::with_capacity(32);
    for byte in bytes {
        write!(&mut token, "{byte:02x}").map_err(|_| "failed to format client token")?;
    }
    Ok(token)
}

enum ClientEvent {
    Input(Vec<u8>),
    Server {
        generation: u64,
        message: ServerMessage,
    },
    InputClosed,
    ServerClosed {
        generation: u64,
        message: String,
    },
}

fn send(writer: &ClientWriter, message: &ClientMessage) -> Result<()> {
    let mut writer = writer.lock().map_err(|_| "client writer lock poisoned")?;
    write_message(&mut *writer, message)
}

fn spawn_server_reader<R: Read + Send + 'static>(
    mut reader: R,
    events: mpsc::SyncSender<ClientEvent>,
    generation: u64,
) {
    thread::Builder::new()
        .name("plux-server-reader".to_string())
        .spawn(move || loop {
            match read_message::<_, ServerMessage>(&mut reader) {
                Ok(Some(message)) => {
                    if events
                        .send(ClientEvent::Server {
                            generation,
                            message,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = events.send(ClientEvent::ServerClosed {
                        generation,
                        message: "plux daemon closed the connection".to_string(),
                    });
                    return;
                }
                Err(error) => {
                    let _ = events.send(ClientEvent::ServerClosed {
                        generation,
                        message: format!("plux daemon connection failed: {error}"),
                    });
                    return;
                }
            }
        })
        .expect("failed to spawn server reader thread");
}

fn spawn_stdin_reader(events: mpsc::SyncSender<ClientEvent>) {
    thread::Builder::new()
        .name("plux-stdin-reader".to_string())
        .spawn(move || {
            let mut stdin = io::stdin();
            let mut buffer = [0_u8; 4096];
            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) => {
                        let _ = events.send(ClientEvent::InputClosed);
                        return;
                    }
                    Ok(size) => {
                        if events
                            .send(ClientEvent::Input(buffer[..size].to_vec()))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = events.send(ClientEvent::InputClosed);
                        return;
                    }
                }
            }
        })
        .expect("failed to spawn stdin reader thread");
}

fn spawn_signal_reader(events: mpsc::SyncSender<ClientEvent>) {
    thread::Builder::new()
        .name("plux-signal-reader".to_string())
        .spawn(move || {
            let Ok(mut signals) = Signals::new([SIGHUP, SIGINT, SIGTERM]) else {
                return;
            };
            if signals.forever().next().is_some() {
                let _ = events.send(ClientEvent::InputClosed);
            }
        })
        .expect("failed to spawn plux signal reader thread");
}

struct InputState {
    prefix_byte: u8,
    prefix: bool,
    mode: InputMode,
    query: Vec<u8>,
    last_query: Vec<u8>,
    pending_escape: Vec<u8>,
    escape_started: Option<Instant>,
    mouse_enabled: bool,
    alternate_screen: bool,
    rows: u16,
    cols: u16,
    cursor_row: u16,
    cursor_col: u16,
    selection_anchor: Option<(u16, u16)>,
    selection_mode: CopyMode,
    mouse_selecting: bool,
    status_drawn: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Scroll,
    Search,
    Rename,
}

enum InputAction {
    Forward(Vec<u8>),
    Message(ClientMessage),
    Detach,
}

type Selection = (u16, u16, u16, u16);

struct TerminalView {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
}

impl TerminalView {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            rows,
            cols,
        }
    }

    fn set_size(&mut self, rows: u16, cols: u16) {
        if (self.rows, self.cols) == (rows, cols) {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.screen_mut().set_size(rows, cols);
    }

    fn process(&mut self, data: &str) {
        self.parser.process(data.as_bytes());
    }

    fn redraw_selection(
        &self,
        stdout: &mut impl Write,
        previous: Option<Selection>,
        current: Option<Selection>,
    ) -> Result<()> {
        let Some((first_row, last_row)) = selection_row_range(previous, current, self.rows) else {
            return Ok(());
        };
        let screen = self.parser.screen();
        stdout.write_all(b"\x1b[?25l")?;
        for row in first_row..=last_row {
            let contents = screen
                .rows_formatted(0, self.cols)
                .nth(usize::from(row))
                .unwrap_or_default();
            write!(stdout, "\x1b[{};1H", row + 1)?;
            stdout.write_all(&contents)?;
            stdout.write_all(b"\x1b[K")?;

            let Some((start_col, end_col)) = selection_columns(current, row, self.cols) else {
                continue;
            };
            for col in start_col..end_col {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let contents = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                write!(
                    stdout,
                    "\x1b[{};{}H\x1b[7m{}\x1b[27m",
                    row + 1,
                    col + 1,
                    contents
                )?;
            }
        }

        let (cursor_row, cursor_col) = screen.cursor_position();
        write!(stdout, "\x1b[{};{}H", cursor_row + 1, cursor_col + 1)?;
        if !screen.hide_cursor() {
            stdout.write_all(b"\x1b[?25h")?;
        }
        stdout.flush()?;
        Ok(())
    }
}

fn selection_row_range(
    previous: Option<Selection>,
    current: Option<Selection>,
    rows: u16,
) -> Option<(u16, u16)> {
    let mut range: Option<(u16, u16)> = None;
    for selection in [previous, current].into_iter().flatten() {
        let first = selection.0.min(rows.saturating_sub(1));
        let last = selection.2.min(rows.saturating_sub(1));
        range = Some(match range {
            Some((range_first, range_last)) => (range_first.min(first), range_last.max(last)),
            None => (first, last),
        });
    }
    range
}

fn selection_columns(selection: Option<Selection>, row: u16, cols: u16) -> Option<(u16, u16)> {
    let (start_row, start_col, end_row, end_col) = selection?;
    if row < start_row || row > end_row {
        return None;
    }
    let start = if row == start_row { start_col } else { 0 };
    let end = if row == end_row { end_col } else { cols };
    (start < end).then_some((start.min(cols), end.min(cols)))
}

impl InputState {
    fn new(rows: u16, cols: u16, prefix_byte: u8) -> Self {
        Self {
            prefix_byte,
            prefix: false,
            mode: InputMode::Normal,
            query: Vec::new(),
            last_query: Vec::new(),
            pending_escape: Vec::new(),
            escape_started: None,
            mouse_enabled: false,
            alternate_screen: false,
            rows,
            cols,
            cursor_row: rows.saturating_sub(1),
            cursor_col: 0,
            selection_anchor: None,
            selection_mode: CopyMode::Character,
            mouse_selecting: false,
            status_drawn: false,
        }
    }

    fn set_size(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
    }

    fn set_mouse_enabled(&mut self, enabled: bool) {
        self.mouse_enabled = enabled;
    }

    fn set_alternate_screen(&mut self, enabled: bool) {
        self.alternate_screen = enabled;
    }

    fn selection_coordinates_if_active(&self) -> Option<Selection> {
        self.selection_anchor.map(|_| self.selection_coordinates())
    }

    fn status_text(&self) -> Option<String> {
        match self.mode {
            InputMode::Search => Some(format!("/{}", String::from_utf8_lossy(&self.query))),
            InputMode::Rename => Some(format!("rename: {}", String::from_utf8_lossy(&self.query))),
            InputMode::Scroll if self.selection_anchor.is_some() => Some(format!(
                "select {:?}: {},{}",
                self.selection_mode,
                self.cursor_row + 1,
                self.cursor_col + 1
            )),
            InputMode::Scroll => Some("scroll: k/Up PgUp | j/Down PgDn | q".into()),
            _ => None,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Vec<InputAction> {
        let mut actions = Vec::new();
        let mut input = Vec::with_capacity(self.pending_escape.len() + bytes.len());
        input.extend_from_slice(&self.pending_escape);
        input.extend_from_slice(bytes);
        self.pending_escape.clear();
        self.escape_started = None;

        let mut index = 0;
        while index < input.len() {
            if input[index..].starts_with(b"\x1b[M") {
                if input.len() - index < 6 {
                    self.defer_escape(&input[index..]);
                    break;
                }
                let bytes = &input[index..index + 6];
                let code = u32::from(bytes[3].saturating_sub(32));
                if self.mouse_enabled && self.mode != InputMode::Scroll && code & 4 == 0 {
                    actions.push(InputAction::Forward(bytes.to_vec()));
                } else if self.handle_mouse_selection(
                    code,
                    u16::from(bytes[5].saturating_sub(33)),
                    u16::from(bytes[4].saturating_sub(33)),
                    code & 3 == 3,
                    &mut actions,
                ) {
                } else if code & 64 != 0 {
                    if self.alternate_screen && self.mode != InputMode::Scroll {
                        actions.push(InputAction::Forward(
                            if code & 1 == 0 {
                                b"\x1b[5~"
                            } else {
                                b"\x1b[6~"
                            }
                            .to_vec(),
                        ));
                    } else {
                        if self.mode != InputMode::Scroll {
                            self.enter_scroll_mode();
                        }
                        actions.push(InputAction::Message(ClientMessage::Scroll {
                            rows: if code & 1 == 0 {
                                MOUSE_SCROLL_LINES
                            } else {
                                -MOUSE_SCROLL_LINES
                            },
                        }));
                    }
                }
                index += 6;
                continue;
            }
            if input[index..].starts_with(b"\x1b[<") {
                match parse_sgr_mouse(&input[index..]) {
                    Some((length, code, col, row, release)) => {
                        if self.mouse_enabled && self.mode != InputMode::Scroll && code & 4 == 0 {
                            actions
                                .push(InputAction::Forward(input[index..index + length].to_vec()));
                        } else if self.handle_mouse_selection(code, row, col, release, &mut actions)
                        {
                        } else if code & 64 != 0 {
                            if self.alternate_screen && self.mode != InputMode::Scroll {
                                actions.push(InputAction::Forward(
                                    if code & 1 == 0 {
                                        b"\x1b[5~"
                                    } else {
                                        b"\x1b[6~"
                                    }
                                    .to_vec(),
                                ));
                            } else {
                                if self.mode != InputMode::Scroll {
                                    self.enter_scroll_mode();
                                }
                                actions.push(InputAction::Message(ClientMessage::Scroll {
                                    rows: if code & 1 == 0 {
                                        MOUSE_SCROLL_LINES
                                    } else {
                                        -MOUSE_SCROLL_LINES
                                    },
                                }));
                            }
                        }
                        index += length;
                        continue;
                    }
                    None if escape_sequence_len(&input[index..]).is_none() => {
                        self.defer_escape(&input[index..]);
                        break;
                    }
                    None => {}
                }
            }
            if self.mode == InputMode::Normal {
                if let Some((length, rows)) = parse_direct_scroll_escape(&input[index..]) {
                    if self.alternate_screen {
                        actions.push(InputAction::Forward(input[index..index + length].to_vec()));
                    } else {
                        self.enter_scroll_mode();
                        actions.push(InputAction::Message(ClientMessage::Scroll { rows }));
                    }
                    index += length;
                    continue;
                }
            }

            if input[index] == 27 {
                if self.mode == InputMode::Scroll {
                    if let Some(message) = parse_scroll_escape(&input[index..]) {
                        actions.push(InputAction::Message(message));
                        index += escape_sequence_len(&input[index..]).unwrap();
                        continue;
                    }
                }
                let Some(length) = escape_sequence_len(&input[index..]) else {
                    self.defer_escape(&input[index..]);
                    break;
                };
                actions.push(InputAction::Forward(input[index..index + length].to_vec()));
                index += length;
                continue;
            }

            self.feed_byte(input[index], &mut actions);
            index += 1;
        }
        actions
    }

    fn defer_escape(&mut self, bytes: &[u8]) {
        self.pending_escape.extend_from_slice(bytes);
        self.escape_started = Some(Instant::now());
    }

    fn handle_mouse_selection(
        &mut self,
        code: u32,
        row: u16,
        col: u16,
        release: bool,
        actions: &mut Vec<InputAction>,
    ) -> bool {
        if code & 64 != 0 || (self.mode != InputMode::Scroll && self.mouse_enabled) {
            return false;
        }
        let row = row.min(self.rows.saturating_sub(1));
        let col = col.min(self.cols.saturating_sub(1));
        if self.mouse_selecting {
            self.cursor_row = row;
            self.cursor_col = col;
            if release {
                self.mouse_selecting = false;
                let (start_row, start_col, end_row, end_col) = self.selection_coordinates();
                actions.push(InputAction::Message(ClientMessage::Copy {
                    start_row,
                    start_col,
                    end_row,
                    end_col,
                    mode: self.selection_mode,
                }));
            }
            return true;
        }
        if !release && code & 3 == 0 && code & 32 == 0 {
            if self.mode != InputMode::Scroll {
                self.enter_scroll_mode();
            }
            self.selection_anchor = Some((row, col));
            self.selection_mode = CopyMode::Character;
            self.cursor_row = row;
            self.cursor_col = col;
            self.mouse_selecting = true;
            return true;
        }
        false
    }

    fn enter_scroll_mode(&mut self) {
        self.mode = InputMode::Scroll;
        self.selection_anchor = None;
        self.selection_mode = CopyMode::Character;
        self.cursor_row = self.rows.saturating_sub(1);
        self.cursor_col = 0;
    }

    fn feed_byte(&mut self, byte: u8, actions: &mut Vec<InputAction>) {
        match self.mode {
            InputMode::Normal => {
                if self.prefix {
                    self.prefix = false;
                    match byte {
                        b'd' => actions.push(InputAction::Detach),
                        b'[' => self.enter_scroll_mode(),
                        b'c' => actions.push(InputAction::Message(ClientMessage::Split {
                            vertical: true,
                        })),
                        b'/' => {
                            self.mode = InputMode::Search;
                            self.query.clear();
                        }
                        b'r' => {
                            self.mode = InputMode::Rename;
                            self.query.clear();
                        }
                        b'>' | b'+' => {
                            actions.push(InputAction::Message(ClientMessage::AdjustRatio {
                                delta: 5,
                            }))
                        }
                        b'<' | b'-' => {
                            actions.push(InputAction::Message(ClientMessage::AdjustRatio {
                                delta: -5,
                            }))
                        }
                        b'v' => actions.push(InputAction::Message(ClientMessage::Split {
                            vertical: true,
                        })),
                        b's' => actions.push(InputAction::Message(ClientMessage::Split {
                            vertical: false,
                        })),
                        b'x' => actions.push(InputAction::Message(ClientMessage::ClosePane)),
                        b'z' => actions.push(InputAction::Message(ClientMessage::Zoom)),
                        b'h' | b'j' | b'k' | b'l' => {
                            let direction = match byte {
                                b'h' => "left",
                                b'j' => "down",
                                b'k' => "up",
                                _ => "right",
                            };
                            actions.push(InputAction::Message(ClientMessage::Focus {
                                direction: direction.to_string(),
                            }));
                        }
                        _ => actions.push(InputAction::Forward(vec![self.prefix_byte, byte])),
                    }
                } else if byte == self.prefix_byte {
                    self.prefix = true;
                } else {
                    actions.push(InputAction::Forward(vec![byte]));
                }
            }
            InputMode::Scroll => self.feed_scroll_byte(byte, actions),
            InputMode::Search => match byte {
                13 | 10 => {
                    let query = String::from_utf8_lossy(&self.query).into_owned();
                    self.last_query = self.query.clone();
                    self.mode = InputMode::Scroll;
                    self.query.clear();
                    actions.push(InputAction::Message(ClientMessage::Search {
                        query,
                        direction: 1,
                    }));
                }
                27 => {
                    self.mode = InputMode::Scroll;
                    self.query.clear();
                }
                8 | 127 => {
                    self.query.pop();
                }
                byte if byte >= 0x20 && byte != 0x7f => self.query.push(byte),
                _ => {}
            },
            InputMode::Rename => match byte {
                13 | 10 => {
                    let name = String::from_utf8_lossy(&self.query).into_owned();
                    self.mode = InputMode::Normal;
                    self.query.clear();
                    if !name.is_empty() {
                        actions.push(InputAction::Message(ClientMessage::Rename { name }));
                    }
                }
                27 => {
                    self.mode = InputMode::Normal;
                    self.query.clear();
                }
                8 | 127 => {
                    self.query.pop();
                }
                byte if byte >= 0x20 && byte != 0x7f => self.query.push(byte),
                _ => {}
            },
        }
    }

    fn feed_scroll_byte(&mut self, byte: u8, actions: &mut Vec<InputAction>) {
        match byte {
            b'j' if self.selection_anchor.is_some() => {
                self.cursor_row = self
                    .cursor_row
                    .saturating_add(1)
                    .min(self.rows.saturating_sub(1));
            }
            b'k' if self.selection_anchor.is_some() => {
                self.cursor_row = self.cursor_row.saturating_sub(1);
            }
            b'j' => actions.push(InputAction::Message(ClientMessage::Scroll { rows: -1 })),
            b'k' => actions.push(InputAction::Message(ClientMessage::Scroll { rows: 1 })),
            b'g' => actions.push(InputAction::Message(ClientMessage::ScrollToTop)),
            b'G' => actions.push(InputAction::Message(ClientMessage::ScrollToBottom)),
            b'n' => self.search_again(1, actions),
            b'N' => self.search_again(-1, actions),
            b'v' => {
                self.toggle_selection(CopyMode::Character);
            }
            b'V' => {
                self.toggle_selection(CopyMode::Line);
            }
            0x16 => {
                self.toggle_selection(CopyMode::Rectangle);
            }
            b'h' if self.selection_anchor.is_some() => {
                self.cursor_col = self.cursor_col.saturating_sub(1);
            }
            b'l' if self.selection_anchor.is_some() => {
                self.cursor_col = self
                    .cursor_col
                    .saturating_add(1)
                    .min(self.cols.saturating_sub(1));
            }
            b'y' => {
                let (start_row, start_col, end_row, end_col) = self.selection_coordinates();
                actions.push(InputAction::Message(ClientMessage::Copy {
                    start_row,
                    start_col,
                    end_row,
                    end_col,
                    mode: self.selection_mode,
                }));
            }
            b'/' => {
                self.mode = InputMode::Search;
                self.query.clear();
            }
            b'q' => {
                self.mode = InputMode::Normal;
                self.selection_anchor = None;
                self.selection_mode = CopyMode::Character;
                self.mouse_selecting = false;
                actions.push(InputAction::Message(ClientMessage::ScrollToBottom));
            }
            _ => {}
        }
    }

    fn search_again(&self, direction: i8, actions: &mut Vec<InputAction>) {
        if !self.last_query.is_empty() {
            actions.push(InputAction::Message(ClientMessage::Search {
                query: String::from_utf8_lossy(&self.last_query).into_owned(),
                direction,
            }));
        }
    }

    fn toggle_selection(&mut self, mode: CopyMode) {
        self.mouse_selecting = false;
        if self.selection_anchor.is_some() && self.selection_mode == mode {
            self.selection_anchor = None;
        } else {
            self.selection_anchor = Some((self.cursor_row, self.cursor_col));
            self.selection_mode = mode;
        }
    }

    fn selection_coordinates(&self) -> (u16, u16, u16, u16) {
        let Some((anchor_row, anchor_col)) = self.selection_anchor else {
            return (0, 0, self.rows.saturating_sub(1), self.cols);
        };
        if matches!(self.selection_mode, CopyMode::Line) {
            return (
                anchor_row.min(self.cursor_row),
                0,
                anchor_row.max(self.cursor_row),
                self.cols,
            );
        }
        if (anchor_row, anchor_col) <= (self.cursor_row, self.cursor_col) {
            (
                anchor_row,
                anchor_col,
                self.cursor_row,
                self.cursor_col.saturating_add(1),
            )
        } else {
            (
                self.cursor_row,
                self.cursor_col,
                anchor_row,
                anchor_col.saturating_add(1),
            )
        }
    }

    fn tick(&mut self) -> Option<InputAction> {
        if self
            .escape_started
            .is_some_and(|started| started.elapsed() > Duration::from_millis(50))
        {
            let pending = std::mem::take(&mut self.pending_escape);
            self.escape_started = None;
            if self.mode == InputMode::Scroll {
                self.mode = InputMode::Normal;
                return Some(InputAction::Message(ClientMessage::ScrollToBottom));
            }
            if !pending.is_empty() {
                return Some(InputAction::Forward(pending));
            }
        }
        None
    }
}

fn parse_scroll_escape(bytes: &[u8]) -> Option<ClientMessage> {
    match bytes {
        [27, b'[', b'A'] => Some(ClientMessage::Scroll { rows: 1 }),
        [27, b'[', b'B'] => Some(ClientMessage::Scroll { rows: -1 }),
        [27, b'[', b'H'] => Some(ClientMessage::ScrollToTop),
        [27, b'[', b'F'] => Some(ClientMessage::ScrollToBottom),
        [27, b'[', b'5', b'~'] => Some(ClientMessage::Scroll { rows: 10 }),
        [27, b'[', b'6', b'~'] => Some(ClientMessage::Scroll { rows: -10 }),
        _ => None,
    }
}

fn parse_direct_scroll_escape(bytes: &[u8]) -> Option<(usize, i32)> {
    if bytes.starts_with(b"\x1b[5~") {
        Some((4, 10))
    } else if bytes.starts_with(b"\x1b[6~") {
        Some((4, -10))
    } else {
        None
    }
}

fn escape_sequence_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 2 {
        return None;
    }
    if bytes[1] != b'[' {
        return Some(2);
    }
    bytes[2..]
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))
        .map(|position| position + 3)
}

fn parse_sgr_mouse(bytes: &[u8]) -> Option<(usize, u32, u16, u16, bool)> {
    if !bytes.starts_with(b"\x1b[<") {
        return None;
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == b'M' || *byte == b'm')?;
    let fields = std::str::from_utf8(&bytes[3..end])
        .ok()?
        .split(';')
        .collect::<Vec<_>>();
    if fields.len() != 3 {
        return None;
    }
    Some((
        end + 1,
        fields[0].parse().ok()?,
        fields[1].parse::<u16>().ok()?.saturating_sub(1),
        fields[2].parse::<u16>().ok()?.saturating_sub(1),
        bytes[end] == b'm',
    ))
}

fn handle_action(writer: &ClientWriter, action: InputAction) -> Result<bool> {
    match action {
        InputAction::Forward(bytes) => send(writer, &ClientMessage::Input { bytes })?,
        InputAction::Message(message) => send(writer, &message)?,
        InputAction::Detach => return Ok(true),
    }
    Ok(false)
}

fn copy_to_clipboard(config: &Config, text: &str) -> Result<()> {
    let candidates = if let Some(command) = config.copy_command.as_deref() {
        vec![(command.to_string(), Vec::new())]
    } else if cfg!(target_os = "macos") {
        vec![("pbcopy".to_string(), Vec::new())]
    } else {
        let mut candidates = Vec::new();
        if env::var_os("WAYLAND_DISPLAY").is_some() {
            candidates.push(("wl-copy".to_string(), Vec::new()));
        }
        if env::var_os("DISPLAY").is_some() {
            candidates.push((
                "xclip".to_string(),
                vec!["-selection".to_string(), "clipboard".to_string()],
            ));
            candidates.push((
                "xsel".to_string(),
                vec!["--clipboard".to_string(), "--input".to_string()],
            ));
        }
        candidates
    };

    for (command, args) in candidates {
        let Ok(mut child) = Command::new(&command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        if child.wait()?.success() {
            return Ok(());
        }
    }
    let encoded = base64_encode(text.as_bytes());
    let mut stdout = io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()?;
    Ok(())
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(TABLE[usize::from(first >> 2)] as char);
        encoded.push(TABLE[usize::from((first & 0x03) << 4 | second >> 4)] as char);
        encoded.push(if chunk.len() > 1 {
            TABLE[usize::from((second & 0x0f) << 2 | third >> 6)] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[usize::from(third & 0x3f)] as char
        } else {
            '='
        });
    }
    encoded
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter(mouse: bool) -> Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        ) {
            let _ = terminal::disable_raw_mode();
            return Err(error.into());
        }
        if mouse {
            stdout.write_all(b"\x1b[?1000h\x1b[?1006h")?;
            stdout.flush()?;
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = stdout
            .write_all(b"\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l");
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = stdout.flush();
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use std::{os::unix::net::UnixStream, sync::mpsc, time::Duration};

    use super::{
        draw_input_status, spawn_server_reader, ClientEvent, InputAction, InputState, TerminalView,
    };
    use crate::protocol::ClientMessage;

    #[test]
    fn prefix_detaches_without_forwarding() {
        let mut state = InputState::new(24, 80, 0);
        let actions = state.feed(&[0, b'd']);
        assert!(matches!(actions.as_slice(), [InputAction::Detach]));
    }

    #[test]
    fn regular_input_is_forwarded() {
        let mut state = InputState::new(24, 80, 0);
        let actions = state.feed(b"echo hi\r");
        assert_eq!(actions.len(), 8);
    }

    #[test]
    fn selection_overlay_highlights_and_restores_cells() {
        let mut view = TerminalView::new(2, 10);
        view.process("hello");

        let mut highlighted = Vec::new();
        view.redraw_selection(&mut highlighted, None, Some((0, 1, 0, 4)))
            .unwrap();
        assert_eq!(
            highlighted
                .windows(b"\x1b[7m".len())
                .filter(|window| *window == b"\x1b[7m")
                .count(),
            3
        );

        let mut restored = Vec::new();
        view.redraw_selection(&mut restored, Some((0, 1, 0, 4)), None)
            .unwrap();
        assert!(!restored
            .windows(b"\x1b[7m".len())
            .any(|window| window == b"\x1b[7m"));
        assert!(String::from_utf8_lossy(&restored).contains("hello"));
    }

    #[test]
    fn clearing_status_restores_the_bottom_row() {
        let mut view = TerminalView::new(2, 10);
        view.process("top\r\nbottom");
        let mut input = InputState::new(2, 10, 0);
        input.status_drawn = true;
        let mut rendered = Vec::new();

        draw_input_status(&mut rendered, &mut input, &view).unwrap();

        assert!(String::from_utf8_lossy(&rendered).contains("bottom"));
    }

    #[test]
    fn server_disconnect_is_not_a_normal_client_close() {
        let (reader, writer) = UnixStream::pair().unwrap();
        let (events_tx, events_rx) = mpsc::sync_channel(1);
        spawn_server_reader(reader, events_tx, 1);
        drop(writer);

        assert!(matches!(
            events_rx.recv_timeout(Duration::from_secs(1)),
            Ok(ClientEvent::ServerClosed { .. })
        ));
    }

    #[test]
    fn scroll_prefix_maps_to_scroll_action() {
        let mut state = InputState::new(24, 80, 0);
        let actions = state.feed(&[0, b'[', b'k']);
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Scroll { rows: 1 })]
        ));
    }

    #[test]
    fn search_repeat_and_selection_are_encoded() {
        let mut state = InputState::new(24, 80, 0);
        state.feed(&[0, b'[', b'/', b'a', b'b', 13]);
        let actions = state.feed(b"nN");
        assert!(matches!(
            actions.first(),
            Some(InputAction::Message(ClientMessage::Search {
                direction: 1,
                ..
            }))
        ));
        assert!(matches!(
            actions.get(1),
            Some(InputAction::Message(ClientMessage::Search {
                direction: -1,
                ..
            }))
        ));
        state.feed(b"vll");
        let actions = state.feed(b"y");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Copy {
                start_col: 0,
                end_col: 3,
                ..
            })]
        ));
    }

    #[test]
    fn mouse_wheel_scrolls_when_application_does_not_capture_mouse() {
        let mut state = InputState::new(24, 80, 0);
        let actions = state.feed(b"\x1b[<64;10;5M");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Scroll { rows: 3 })]
        ));
        assert!(matches!(state.mode, super::InputMode::Scroll));
    }

    #[test]
    fn page_up_enters_scroll_mode_directly() {
        let mut state = InputState::new(24, 80, 0);
        let actions = state.feed(b"\x1b[5~");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Scroll { rows: 10 })]
        ));
        assert!(matches!(state.mode, super::InputMode::Scroll));
    }

    #[test]
    fn page_up_is_forwarded_in_alternate_screen() {
        let mut state = InputState::new(24, 80, 0);
        state.set_alternate_screen(true);
        let actions = state.feed(b"\x1b[5~");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Forward(bytes)] if bytes == b"\x1b[5~"
        ));
        assert!(matches!(state.mode, super::InputMode::Normal));
    }

    #[test]
    fn mouse_wheel_maps_to_page_keys_in_alternate_screen() {
        let mut state = InputState::new(24, 80, 0);
        state.set_alternate_screen(true);
        assert!(matches!(
            state.feed(b"\x1b[<64;10;5M").as_slice(),
            [InputAction::Forward(bytes)] if bytes == b"\x1b[5~"
        ));
        assert!(matches!(
            state.feed(b"\x1b[Ma**").as_slice(),
            [InputAction::Forward(bytes)] if bytes == b"\x1b[6~"
        ));
        assert!(matches!(state.mode, super::InputMode::Normal));
    }

    #[test]
    fn split_page_up_is_parsed_as_one_sequence() {
        let mut state = InputState::new(24, 80, 0);
        assert!(state.feed(b"\x1b").is_empty());
        let actions = state.feed(b"[5~");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Scroll { rows: 10 })]
        ));
    }

    #[test]
    fn split_sgr_mouse_is_parsed_as_one_sequence() {
        let mut state = InputState::new(24, 80, 0);
        assert!(state.feed(b"\x1b[<64;10").is_empty());
        let actions = state.feed(b";5M");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Scroll { rows: 3 })]
        ));
    }

    #[test]
    fn mouse_drag_creates_a_copy_selection() {
        let mut state = InputState::new(24, 80, 0);
        assert!(state.feed(b"\x1b[<0;3;2M").is_empty());
        assert!(matches!(state.mode, super::InputMode::Scroll));
        assert_eq!(state.selection_anchor, Some((1, 2)));

        assert!(state.feed(b"\x1b[<32;6;2M").is_empty());
        let actions = state.feed(b"\x1b[<0;6;2m");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Copy {
                start_row: 1,
                start_col: 2,
                end_row: 1,
                end_col: 6,
                mode: crate::protocol::CopyMode::Character,
            })]
        ));
    }

    #[test]
    fn split_unknown_csi_is_forwarded_without_corrupting_input() {
        let mut state = InputState::new(24, 80, 0);
        assert!(state.feed(b"\x1b[1;").is_empty());
        let actions = state.feed(b"2Zx");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Forward(sequence), InputAction::Forward(rest)]
                if sequence == b"\x1b[1;2Z" && rest == b"x"
        ));
    }

    #[test]
    fn captured_mouse_is_forwarded_to_application() {
        let mut state = InputState::new(24, 80, 0);
        state.set_mouse_enabled(true);
        let bytes = b"\x1b[<0;10;5M";
        let actions = state.feed(bytes);
        assert!(matches!(actions.as_slice(), [InputAction::Forward(value)] if value == bytes));
    }

    #[test]
    fn legacy_x10_mouse_wheel_scrolls() {
        let mut state = InputState::new(24, 80, 0);
        let actions = state.feed(b"\x1b[M`**");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Scroll { rows: 3 })]
        ));
        let actions = state.feed(b"\x1b[Ma**");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Scroll { rows: -3 })]
        ));
    }

    #[test]
    fn scroll_mode_takes_over_wheel_from_application() {
        let mut state = InputState::new(24, 80, 0);
        state.set_mouse_enabled(true);
        state.feed(&[0, b'[']);
        let actions = state.feed(b"\x1b[<64;10;5M");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Scroll { rows: 3 })]
        ));
    }

    #[test]
    fn configurable_prefix_and_rename_are_handled() {
        let mut state = InputState::new(24, 80, 1);
        let actions = state.feed(&[1, b'c']);
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Split {
                vertical: true
            })]
        ));
        let actions = state.feed(&[1, b'z']);
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Zoom)]
        ));

        let mut state = InputState::new(24, 80, 0);
        let actions = state.feed(&[0, b'r', b'n', b'e', b'w', b'\n']);
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Message(ClientMessage::Rename { name })] if name == "new"
        ));
    }

    #[test]
    fn clipboard_fallback_encoding_is_valid() {
        assert_eq!(super::base64_encode(b"hello"), "aGVsbG8=");
    }
}
