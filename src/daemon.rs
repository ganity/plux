use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs, io,
    net::Shutdown,
    os::unix::fs::PermissionsExt,
    os::unix::net::{UnixListener, UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGTERM},
    iterator::Signals,
};

use crate::{
    config::Config,
    error::Result,
    layout::{FocusDirection, SplitDirection},
    pane::PaneEvent,
    protocol::{read_message, validate_client_token, write_message, ClientMessage, ServerMessage},
    session::{validate_name, Session, SessionOptions},
    socket::{remove_socket, set_private_socket, socket_path},
};

const CLIENT_LEASE_TIMEOUT: Duration = Duration::from_secs(30);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);
const CLIENT_OUTPUT_QUEUE_CAPACITY: usize = 64;
const CONTROL_EVENT_CAPACITY: usize = 256;
const PANE_EVENT_CAPACITY: usize = 128;
const CONTROL_EVENT_BUDGET: usize = 64;
const PANE_EVENT_BUDGET: usize = 64;

enum Event {
    ClientMessage {
        client_id: u64,
        message: ClientMessage,
    },
    ClientDisconnected(u64),
    SnapshotWritten(u64),
    ClientWriteFailed(u64),
}

struct Client {
    id: u64,
    token: Option<String>,
    last_seen: Arc<Mutex<Instant>>,
    writer: Arc<ClientWriter>,
    session: Option<String>,
    snapshot_in_flight: bool,
}

struct SearchTask {
    session_name: String,
    query: String,
    direction: i8,
    original: usize,
    maximum: usize,
    next: usize,
}

struct PendingExit {
    session_name: String,
    pane_id: u64,
    status: String,
    session_finished: bool,
}

struct PendingResize {
    rows: u16,
    cols: u16,
    requested_at: Instant,
}

struct QueuedMessage {
    message: ServerMessage,
    snapshot: bool,
}

struct WriterState {
    queue: VecDeque<QueuedMessage>,
    writing: bool,
    closed: bool,
}

struct ClientWriter {
    stream: Mutex<UnixStream>,
    state: Mutex<WriterState>,
    wake: Condvar,
}

impl ClientWriter {
    fn new(
        stream: UnixStream,
        events: mpsc::SyncSender<Event>,
        client_id: u64,
    ) -> Result<Arc<Self>> {
        let writer = Arc::new(Self {
            stream: Mutex::new(stream),
            state: Mutex::new(WriterState {
                queue: VecDeque::new(),
                writing: false,
                closed: false,
            }),
            wake: Condvar::new(),
        });
        let worker = writer.clone();
        thread::Builder::new()
            .name("plux-client-writer".to_string())
            .spawn(move || client_writer(worker, events, client_id))?;
        Ok(writer)
    }

    fn enqueue(&self, message: ServerMessage, snapshot: bool) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "client writer lock poisoned")?;
        if state.closed {
            return Err("client writer is closed".into());
        }
        if state.queue.len() >= CLIENT_OUTPUT_QUEUE_CAPACITY {
            return Err("client output queue is full".into());
        }
        state.queue.push_back(QueuedMessage { message, snapshot });
        self.wake.notify_one();
        Ok(())
    }

    fn wait_idle(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        while state.writing || !state.queue.is_empty() {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            self.wake.notify_all();
        }
    }
}

impl SearchTask {
    fn offset(&self) -> Option<usize> {
        if self.next > self.maximum || (self.direction < 0 && self.next >= self.maximum) {
            return None;
        }
        if self.direction >= 0 {
            let first = self.maximum.saturating_sub(self.original);
            if self.next < first {
                Some(self.original + 1 + self.next)
            } else {
                Some(self.next - first)
            }
        } else if self.next < self.original {
            Some(self.original - 1 - self.next)
        } else {
            Some(self.maximum - (self.next - self.original))
        }
    }
}

pub fn run(config: Config) -> Result<()> {
    Daemon::new(config)?.run()
}

struct Daemon {
    config: Config,
    listener: UnixListener,
    socket_path: std::path::PathBuf,
    sessions: HashMap<String, Session>,
    events_tx: mpsc::SyncSender<Event>,
    events_rx: mpsc::Receiver<Event>,
    pane_events_tx: mpsc::SyncSender<PaneEvent>,
    pane_events_rx: mpsc::Receiver<PaneEvent>,
    clients: HashMap<u64, Client>,
    next_client_id: u64,
    next_pane_id: u64,
    pending_resizes: HashMap<String, PendingResize>,
    pending_snapshots: HashSet<String>,
    pending_exits: Vec<PendingExit>,
    last_snapshot: Instant,
    search_tasks: HashMap<u64, SearchTask>,
    shutdown_requested: Arc<AtomicBool>,
}

impl Daemon {
    fn new(config: Config) -> Result<Self> {
        let socket_path = socket_path(&config)?;
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                for _ in 0..5 {
                    match UnixStream::connect(&socket_path) {
                        Ok(_) => return Err("plux daemon is already running".into()),
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                            ) =>
                        {
                            thread::sleep(Duration::from_millis(20))
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                remove_socket(&socket_path)?;
                UnixListener::bind(&socket_path)?
            }
            Err(error) => return Err(error.into()),
        };
        set_private_socket(&socket_path)?;
        listener.set_nonblocking(true)?;
        let (events_tx, events_rx) = mpsc::sync_channel(CONTROL_EVENT_CAPACITY);
        let (pane_events_tx, pane_events_rx) = mpsc::sync_channel(PANE_EVENT_CAPACITY);
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let signal_shutdown = shutdown_requested.clone();
        thread::Builder::new()
            .name("plux-signal-reader".to_string())
            .spawn(move || {
                let Ok(mut signals) = Signals::new([SIGHUP, SIGINT, SIGTERM]) else {
                    return;
                };
                if signals.forever().next().is_some() {
                    signal_shutdown.store(true, Ordering::Release);
                }
            })?;
        Ok(Self {
            config,
            listener,
            socket_path,
            sessions: HashMap::new(),
            events_tx,
            events_rx,
            pane_events_tx,
            pane_events_rx,
            clients: HashMap::new(),
            next_client_id: 1,
            next_pane_id: 1,
            pending_resizes: HashMap::new(),
            pending_snapshots: HashSet::new(),
            pending_exits: Vec::new(),
            last_snapshot: Instant::now(),
            search_tasks: HashMap::new(),
            shutdown_requested,
        })
    }

    fn run(mut self) -> Result<()> {
        let result = self.event_loop();
        let _ = remove_socket(&self.socket_path);
        result
    }

    fn event_loop(&mut self) -> Result<()> {
        loop {
            self.expire_stale_clients();
            if self.shutdown_requested.load(Ordering::Acquire) {
                self.wait_for_client_writers();
                self.shutdown_sessions();
                return Ok(());
            }
            self.accept_client()?;
            match self.events_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(event) => self.handle_event(event)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
            for _ in 1..CONTROL_EVENT_BUDGET {
                let Ok(event) = self.events_rx.try_recv() else {
                    break;
                };
                self.handle_event(event)?;
            }
            for _ in 0..PANE_EVENT_BUDGET {
                let Ok(event) = self.pane_events_rx.try_recv() else {
                    break;
                };
                self.handle_pane_event(event)?;
            }
            self.process_search_steps()?;
            self.flush_snapshots()?;
        }
    }

    fn process_search_steps(&mut self) -> Result<()> {
        let client_ids = self.search_tasks.keys().copied().collect::<Vec<_>>();
        for client_id in client_ids {
            self.process_search_step(client_id)?;
        }
        Ok(())
    }

    fn process_search_step(&mut self, client_id: u64) -> Result<()> {
        const SEARCH_STEP: usize = 64;
        let Some(task) = self.search_tasks.get(&client_id) else {
            return Ok(());
        };
        let session_name = task.session_name.clone();
        let query = task.query.clone();
        let mut found = false;
        let mut missing = false;

        for _ in 0..SEARCH_STEP {
            let Some(offset) = self
                .search_tasks
                .get(&client_id)
                .and_then(SearchTask::offset)
            else {
                break;
            };
            if let Some(task) = self.search_tasks.get_mut(&client_id) {
                task.next = task.next.saturating_add(1);
            }
            let matches = self
                .sessions
                .get_mut(&session_name)
                .and_then(|session| session.panes.get_mut(&session.focused))
                .map(|pane| {
                    pane.terminal.set_scrollback(offset);
                    pane.terminal.screen().contents().contains(&query)
                });
            match matches {
                Some(true) => {
                    found = true;
                    break;
                }
                Some(false) => {}
                None => {
                    missing = true;
                    break;
                }
            }
        }

        let finished = found
            || missing
            || self
                .search_tasks
                .get(&client_id)
                .is_some_and(|task| task.offset().is_none());
        if !finished {
            return Ok(());
        }

        let task = self
            .search_tasks
            .remove(&client_id)
            .ok_or("search task disappeared")?;
        if !found {
            if let Some(session) = self.sessions.get_mut(&task.session_name) {
                if let Some(pane) = session.panes.get_mut(&session.focused) {
                    pane.terminal.set_scrollback(task.original);
                }
            }
        }
        self.send_to(client_id, ServerMessage::SearchResult { found })?;
        self.send_attached_snapshot(client_id)
    }

    fn cancel_search(&mut self, client_id: u64) -> Result<()> {
        let Some(task) = self.search_tasks.remove(&client_id) else {
            return Ok(());
        };
        if let Some(session) = self.sessions.get_mut(&task.session_name) {
            if let Some(pane) = session.panes.get_mut(&session.focused) {
                pane.terminal.set_scrollback(task.original);
            }
        }
        Ok(())
    }

    fn flush_snapshots(&mut self) -> Result<()> {
        let now = Instant::now();
        let ready = self
            .pending_resizes
            .iter()
            .filter(|(_, resize)| now.duration_since(resize.requested_at) >= RESIZE_DEBOUNCE)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in ready {
            let Some(resize) = self.pending_resizes.remove(&name) else {
                continue;
            };
            if let Some(session) = self.sessions.get_mut(&name) {
                session.resize(resize.rows, resize.cols)?;
                self.pending_snapshots.insert(name);
            }
        }
        let interval =
            Duration::from_micros(1_000_000_u64 / u64::from(self.config.refresh_rate.max(1)));
        if self.pending_snapshots.is_empty() && self.pending_exits.is_empty()
            || self.last_snapshot.elapsed() < interval
        {
            return Ok(());
        }
        let busy_clients = self
            .clients
            .values()
            .filter(|client| client.snapshot_in_flight)
            .map(|client| client.id)
            .collect::<HashSet<_>>();
        let names = self.pending_snapshots.drain().collect::<Vec<_>>();
        for name in names {
            let busy = self
                .client_id_for_session(&name)
                .is_some_and(|client_id| busy_clients.contains(&client_id));
            if busy || self.pending_resizes.contains_key(&name) {
                self.pending_snapshots.insert(name);
            } else {
                self.send_snapshot(&name)?;
            }
        }
        let exits = std::mem::take(&mut self.pending_exits);
        let mut deferred_exits = Vec::new();
        for exit in exits {
            if let Some(client_id) = self.client_id_for_session(&exit.session_name) {
                if busy_clients.contains(&client_id)
                    || self.pending_resizes.contains_key(&exit.session_name)
                    || self.pending_snapshots.contains(&exit.session_name)
                {
                    deferred_exits.push(exit);
                    continue;
                }
                self.send_to(
                    client_id,
                    ServerMessage::ProcessExited {
                        pane_id: exit.pane_id,
                        status: exit.status,
                        session_finished: exit.session_finished,
                    },
                )?;
            }
        }
        self.pending_exits.extend(deferred_exits);
        self.last_snapshot = Instant::now();
        Ok(())
    }

    fn shutdown_sessions(&mut self) {
        let names = self.sessions.keys().cloned().collect::<Vec<_>>();
        for session in self.sessions.values_mut() {
            for pane in session.panes.values_mut() {
                let _ = pane.kill();
            }
        }
        self.sessions.clear();
        for name in names {
            let _ = self.remove_metadata(&name);
        }
    }

    fn accept_client(&mut self) -> Result<()> {
        let accepted = match self.listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut stream = accepted;
        stream.set_nonblocking(false)?;
        if let Err(error) = stream.set_read_timeout(Some(Duration::from_millis(100))) {
            if error.kind() == io::ErrorKind::InvalidInput {
                return Ok(());
            }
            return Err(error.into());
        }
        let message = read_message::<_, ClientMessage>(&mut stream).ok().flatten();
        if message.is_some() {
            if let Err(error) = stream.set_read_timeout(None) {
                if error.kind() == io::ErrorKind::InvalidInput {
                    return Ok(());
                }
                return Err(error.into());
            }
        }
        match message {
            Some(ClientMessage::Attach {
                name,
                rows,
                cols,
                client_token,
            }) => {
                if let Err(error) = validate_client_token(&client_token)
                    .and_then(|_| validate_name(&name))
                    .and_then(|_| self.validate_attach_target(&name))
                {
                    self.reject_client(stream, error.to_string());
                    return Ok(());
                }
                let existing = self.client_id_for_session(&name);
                let same_client = existing
                    .and_then(|client_id| self.clients.get(&client_id))
                    .and_then(|client| client.token.as_deref())
                    == Some(client_token.as_str());
                if existing.is_none()
                    || same_client
                    || existing.is_some_and(|client_id| self.client_lease_expired(client_id))
                {
                    self.replace_client(
                        existing,
                        stream,
                        ClientMessage::Attach {
                            name,
                            rows,
                            cols,
                            client_token: client_token.clone(),
                        },
                        client_token,
                    )?;
                } else {
                    let _ = write_message(
                        &mut stream,
                        &ServerMessage::Error {
                            message: "another client is already attached".to_string(),
                        },
                    );
                }
            }
            Some(ClientMessage::Takeover {
                name,
                rows,
                cols,
                client_token,
            }) => {
                if let Err(error) = validate_client_token(&client_token)
                    .and_then(|_| validate_name(&name))
                    .and_then(|_| self.validate_attach_target(&name))
                {
                    self.reject_client(stream, error.to_string());
                    return Ok(());
                }
                let existing = self.client_id_for_session(&name);
                self.replace_client(
                    existing,
                    stream,
                    ClientMessage::Takeover {
                        name,
                        rows,
                        cols,
                        client_token: client_token.clone(),
                    },
                    client_token,
                )?;
            }
            Some(message @ ClientMessage::Create { .. })
            | Some(message @ ClientMessage::List)
            | Some(message @ ClientMessage::Kill { .. })
            | Some(message @ ClientMessage::Shutdown) => {
                self.handle_short_request(stream, message)?;
            }
            _ => {
                let _ = write_message(
                    &mut stream,
                    &ServerMessage::Error {
                        message: "expected Attach or Takeover as the first message".to_string(),
                    },
                );
            }
        }
        Ok(())
    }

    fn reject_client(&self, mut stream: UnixStream, message: String) {
        let _ = write_message(&mut stream, &ServerMessage::Error { message });
        let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
        if matches!(
            read_message::<_, ClientMessage>(&mut stream),
            Ok(Some(ClientMessage::Detach))
        ) {
            let _ = write_message(&mut stream, &ServerMessage::Detached);
        }
    }

    fn install_client(&mut self, stream: UnixStream, token: Option<String>) -> Result<u64> {
        let reader = stream.try_clone()?;
        let events = self.events_tx.clone();
        let client_id = self.next_client_id;
        self.next_client_id += 1;
        let last_seen = Arc::new(Mutex::new(Instant::now()));
        let writer = ClientWriter::new(stream, events.clone(), client_id)?;
        thread::Builder::new()
            .name("plux-client-reader".to_string())
            .spawn({
                let last_seen = last_seen.clone();
                move || client_reader(reader, events, client_id, last_seen)
            })?;
        self.clients.insert(
            client_id,
            Client {
                id: client_id,
                token,
                last_seen,
                writer,
                session: None,
                snapshot_in_flight: false,
            },
        );
        Ok(client_id)
    }

    fn replace_client(
        &mut self,
        existing_client_id: Option<u64>,
        stream: UnixStream,
        message: ClientMessage,
        token: String,
    ) -> Result<()> {
        if let Some(client_id) = existing_client_id {
            self.detach_client(client_id);
        }
        let client_id = self.install_client(stream, Some(token))?;
        if let Err(error) = self.handle_client_message(client_id, message) {
            let _ = self.send_to(
                client_id,
                ServerMessage::Error {
                    message: error.to_string(),
                },
            );
            self.detach_client(client_id);
        }
        Ok(())
    }

    fn handle_short_request(&mut self, stream: UnixStream, message: ClientMessage) -> Result<()> {
        let client_id = self.install_client(stream, None)?;
        let writer = self
            .clients
            .get(&client_id)
            .ok_or("short request client disappeared")?
            .writer
            .clone();
        if let Err(error) = self.handle_client_message(client_id, message) {
            let _ = self.send_to(
                client_id,
                ServerMessage::Error {
                    message: error.to_string(),
                },
            );
        }
        writer.wait_idle();
        self.detach_client(client_id);
        Ok(())
    }

    fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::ClientMessage { client_id, message } => {
                if !self.clients.contains_key(&client_id) {
                    return Ok(());
                }
                if let Err(error) = self.handle_client_message(client_id, message) {
                    let _ = self.send_to(
                        client_id,
                        ServerMessage::Error {
                            message: error.to_string(),
                        },
                    );
                }
                Ok(())
            }
            Event::ClientDisconnected(client_id) => {
                if self.clients.contains_key(&client_id) {
                    self.detach_client(client_id);
                    self.cleanup_finished_temporary();
                }
                Ok(())
            }
            Event::SnapshotWritten(client_id) => {
                if let Some(client) = self.clients.get_mut(&client_id) {
                    client.snapshot_in_flight = false;
                }
                Ok(())
            }
            Event::ClientWriteFailed(client_id) => {
                if self.clients.contains_key(&client_id) {
                    self.detach_client(client_id);
                    self.cleanup_finished_temporary();
                }
                Ok(())
            }
        }
    }

    fn handle_client_message(&mut self, client_id: u64, message: ClientMessage) -> Result<()> {
        let attached = self
            .clients
            .get(&client_id)
            .is_some_and(|client| client.session.is_some());
        let handshake_or_short_request = matches!(
            &message,
            ClientMessage::Attach { .. }
                | ClientMessage::Takeover { .. }
                | ClientMessage::Create { .. }
                | ClientMessage::List
                | ClientMessage::Kill { .. }
                | ClientMessage::Shutdown
                | ClientMessage::Detach
                | ClientMessage::Ping
        );
        if !attached && !handshake_or_short_request {
            let result = self.send_to(
                client_id,
                ServerMessage::Error {
                    message: "client must attach before sending interactive messages".to_string(),
                },
            );
            self.detach_client(client_id);
            return result;
        }
        if !matches!(
            &message,
            ClientMessage::Search { .. } | ClientMessage::Heartbeat
        ) {
            self.cancel_search(client_id)?;
        }
        match message {
            ClientMessage::Create {
                name,
                rows,
                cols,
                command,
                temporary,
            } => {
                self.create_session(name.clone(), rows, cols, command, temporary)?;
                self.send_to(client_id, ServerMessage::Created { name })?;
                self.detach_client(client_id);
                Ok(())
            }
            ClientMessage::Attach {
                name,
                rows,
                cols,
                client_token,
            } => {
                validate_client_token(&client_token)?;
                if let Some(client) = self.clients.get_mut(&client_id) {
                    if let Some(current_token) = client.token.as_ref() {
                        if current_token != &client_token {
                            return self.send_to(
                                client_id,
                                ServerMessage::Error {
                                    message: "client token does not match the active connection"
                                        .to_string(),
                                },
                            );
                        }
                    } else {
                        client.token = Some(client_token);
                    }
                }
                if self
                    .clients
                    .get(&client_id)
                    .and_then(|client| client.session.as_ref())
                    .is_some()
                {
                    return self.send_to(
                        client_id,
                        ServerMessage::Error {
                            message: "client is already attached".to_string(),
                        },
                    );
                }
                self.attach_session(client_id, name, rows, cols)
            }
            ClientMessage::Takeover {
                name,
                rows,
                cols,
                client_token,
            } => {
                validate_client_token(&client_token)?;
                if let Some(client) = self.clients.get_mut(&client_id) {
                    client.token = Some(client_token);
                }
                self.attach_session(client_id, name, rows, cols)
            }
            ClientMessage::List => {
                let mut names = self.sessions.keys().cloned().collect::<Vec<_>>();
                names.sort();
                self.send_to(client_id, ServerMessage::Sessions { names })?;
                self.detach_client(client_id);
                Ok(())
            }
            ClientMessage::Kill { name } => {
                validate_name(&name)?;
                if self
                    .client_id_for_session(&name)
                    .is_some_and(|attached_id| attached_id != client_id)
                {
                    return self.send_to(
                        client_id,
                        ServerMessage::Error {
                            message:
                                "cannot kill the currently attached session from another client"
                                    .to_string(),
                        },
                    );
                }
                self.clear_pending_session(&name);
                if let Some(mut session) = self.sessions.remove(&name) {
                    for pane in session.panes.values_mut() {
                        let _ = pane.kill();
                    }
                }
                self.remove_metadata(&name)?;
                if self.attached_session_name(client_id).as_deref() == Some(&name) {
                    self.send_to(client_id, ServerMessage::Detached)?;
                    self.detach_client(client_id);
                    return Ok(());
                }
                self.send_to(client_id, ServerMessage::Ok)?;
                self.detach_client(client_id);
                Ok(())
            }
            ClientMessage::Shutdown => {
                self.send_to(client_id, ServerMessage::Ok)?;
                self.shutdown_requested.store(true, Ordering::Release);
                Ok(())
            }
            ClientMessage::Input { bytes } => {
                if let Some(session) = self.attached_session_mut(client_id) {
                    if let Some(pane) = session.focused_pane_mut() {
                        pane.write_input_owned(bytes)?;
                    }
                }
                Ok(())
            }
            ClientMessage::Resize { rows, cols } => {
                let session_name = self.attached_session_name(client_id);
                if let Some(name) = session_name {
                    self.pending_resizes.insert(
                        name,
                        PendingResize {
                            rows,
                            cols,
                            requested_at: Instant::now(),
                        },
                    );
                }
                Ok(())
            }
            ClientMessage::Scroll { rows } => {
                self.with_focused_pane(client_id, |pane| {
                    pane.terminal.scroll(rows);
                    if !pane.terminal.is_scrolled() {
                        pane.clear_unread();
                    }
                })?;
                self.send_attached_snapshot(client_id)
            }
            ClientMessage::ScrollToTop => {
                self.with_focused_pane(client_id, |pane| pane.terminal.scroll_to_top())?;
                self.send_attached_snapshot(client_id)
            }
            ClientMessage::ScrollToBottom => {
                self.with_focused_pane(client_id, |pane| {
                    pane.terminal.scroll_to_bottom();
                    pane.clear_unread();
                })?;
                self.send_attached_snapshot(client_id)
            }
            ClientMessage::Search { query, direction } => {
                self.cancel_search(client_id)?;
                let session_name = self
                    .attached_session_name(client_id)
                    .ok_or("no attached session")?;
                let (original, maximum) = self.with_focused_pane(client_id, |pane| {
                    let original = pane.terminal.screen().scrollback();
                    pane.terminal.scroll_to_top();
                    let maximum = pane.terminal.screen().scrollback();
                    pane.terminal.set_scrollback(original);
                    (original, maximum)
                })?;
                self.search_tasks.insert(
                    client_id,
                    SearchTask {
                        session_name,
                        query,
                        direction,
                        original,
                        maximum,
                        next: 0,
                    },
                );
                Ok(())
            }
            ClientMessage::Copy {
                start_row,
                start_col,
                end_row,
                end_col,
                mode,
            } => {
                let text = self
                    .attached_session_mut(client_id)
                    .and_then(|session| session.focused_pane_mut())
                    .map(|pane| {
                        pane.terminal
                            .selection_text(start_row, start_col, end_row, end_col, mode)
                    })
                    .unwrap_or_default();
                self.send_to(client_id, ServerMessage::Copied { text })
            }
            ClientMessage::Split { vertical } => {
                let direction = if vertical {
                    SplitDirection::Vertical
                } else {
                    SplitDirection::Horizontal
                };
                let new_pane_id = self.next_pane_id;
                self.next_pane_id += 1;
                let config = self.config.clone();
                let events = self.pane_events_tx.clone();
                if let Some(session) = self.attached_session_mut(client_id) {
                    session.split(new_pane_id, direction, &config, events)?;
                }
                if let Some(name) = self.attached_session_name(client_id) {
                    self.persist_session(&name)?;
                }
                self.send_attached_snapshot(client_id)
            }
            ClientMessage::ClosePane => {
                let Some(name) = self.attached_session_name(client_id) else {
                    return Ok(());
                };
                let close_session = self
                    .sessions
                    .get(&name)
                    .is_some_and(|session| session.panes.len() == 1);
                if close_session {
                    self.clear_pending_session(&name);
                    if let Some(mut session) = self.sessions.remove(&name) {
                        for pane in session.panes.values_mut() {
                            let _ = pane.kill();
                        }
                    }
                    self.remove_metadata(&name)?;
                    self.send_to(client_id, ServerMessage::Detached)?;
                    self.detach_client(client_id);
                    return Ok(());
                }
                if let Some(session) = self.sessions.get_mut(&name) {
                    session.close_focused()?;
                }
                self.persist_session(&name)?;
                self.send_snapshot(&name)
            }
            ClientMessage::Rename { name } => {
                validate_name(&name)?;
                let old_name = self
                    .attached_session_name(client_id)
                    .ok_or("no attached session")?;
                if old_name == name {
                    return self.send_attached_snapshot(client_id);
                }
                if self.sessions.contains_key(&name) {
                    return Err(format!("session already exists: {name}").into());
                }
                let mut session = self
                    .sessions
                    .remove(&old_name)
                    .ok_or("session does not exist")?;
                session.rename(name.clone());
                self.sessions.insert(name.clone(), session);
                if let Some(client) = self.clients.get_mut(&client_id) {
                    client.session = Some(name.clone());
                }
                self.rename_pending_session(&old_name, &name);
                self.remove_metadata(&old_name)?;
                self.persist_session(&name)?;
                self.send_attached_snapshot(client_id)
            }
            ClientMessage::AdjustRatio { delta } => {
                if let Some(session) = self.attached_session_mut(client_id) {
                    session.adjust_ratio(delta)?;
                }
                if let Some(name) = self.attached_session_name(client_id) {
                    self.persist_session(&name)?;
                }
                self.send_attached_snapshot(client_id)
            }
            ClientMessage::Focus { direction } => {
                let direction = match direction.as_str() {
                    "left" => FocusDirection::Left,
                    "right" => FocusDirection::Right,
                    "up" => FocusDirection::Up,
                    "down" => FocusDirection::Down,
                    _ => {
                        return self.send_to(
                            client_id,
                            ServerMessage::Error {
                                message: format!("unknown focus direction: {direction}"),
                            },
                        )
                    }
                };
                if let Some(session) = self.attached_session_mut(client_id) {
                    session.focus(direction);
                }
                self.send_attached_snapshot(client_id)
            }
            ClientMessage::Zoom => {
                if let Some(session) = self.attached_session_mut(client_id) {
                    session.toggle_zoom();
                }
                self.send_attached_snapshot(client_id)
            }
            ClientMessage::Detach => {
                self.send_to(client_id, ServerMessage::Detached)?;
                self.detach_client(client_id);
                self.cleanup_finished_temporary();
                Ok(())
            }
            ClientMessage::Ping => {
                self.send_to(client_id, ServerMessage::Pong)?;
                self.detach_client(client_id);
                Ok(())
            }
            ClientMessage::Heartbeat => self.send_to(client_id, ServerMessage::HeartbeatAck),
        }
    }

    fn attach_session(&mut self, client_id: u64, name: String, rows: u16, cols: u16) -> Result<()> {
        self.validate_attach_target(&name)?;
        let (pane_id, restarted) = {
            let session = self.sessions.get_mut(&name).expect("session exists");
            let restarted = if session.temporary {
                false
            } else {
                session.restart_focused_if_exited(&self.config, self.pane_events_tx.clone())?
            };
            session.resize(rows, cols)?;
            session.mark_attached();
            (session.focused_pane_id(), restarted)
        };
        let exited = self
            .sessions
            .get(&name)
            .and_then(|session| session.exited.get(&pane_id))
            .cloned();
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.session = Some(name.clone());
        }
        self.send_to(
            client_id,
            ServerMessage::Attached {
                name: name.clone(),
                pane_id,
            },
        )?;
        self.persist_session(&name)?;
        self.send_snapshot(&name)?;
        if !restarted {
            if let Some(status) = exited {
                self.send_to(
                    client_id,
                    ServerMessage::ProcessExited {
                        pane_id,
                        status,
                        session_finished: self
                            .sessions
                            .get(&name)
                            .is_some_and(Session::is_finished),
                    },
                )?;
            }
        }
        Ok(())
    }

    fn validate_attach_target(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        if !self.sessions.contains_key(name) {
            return Err(format!("session does not exist: {name}").into());
        }
        Ok(())
    }

    fn handle_pane_event(&mut self, event: PaneEvent) -> Result<()> {
        let pane_id = match &event {
            PaneEvent::Output { pane_id, .. }
            | PaneEvent::Exited { pane_id, .. }
            | PaneEvent::ReaderError { pane_id, .. } => *pane_id,
        };
        let Some(session_name) = self
            .sessions
            .iter()
            .find(|(_, session)| session.has_pane(pane_id))
            .map(|(name, _)| name.clone())
        else {
            return Ok(());
        };

        match event {
            PaneEvent::Output { bytes, .. } => {
                let attached = if let Some(session) = self.sessions.get_mut(&session_name) {
                    if let Some(pane) = session.pane_mut(pane_id) {
                        pane.process_output(&bytes);
                    }
                    session.attached
                } else {
                    false
                };
                if attached {
                    self.pending_snapshots.insert(session_name);
                }
            }
            PaneEvent::Exited { status, .. } => {
                let (attached, session_finished) =
                    if let Some(session) = self.sessions.get_mut(&session_name) {
                        session.record_exit(pane_id, status.clone());
                        (session.attached, session.is_finished())
                    } else {
                        (false, false)
                    };
                self.persist_session(&session_name)?;
                self.cleanup_finished_temporary();
                if attached {
                    self.pending_exits.push(PendingExit {
                        session_name,
                        pane_id,
                        status,
                        session_finished,
                    });
                }
            }
            PaneEvent::ReaderError { error, .. } => {
                let attached = self
                    .sessions
                    .get(&session_name)
                    .is_some_and(|session| session.attached);
                if attached {
                    if let Some(client_id) = self.client_id_for_session(&session_name) {
                        self.send_to(client_id, ServerMessage::Error { message: error })?;
                    }
                }
            }
        }
        Ok(())
    }

    fn create_session(
        &mut self,
        name: String,
        rows: u16,
        cols: u16,
        command: Option<Vec<String>>,
        temporary: bool,
    ) -> Result<()> {
        validate_name(&name)?;
        if self.sessions.contains_key(&name) {
            return Err(format!("session already exists: {name}").into());
        }
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let pane_events = self.pane_events_tx.clone();
        let session = Session::new_with_command(
            name.clone(),
            pane_id,
            &self.config,
            rows,
            cols,
            SessionOptions { command, temporary },
            pane_events,
        )?;
        self.sessions.insert(name.clone(), session);
        self.persist_session(&name)?;
        Ok(())
    }

    fn persist_session(&self, name: &str) -> Result<()> {
        let session = self.sessions.get(name).ok_or("session does not exist")?;
        let path = self.config.session_metadata_path(name)?;
        let contents = serde_json::to_vec_pretty(&session.metadata())?;
        fs::write(&path, contents)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    fn remove_metadata(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        match fs::remove_file(self.config.session_metadata_path(name)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn clear_pending_session(&mut self, name: &str) {
        self.pending_resizes.remove(name);
        self.pending_snapshots.remove(name);
        self.pending_exits.retain(|exit| exit.session_name != name);
    }

    fn rename_pending_session(&mut self, old_name: &str, new_name: &str) {
        if let Some(resize) = self.pending_resizes.remove(old_name) {
            self.pending_resizes.insert(new_name.to_string(), resize);
        }
        if self.pending_snapshots.remove(old_name) {
            self.pending_snapshots.insert(new_name.to_string());
        }
        for exit in &mut self.pending_exits {
            if exit.session_name == old_name {
                exit.session_name = new_name.to_string();
            }
        }
    }

    fn client_id_for_session(&self, name: &str) -> Option<u64> {
        self.clients
            .values()
            .find(|client| client.session.as_deref() == Some(name))
            .map(|client| client.id)
    }

    fn attached_session_name(&self, client_id: u64) -> Option<String> {
        self.clients.get(&client_id)?.session.clone()
    }

    fn attached_session_mut(&mut self, client_id: u64) -> Option<&mut Session> {
        let name = self.attached_session_name(client_id)?;
        self.sessions.get_mut(&name)
    }

    fn with_focused_pane<T>(
        &mut self,
        client_id: u64,
        operation: impl FnOnce(&mut crate::pane::Pane) -> T,
    ) -> Result<T> {
        self.attached_session_mut(client_id)
            .and_then(|session| session.focused_pane_mut())
            .map(operation)
            .ok_or_else(|| "no attached pane".into())
    }

    fn send_attached_snapshot(&mut self, client_id: u64) -> Result<()> {
        let Some(name) = self.attached_session_name(client_id) else {
            return Ok(());
        };
        if self
            .clients
            .get(&client_id)
            .is_some_and(|client| client.snapshot_in_flight)
        {
            self.pending_snapshots.insert(name);
            return Ok(());
        }
        self.send_snapshot(&name)
    }

    fn send_snapshot(&mut self, session_name: &str) -> Result<()> {
        let Some(client_id) = self.client_id_for_session(session_name) else {
            self.pending_snapshots.remove(session_name);
            return Ok(());
        };
        if self
            .clients
            .get(&client_id)
            .is_some_and(|client| client.snapshot_in_flight)
        {
            self.pending_snapshots.insert(session_name.to_string());
            return Ok(());
        }
        self.pending_snapshots.remove(session_name);
        let Some(session) = self.sessions.get_mut(session_name) else {
            return Ok(());
        };
        let (rows, cols) = session.size();
        let data = session.render()?;
        let mouse_enabled = session.focused_mouse_enabled();
        let alternate_screen = session.focused_alternate_screen();
        let scrollback_available = session.focused_scrollback_available();
        let result = self.send_to(
            client_id,
            ServerMessage::Snapshot {
                rows,
                cols,
                data,
                mouse_enabled,
                alternate_screen,
                scrollback_available,
            },
        );
        self.last_snapshot = Instant::now();
        result
    }

    fn send_to(&mut self, client_id: u64, message: ServerMessage) -> Result<()> {
        let is_snapshot = matches!(message, ServerMessage::Snapshot { .. });
        let Some((writer, snapshot_in_flight)) = self
            .clients
            .get(&client_id)
            .map(|client| (client.writer.clone(), client.snapshot_in_flight))
        else {
            return Ok(());
        };
        if is_snapshot && snapshot_in_flight {
            return Ok(());
        }
        if writer.enqueue(message, is_snapshot).is_err() {
            self.detach_client(client_id);
            return Ok(());
        }
        if is_snapshot {
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.snapshot_in_flight = true;
            }
        }
        Ok(())
    }

    fn client_lease_expired(&self, client_id: u64) -> bool {
        self.clients.get(&client_id).is_some_and(|client| {
            client.token.is_some()
                && client.last_seen.lock().map_or(true, |last_seen| {
                    last_seen.elapsed() >= CLIENT_LEASE_TIMEOUT
                })
        })
    }

    fn expire_stale_clients(&mut self) {
        let expired = self
            .clients
            .keys()
            .copied()
            .filter(|client_id| self.client_lease_expired(*client_id))
            .collect::<Vec<_>>();
        for client_id in expired {
            self.detach_client(client_id);
            self.cleanup_finished_temporary();
        }
    }

    fn detach_client(&mut self, client_id: u64) {
        let _ = self.cancel_search(client_id);
        let Some(client) = self.clients.remove(&client_id) else {
            return;
        };
        client.writer.close();
        if let Some(name) = client.session {
            self.clear_pending_session(&name);
            if let Some(session) = self.sessions.get_mut(&name) {
                session.attached = false;
            }
        }
    }

    fn wait_for_client_writers(&self) {
        for client in self.clients.values() {
            client.writer.wait_idle();
        }
    }

    fn cleanup_finished_temporary(&mut self) {
        let names = self
            .sessions
            .iter()
            .filter(|(_, session)| session.temporary && !session.attached && session.is_finished())
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in names {
            self.clear_pending_session(&name);
            if let Some(mut session) = self.sessions.remove(&name) {
                for pane in session.panes.values_mut() {
                    let _ = pane.kill();
                }
            }
            let _ = self.remove_metadata(&name);
        }
    }
}

fn client_writer(writer: Arc<ClientWriter>, events: mpsc::SyncSender<Event>, client_id: u64) {
    loop {
        let queued = {
            let Ok(mut state) = writer.state.lock() else {
                return;
            };
            while state.queue.is_empty() && !state.closed {
                state = writer
                    .wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.closed && state.queue.is_empty() {
                drop(state);
                if let Ok(stream) = writer.stream.lock() {
                    let _ = stream.shutdown(Shutdown::Both);
                }
                return;
            }
            state.writing = true;
            state.queue.pop_front()
        };
        let Some(QueuedMessage { message, snapshot }) = queued else {
            continue;
        };

        let write_result = writer
            .stream
            .lock()
            .map_err(|_| "client writer lock poisoned".to_string())
            .and_then(|mut stream| {
                write_message(&mut *stream, &message).map_err(|error| error.to_string())
            });
        if write_result.is_err() {
            if let Ok(mut state) = writer.state.lock() {
                state.closed = true;
                state.writing = false;
                state.queue.clear();
                writer.wake.notify_all();
            }
            let _ = events.send(Event::ClientWriteFailed(client_id));
            return;
        }

        if let Ok(mut state) = writer.state.lock() {
            state.writing = false;
            writer.wake.notify_all();
        }
        if snapshot {
            let _ = events.send(Event::SnapshotWritten(client_id));
        }
    }
}

fn client_reader(
    mut reader: UnixStream,
    events: mpsc::SyncSender<Event>,
    client_id: u64,
    last_seen: Arc<Mutex<Instant>>,
) {
    loop {
        match read_message::<_, ClientMessage>(&mut reader) {
            Ok(Some(message)) => {
                if let Ok(mut arrival) = last_seen.lock() {
                    *arrival = Instant::now();
                }
                if events
                    .send(Event::ClientMessage { client_id, message })
                    .is_err()
                {
                    return;
                }
            }
            Ok(None) | Err(_) => {
                let _ = events.send(Event::ClientDisconnected(client_id));
                return;
            }
        }
    }
}
