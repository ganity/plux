#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::{mpsc, Mutex, MutexGuard},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use plux::protocol::{read_message, write_message, ClientMessage, ServerMessage};
use vt100::Parser;

static DAEMON_TEST_LOCK: Mutex<()> = Mutex::new(());

struct TestDaemon {
    _serial: MutexGuard<'static, ()>,
    root: PathBuf,
    runtime: PathBuf,
    config: PathBuf,
    user: String,
    daemon: Child,
}

const FIRST_CLIENT_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const SECOND_CLIENT_TOKEN: &str = "fedcba9876543210fedcba9876543210";
const THIRD_CLIENT_TOKEN: &str = "11111111111111111111111111111111";

impl TestDaemon {
    fn start() -> Self {
        let serial = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/tmp").join(format!("px-{}-{suffix:x}", std::process::id()));
        let runtime = root.join("r");
        let config = root.join("c");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&config).unwrap();

        let user = format!("t{}", std::process::id());
        let daemon = Command::new(env!("CARGO_BIN_EXE_plux"))
            .arg("__daemon")
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", &config)
            .env("USER", &user)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let test = Self {
            _serial: serial,
            root,
            runtime,
            config,
            user,
            daemon,
        };
        test.wait_for_socket();
        test
    }

    fn socket_path(&self) -> PathBuf {
        self.runtime
            .join(format!("plux-{}", self.user))
            .join("plux.sock")
    }

    fn wait_for_socket(&self) {
        for _ in 0..100 {
            if UnixStream::connect(self.socket_path()).is_ok() {
                thread::sleep(Duration::from_millis(50));
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("daemon socket did not become ready");
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_plux"))
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("USER", &self.user)
            .output()
            .unwrap()
    }

    fn create_with_command(&self, name: &str, command: Vec<String>) {
        let mut stream = UnixStream::connect(self.socket_path()).unwrap();
        write_message(
            &mut stream,
            &ClientMessage::Create {
                name: name.to_string(),
                rows: 24,
                cols: 80,
                command: Some(command),
                temporary: false,
            },
        )
        .unwrap();
        assert!(matches!(
            next_server(&mut stream),
            ServerMessage::Created { .. }
        ));
    }

    fn attach(&self, name: &str) -> UnixStream {
        self.attach_with_token(name, FIRST_CLIENT_TOKEN).0
    }

    fn attach_with_snapshot(&self, name: &str) -> (UnixStream, String) {
        self.attach_with_token(name, FIRST_CLIENT_TOKEN)
    }

    fn attach_with_token(&self, name: &str, client_token: &str) -> (UnixStream, String) {
        let mut stream = UnixStream::connect(self.socket_path()).unwrap();
        write_message(
            &mut stream,
            &ClientMessage::Attach {
                name: name.to_string(),
                rows: 24,
                cols: 80,
                client_token: client_token.to_string(),
            },
        )
        .unwrap();
        assert!(matches!(
            next_server(&mut stream),
            ServerMessage::Attached { .. }
        ));
        let snapshot = match next_server(&mut stream) {
            ServerMessage::Snapshot { data, .. } => data,
            response => panic!("expected initial snapshot, got {response:?}"),
        };
        (stream, snapshot)
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn next_server<R: Read>(stream: &mut R) -> ServerMessage {
    read_message(stream).unwrap().unwrap()
}

fn wait_for_detached<R: Read>(stream: &mut R) {
    for _ in 0..20 {
        match next_server(stream) {
            ServerMessage::Detached => return,
            ServerMessage::Snapshot { .. } => {}
            response => panic!("unexpected detach response: {response:?}"),
        }
    }
    panic!("client did not receive Detached");
}

fn wait_for_heartbeat<R: Read>(stream: &mut R) {
    for _ in 0..20 {
        match next_server(stream) {
            ServerMessage::HeartbeatAck => return,
            ServerMessage::Snapshot { .. } => {}
            response => panic!("unexpected heartbeat response: {response:?}"),
        }
    }
    panic!("client did not receive HeartbeatAck");
}

#[test]
fn osc52_copy_from_pane_reaches_attached_client() {
    let test = TestDaemon::start();
    test.create_with_command(
        "osc52-copy",
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 1; printf '\\033]52;c;Q2xhdWRlIOWkjeWItuaWh+acrA==\\007'; sleep 30".to_string(),
        ],
    );

    let mut attached = test.attach("osc52-copy");
    attached
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    for _ in 0..20 {
        match read_message(&mut attached) {
            Ok(Some(ServerMessage::Copied { text })) => {
                assert_eq!(text, "Claude 复制文本");
                return;
            }
            Ok(Some(ServerMessage::Snapshot { .. })) => {}
            response => panic!("OSC 52 copy was not forwarded: {response:?}"),
        }
    }
    panic!("OSC 52 copy was not forwarded");
}

#[test]
fn resize_burst_is_coalesced_without_detaching_client() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "resize-burst"]).status.success());

    let mut attached = test.attach("resize-burst");
    attached
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut burst = Vec::new();
    for index in 0..256 {
        let (rows, cols) = if index % 2 == 0 { (24, 80) } else { (3, 10) };
        write_message(&mut burst, &ClientMessage::Resize { rows, cols }).unwrap();
    }
    write_message(&mut burst, &ClientMessage::Heartbeat).unwrap();
    attached.write_all(&burst).unwrap();

    let mut snapshots = 0;
    let mut saw_final_size = false;
    let mut saw_heartbeat = false;
    while !saw_final_size || !saw_heartbeat {
        match next_server(&mut attached) {
            ServerMessage::Snapshot { rows, cols, .. } => {
                snapshots += 1;
                saw_final_size |= (rows, cols) == (3, 10);
            }
            ServerMessage::HeartbeatAck => saw_heartbeat = true,
            response => panic!("unexpected resize response: {response:?}"),
        }
    }
    assert!(
        snapshots <= 4,
        "resize burst produced {snapshots} snapshots instead of coalescing"
    );

    write_message(&mut attached, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut attached);
}

#[test]
fn resize_burst_applies_only_the_final_pty_size() {
    let test = TestDaemon::start();
    test.create_with_command(
        "resize-winch",
        vec!["/bin/sh".to_string(), "-i".to_string()],
    );

    let (mut attached, initial) = test.attach_with_snapshot("resize-winch");
    attached
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut startup = initial;
    write_message(
        &mut attached,
        &ClientMessage::Input {
            bytes: b"printf READY\\n\r".to_vec(),
        },
    )
    .unwrap();
    while !startup.contains("READY") {
        match next_server(&mut attached) {
            ServerMessage::Snapshot { data, .. } => startup.push_str(&data),
            response => panic!("unexpected startup response: {response:?}"),
        }
    }
    for index in 0..100 {
        write_message(
            &mut attached,
            &ClientMessage::Resize {
                rows: if index % 2 == 0 { 8 } else { 48 },
                cols: if index % 2 == 0 { 20 } else { 160 },
            },
        )
        .unwrap();
        thread::sleep(Duration::from_millis(5));
    }
    write_message(&mut attached, &ClientMessage::Resize { rows: 31, cols: 97 }).unwrap();
    thread::sleep(Duration::from_secs(1));
    write_message(
        &mut attached,
        &ClientMessage::Input {
            bytes: b"stty size; printf FINAL\\n\r".to_vec(),
        },
    )
    .unwrap();
    write_message(&mut attached, &ClientMessage::Heartbeat).unwrap();

    let mut snapshot_count = 0;
    let mut rendered = String::new();
    let mut saw_final_size = false;
    let mut saw_final_output = false;
    let mut saw_heartbeat = false;
    while !saw_final_size || !saw_final_output || !saw_heartbeat {
        match next_server(&mut attached) {
            ServerMessage::Snapshot {
                rows, cols, data, ..
            } => {
                snapshot_count += 1;
                rendered.push_str(&data);
                saw_final_size |= (rows, cols) == (31, 97);
                saw_final_output |= rendered.contains("31 97") && rendered.contains("FINAL");
            }
            ServerMessage::HeartbeatAck => saw_heartbeat = true,
            response => panic!("unexpected resize response: {response:?}"),
        }
    }
    assert!(
        snapshot_count <= 4,
        "resize burst produced {snapshot_count} snapshots"
    );
}

#[test]
fn blocked_child_input_is_bounded_without_blocking_short_requests() {
    let test = TestDaemon::start();
    test.create_with_command(
        "blocked-input",
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30".to_string(),
        ],
    );
    let mut attached = test.attach("blocked-input");
    attached
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write_message(
        &mut attached,
        &ClientMessage::Input {
            bytes: vec![b'x'; 1024 * 1024 + 1],
        },
    )
    .unwrap();
    assert!(matches!(
        next_server(&mut attached),
        ServerMessage::Error { message } if message.contains("pane input backlog exceeded")
    ));
    let started = std::time::Instant::now();
    let listed = test.cli(&["list"]);
    assert!(listed.status.success(), "short request failed: {listed:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn large_resize_snapshot_survives_temporary_backpressure() {
    let test = TestDaemon::start();
    test.create_with_command(
        "large-resize",
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            concat!(
                "sleep 1; ",
                "awk 'BEGIN { ",
                "for (r = 0; r < 620; r++) { ",
                "for (c = 0; c < 1000; c++) printf \"\\033[3%dmX\", c % 8; ",
                "printf \"\\033[0m\\n\" } ",
                "print \"READY\" }'; ",
                "sleep 30"
            )
            .to_string(),
        ],
    );

    let mut attached = test.attach("large-resize");
    attached
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write_message(
        &mut attached,
        &ClientMessage::Resize {
            rows: 620,
            cols: 1000,
        },
    )
    .unwrap();
    assert!(matches!(
        next_server(&mut attached),
        ServerMessage::Snapshot {
            rows: 620,
            cols: 1000,
            ..
        }
    ));

    let mut saw_ready = false;
    for _ in 0..100 {
        if matches!(
            next_server(&mut attached),
            ServerMessage::Snapshot { ref data, .. } if data.contains("READY")
        ) {
            saw_ready = true;
            break;
        }
    }
    assert!(saw_ready, "large ANSI fixture did not finish rendering");

    for _ in 0..2 {
        write_message(&mut attached, &ClientMessage::Heartbeat).unwrap();
        wait_for_heartbeat(&mut attached);
    }

    let mut burst = Vec::new();
    write_message(
        &mut burst,
        &ClientMessage::Resize {
            rows: 619,
            cols: 1000,
        },
    )
    .unwrap();
    write_message(&mut burst, &ClientMessage::Heartbeat).unwrap();
    attached.write_all(&burst).unwrap();
    thread::sleep(Duration::from_secs(3));

    let mut saw_snapshot = false;
    let mut saw_heartbeat = false;
    while !saw_snapshot || !saw_heartbeat {
        match next_server(&mut attached) {
            ServerMessage::Snapshot {
                rows, cols, data, ..
            } => {
                assert!(
                    data.len() > 3_000_000,
                    "ANSI fixture snapshot was too small"
                );
                saw_snapshot |= (rows, cols) == (619, 1000);
            }
            ServerMessage::HeartbeatAck => saw_heartbeat = true,
            response => panic!("unexpected backpressure response: {response:?}"),
        }
    }
}

#[test]
fn disconnected_competing_client_does_not_kill_daemon() {
    let test = TestDaemon::start();
    let created = test.cli(&["new", "stable"]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let mut attached = test.attach("stable");
    let competing = UnixStream::connect(test.socket_path()).unwrap();
    drop(competing);
    thread::sleep(Duration::from_millis(250));

    let list_while_attached = test.cli(&["list"]);
    assert!(
        list_while_attached.status.success(),
        "short request failed after peer disconnect: stdout={}, stderr={}",
        String::from_utf8_lossy(&list_while_attached.stdout),
        String::from_utf8_lossy(&list_while_attached.stderr)
    );
    assert!(String::from_utf8_lossy(&list_while_attached.stdout)
        .lines()
        .any(|name| name == "stable"));

    let created_other = test.cli(&["new", "other"]);
    assert!(
        created_other.status.success(),
        "new while attached failed: {}",
        String::from_utf8_lossy(&created_other.stderr)
    );
    let killed_other = test.cli(&["kill", "other"]);
    assert!(
        killed_other.status.success(),
        "kill while attached failed: {}",
        String::from_utf8_lossy(&killed_other.stderr)
    );

    write_message(&mut attached, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut attached);
    drop(attached);

    let listed = test.cli(&["list"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(String::from_utf8_lossy(&listed.stdout)
        .lines()
        .any(|name| name == "stable"));
}

#[test]
fn takeover_closes_previous_client_connection() {
    let test = TestDaemon::start();
    let created = test.cli(&["new", "work"]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let mut first = test.attach("work");
    first
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();

    let mut second = UnixStream::connect(test.socket_path()).unwrap();
    write_message(
        &mut second,
        &ClientMessage::Takeover {
            name: "work".to_string(),
            rows: 24,
            cols: 80,
            client_token: SECOND_CLIENT_TOKEN.to_string(),
        },
    )
    .unwrap();
    assert!(matches!(
        next_server(&mut second),
        ServerMessage::Attached { .. }
    ));
    assert!(matches!(
        next_server(&mut second),
        ServerMessage::Snapshot { .. }
    ));

    let mut closed = [0_u8; 1];
    loop {
        match first.read(&mut closed) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                panic!("old client stayed attached")
            }
            Err(_) => break,
        }
    }

    write_message(&mut second, &ClientMessage::Detach).unwrap();
    for _ in 0..10 {
        if matches!(next_server(&mut second), ServerMessage::Detached) {
            return;
        }
    }
    panic!("takeover client did not receive Detached");
}

#[test]
fn takeover_of_other_session_keeps_existing_client_connection() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "work"]).status.success());
    assert!(test.cli(&["new", "web"]).status.success());

    let mut work = test.attach("work");
    work.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();

    let mut web = UnixStream::connect(test.socket_path()).unwrap();
    write_message(
        &mut web,
        &ClientMessage::Takeover {
            name: "web".to_string(),
            rows: 24,
            cols: 80,
            client_token: SECOND_CLIENT_TOKEN.to_string(),
        },
    )
    .unwrap();
    assert!(matches!(
        next_server(&mut web),
        ServerMessage::Attached { .. }
    ));
    assert!(matches!(
        next_server(&mut web),
        ServerMessage::Snapshot { .. }
    ));

    write_message(&mut work, &ClientMessage::Heartbeat).unwrap();
    wait_for_heartbeat(&mut work);
    write_message(&mut web, &ClientMessage::Heartbeat).unwrap();
    wait_for_heartbeat(&mut web);
}

#[test]
fn takeover_replaces_only_the_target_session_client() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "work"]).status.success());
    assert!(test.cli(&["new", "web"]).status.success());

    let (mut old_work, _) = test.attach_with_token("work", FIRST_CLIENT_TOKEN);
    let (mut web, _) = test.attach_with_token("web", SECOND_CLIENT_TOKEN);
    old_work
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();

    let mut new_work = UnixStream::connect(test.socket_path()).unwrap();
    write_message(
        &mut new_work,
        &ClientMessage::Takeover {
            name: "work".to_string(),
            rows: 24,
            cols: 80,
            client_token: THIRD_CLIENT_TOKEN.to_string(),
        },
    )
    .unwrap();
    assert!(matches!(
        next_server(&mut new_work),
        ServerMessage::Attached { .. }
    ));
    assert!(matches!(
        next_server(&mut new_work),
        ServerMessage::Snapshot { .. }
    ));

    let mut closed = [0_u8; 1];
    loop {
        match old_work.read(&mut closed) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                panic!("old target-session client stayed attached")
            }
            Err(_) => break,
        }
    }
    thread::sleep(Duration::from_millis(200));
    write_message(&mut web, &ClientMessage::Heartbeat).unwrap();
    wait_for_heartbeat(&mut web);
    write_message(&mut new_work, &ClientMessage::Heartbeat).unwrap();
    wait_for_heartbeat(&mut new_work);
}

#[test]
fn different_sessions_receive_their_own_output_and_survive_short_requests() {
    let test = TestDaemon::start();
    test.create_with_command(
        "work-output",
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 1; printf 'WORK_ONLY\\n'; sleep 30".to_string(),
        ],
    );
    test.create_with_command(
        "web-output",
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 1; printf 'WEB_ONLY\\n'; sleep 30".to_string(),
        ],
    );

    let (mut work, _) = test.attach_with_token("work-output", FIRST_CLIENT_TOKEN);
    let (mut web, _) = test.attach_with_token("web-output", SECOND_CLIENT_TOKEN);
    work.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    web.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    let mut work_output = String::new();
    while !work_output.contains("WORK_ONLY") {
        if let ServerMessage::Snapshot { data, .. } = next_server(&mut work) {
            work_output = data;
        }
    }
    let mut web_output = String::new();
    while !web_output.contains("WEB_ONLY") {
        if let ServerMessage::Snapshot { data, .. } = next_server(&mut web) {
            web_output = data;
        }
    }
    assert!(!work_output.contains("WEB_ONLY"));
    assert!(!web_output.contains("WORK_ONLY"));

    assert!(test.cli(&["list"]).status.success());
    assert!(test.cli(&["new", "short-request"]).status.success());
    assert!(test.cli(&["kill", "short-request"]).status.success());

    write_message(&mut work, &ClientMessage::Heartbeat).unwrap();
    wait_for_heartbeat(&mut work);
    write_message(&mut web, &ClientMessage::Heartbeat).unwrap();
    wait_for_heartbeat(&mut web);
}

#[test]
fn same_client_token_reconnects_and_heartbeats() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "work"]).status.success());

    let mut first = test.attach("work");
    first
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();

    let (mut resumed, _) = test.attach_with_token("work", FIRST_CLIENT_TOKEN);
    let mut closed = [0_u8; 1];
    loop {
        match first.read(&mut closed) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                panic!("replaced client stayed attached")
            }
            Err(_) => break,
        }
    }

    write_message(&mut resumed, &ClientMessage::Heartbeat).unwrap();
    wait_for_heartbeat(&mut resumed);

    let mut competing = UnixStream::connect(test.socket_path()).unwrap();
    write_message(
        &mut competing,
        &ClientMessage::Attach {
            name: "work".to_string(),
            rows: 24,
            cols: 80,
            client_token: SECOND_CLIENT_TOKEN.to_string(),
        },
    )
    .unwrap();
    assert!(matches!(
        next_server(&mut competing),
        ServerMessage::Error { message } if message.contains("another client is already attached")
    ));

    write_message(&mut resumed, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut resumed);
}

#[test]
fn repeated_same_token_reconnects_leave_one_client() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "reconnect-loop"]).status.success());

    let (mut current, _) = test.attach_with_token("reconnect-loop", FIRST_CLIENT_TOKEN);
    for _ in 0..10 {
        current
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let (mut next, _) = test.attach_with_token("reconnect-loop", FIRST_CLIENT_TOKEN);
        let mut closed = [0_u8; 1];
        loop {
            if current.read(&mut closed).unwrap() == 0 {
                break;
            }
        }
        write_message(&mut next, &ClientMessage::Heartbeat).unwrap();
        wait_for_heartbeat(&mut next);
        current = next;
    }

    write_message(&mut current, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut current);
}

#[test]
fn bridge_forwards_protocol_without_extra_output() {
    let test = TestDaemon::start();
    let mut bridge = Command::new(env!("CARGO_BIN_EXE_plux"))
        .arg("__bridge")
        .env("XDG_RUNTIME_DIR", &test.runtime)
        .env("XDG_CONFIG_HOME", &test.config)
        .env("USER", &test.user)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = bridge.stdin.take().unwrap();
    let mut stdout = bridge.stdout.take().unwrap();
    thread::sleep(Duration::from_millis(200));
    write_message(&mut stdin, &ClientMessage::List).unwrap();
    assert!(matches!(
        next_server(&mut stdout),
        ServerMessage::Sessions { names } if names.is_empty()
    ));
    drop(stdin);
    assert!(bridge.wait().unwrap().success());
}

#[test]
fn bridge_keeps_forwarding_after_interactive_attach() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "work"]).status.success());
    thread::sleep(Duration::from_millis(100));
    let mut bridge = Command::new(env!("CARGO_BIN_EXE_plux"))
        .arg("__bridge")
        .env("XDG_RUNTIME_DIR", &test.runtime)
        .env("XDG_CONFIG_HOME", &test.config)
        .env("USER", &test.user)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = bridge.stdin.take().unwrap();
    let mut stdout = bridge.stdout.take().unwrap();
    let (messages_tx, messages_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(Some(message)) = read_message(&mut stdout) {
            if messages_tx.send(message).is_err() {
                return;
            }
        }
    });

    write_message(
        &mut stdin,
        &ClientMessage::Takeover {
            name: "work".to_string(),
            rows: 24,
            cols: 80,
            client_token: FIRST_CLIENT_TOKEN.to_string(),
        },
    )
    .unwrap();
    let attached = messages_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        matches!(attached, Ok(ServerMessage::Attached { .. })),
        "bridge did not attach: {attached:?}"
    );
    let snapshot = messages_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        matches!(snapshot, Ok(ServerMessage::Snapshot { .. })),
        "bridge did not forward snapshot: {snapshot:?}"
    );

    write_message(&mut stdin, &ClientMessage::Heartbeat).unwrap();
    let heartbeat = loop {
        match messages_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(ServerMessage::Snapshot { .. }) => {}
            response => break response,
        }
    };
    if !matches!(heartbeat, Ok(ServerMessage::HeartbeatAck)) {
        let _ = bridge.kill();
        let _ = bridge.wait();
        panic!("bridge did not forward heartbeat: {heartbeat:?}");
    }

    write_message(&mut stdin, &ClientMessage::Detach).unwrap();
    let detached = loop {
        match messages_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(ServerMessage::Snapshot { .. }) => {}
            response => break response,
        }
    };
    assert!(matches!(detached, Ok(ServerMessage::Detached)));
    drop(stdin);
    assert!(bridge.wait().unwrap().success());
}

#[test]
fn bridges_can_attach_different_sessions_concurrently() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "work"]).status.success());
    assert!(test.cli(&["new", "web"]).status.success());

    let spawn_bridge = || {
        Command::new(env!("CARGO_BIN_EXE_plux"))
            .arg("__bridge")
            .env("XDG_RUNTIME_DIR", &test.runtime)
            .env("XDG_CONFIG_HOME", &test.config)
            .env("USER", &test.user)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut work_bridge = spawn_bridge();
    let mut work_in = work_bridge.stdin.take().unwrap();
    let mut work_out = work_bridge.stdout.take().unwrap();
    write_message(
        &mut work_in,
        &ClientMessage::Takeover {
            name: "work".to_string(),
            rows: 24,
            cols: 80,
            client_token: FIRST_CLIENT_TOKEN.to_string(),
        },
    )
    .unwrap();
    assert!(matches!(
        next_server(&mut work_out),
        ServerMessage::Attached { .. }
    ));
    assert!(matches!(
        next_server(&mut work_out),
        ServerMessage::Snapshot { .. }
    ));

    let mut web_bridge = spawn_bridge();
    let mut web_in = web_bridge.stdin.take().unwrap();
    let mut web_out = web_bridge.stdout.take().unwrap();
    write_message(
        &mut web_in,
        &ClientMessage::Takeover {
            name: "web".to_string(),
            rows: 24,
            cols: 80,
            client_token: SECOND_CLIENT_TOKEN.to_string(),
        },
    )
    .unwrap();
    assert!(matches!(
        next_server(&mut web_out),
        ServerMessage::Attached { .. }
    ));
    assert!(matches!(
        next_server(&mut web_out),
        ServerMessage::Snapshot { .. }
    ));

    thread::sleep(Duration::from_millis(200));
    write_message(&mut work_in, &ClientMessage::Heartbeat).unwrap();
    wait_for_heartbeat(&mut work_out);
    write_message(&mut web_in, &ClientMessage::Heartbeat).unwrap();
    wait_for_heartbeat(&mut web_out);

    write_message(&mut work_in, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut work_out);
    write_message(&mut web_in, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut web_out);
    drop(work_in);
    drop(web_in);
    assert!(work_bridge.wait().unwrap().success());
    assert!(web_bridge.wait().unwrap().success());
}

#[test]
fn unauthenticated_interactive_client_cannot_hold_attach_slot() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "work"]).status.success());

    let mut invalid = UnixStream::connect(test.socket_path()).unwrap();
    write_message(&mut invalid, &ClientMessage::Heartbeat).unwrap();
    assert!(matches!(
        next_server(&mut invalid),
        ServerMessage::Error { .. }
    ));
    drop(invalid);

    let _silent = UnixStream::connect(test.socket_path()).unwrap();
    thread::sleep(Duration::from_millis(150));
    let mut partial = UnixStream::connect(test.socket_path()).unwrap();
    partial.write_all(&[0, 3]).unwrap();
    thread::sleep(Duration::from_millis(150));

    let mut attached = test.attach("work");
    write_message(&mut attached, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut attached);
}

#[test]
fn invalid_takeover_keeps_current_client_and_daemon_alive() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "work"]).status.success());

    let mut attached = test.attach("work");
    let mut invalid = UnixStream::connect(test.socket_path()).unwrap();
    write_message(
        &mut invalid,
        &ClientMessage::Takeover {
            name: "bad/name".to_string(),
            rows: 24,
            cols: 80,
            client_token: SECOND_CLIENT_TOKEN.to_string(),
        },
    )
    .unwrap();
    assert!(matches!(
        next_server(&mut invalid),
        ServerMessage::Error { .. }
    ));

    write_message(&mut attached, &ClientMessage::Resize { rows: 24, cols: 80 }).unwrap();
    assert!(matches!(
        next_server(&mut attached),
        ServerMessage::Snapshot { .. }
    ));
    assert!(test.cli(&["list"]).status.success());

    write_message(&mut attached, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut attached);
}

#[test]
fn missing_takeover_keeps_current_client_attached() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "work"]).status.success());

    let mut attached = test.attach("work");
    let mut takeover = UnixStream::connect(test.socket_path()).unwrap();
    write_message(
        &mut takeover,
        &ClientMessage::Takeover {
            name: "missing".to_string(),
            rows: 24,
            cols: 80,
            client_token: SECOND_CLIENT_TOKEN.to_string(),
        },
    )
    .unwrap();
    assert!(matches!(
        next_server(&mut takeover),
        ServerMessage::Error { message } if message.contains("session does not exist: missing")
    ));

    write_message(&mut attached, &ClientMessage::Resize { rows: 24, cols: 80 }).unwrap();
    assert!(matches!(
        next_server(&mut attached),
        ServerMessage::Snapshot { .. }
    ));
    write_message(&mut attached, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut attached);
}

#[test]
fn final_snapshot_arrives_before_process_exit() {
    let test = TestDaemon::start();
    test.create_with_command(
        "final",
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "read line; printf 'final-marker\\n'".to_string(),
        ],
    );

    let mut attached = test.attach("final");
    write_message(
        &mut attached,
        &ClientMessage::Input {
            bytes: b"run\r".to_vec(),
        },
    )
    .unwrap();

    let mut saw_final_output = false;
    for _ in 0..10 {
        match next_server(&mut attached) {
            ServerMessage::Snapshot { data, .. } => {
                saw_final_output |= data.contains("final-marker");
            }
            ServerMessage::ProcessExited {
                session_finished, ..
            } => {
                assert!(saw_final_output, "process exit arrived before final output");
                assert!(session_finished);
                return;
            }
            _ => {}
        }
    }
    panic!("process exit was not reported");
}

#[test]
fn pane_exit_does_not_finish_a_split_session() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "split"]).status.success());

    let mut attached = test.attach("split");
    write_message(&mut attached, &ClientMessage::Split { vertical: true }).unwrap();
    assert!(matches!(
        next_server(&mut attached),
        ServerMessage::Snapshot { .. }
    ));
    write_message(
        &mut attached,
        &ClientMessage::Input {
            bytes: b"exit\r".to_vec(),
        },
    )
    .unwrap();

    for _ in 0..10 {
        if let ServerMessage::ProcessExited {
            session_finished, ..
        } = next_server(&mut attached)
        {
            assert!(!session_finished, "one pane must not end the whole session");
            write_message(&mut attached, &ClientMessage::Resize { rows: 24, cols: 80 }).unwrap();
            assert!(matches!(
                next_server(&mut attached),
                ServerMessage::Snapshot { .. }
            ));
            write_message(&mut attached, &ClientMessage::Detach).unwrap();
            wait_for_detached(&mut attached);
            return;
        }
    }
    panic!("pane exit was not reported");
}

#[test]
fn concurrent_cold_start_creates_leave_no_daemon_behind() {
    let mut test = TestDaemon::start();
    test.daemon.kill().unwrap();
    test.daemon.wait().unwrap();
    fs::remove_file(test.socket_path()).unwrap();

    let outputs = thread::scope(|scope| {
        let test = &test;
        (0..32)
            .map(|index| {
                let name = format!("cold-{index}");
                scope.spawn(move || test.cli(&["new", &name]))
            })
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    for output in outputs {
        assert!(
            output.status.success(),
            "concurrent cold-start create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let listed = test.cli(&["list"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(test.cli(&["stop"]).status.success());
    for _ in 0..100 {
        if !test.socket_path().exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("cold-start daemon did not stop");
}

#[test]
fn attach_to_missing_session_does_not_create_it() {
    let test = TestDaemon::start();
    let mut stream = UnixStream::connect(test.socket_path()).unwrap();
    write_message(
        &mut stream,
        &ClientMessage::Attach {
            name: "missing".to_string(),
            rows: 24,
            cols: 80,
            client_token: FIRST_CLIENT_TOKEN.to_string(),
        },
    )
    .unwrap();
    assert!(matches!(
        next_server(&mut stream),
        ServerMessage::Error { message } if message.contains("session does not exist: missing")
    ));
    write_message(&mut stream, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut stream);
    assert!(!String::from_utf8_lossy(&test.cli(&["list"]).stdout)
        .lines()
        .any(|name| name == "missing"));
}

#[test]
fn list_without_daemon_does_not_start_one() {
    let mut test = TestDaemon::start();
    test.daemon.kill().unwrap();
    test.daemon.wait().unwrap();
    fs::remove_file(test.socket_path()).unwrap();

    let listed = test.cli(&["list"]);
    assert!(!listed.status.success());
    assert!(!test.socket_path().exists());
}

#[test]
fn stop_terminates_the_daemon_and_removes_its_socket() {
    let mut test = TestDaemon::start();
    assert!(test.cli(&["new", "work"]).status.success());

    let stopped = test.cli(&["stop"]);
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(test.daemon.wait().unwrap().success());
    assert!(!test.socket_path().exists());
}

#[test]
fn search_completes_without_blocking_the_client_protocol() {
    let test = TestDaemon::start();
    let created = test.cli(&["new", "search"]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let mut attached = test.attach("search");
    write_message(
        &mut attached,
        &ClientMessage::Input {
            bytes: b"printf '\\nsearch-target\\n'\r".to_vec(),
        },
    )
    .unwrap();
    let mut saw_target = false;
    for _ in 0..20 {
        if let ServerMessage::Snapshot { data, .. } = next_server(&mut attached) {
            if data.contains("search-target") {
                saw_target = true;
                break;
            }
        }
    }
    assert!(saw_target, "shell output did not reach the attached client");

    write_message(
        &mut attached,
        &ClientMessage::Search {
            query: "search-target".to_string(),
            direction: 1,
        },
    )
    .unwrap();
    let mut found = false;
    for _ in 0..10 {
        if matches!(
            next_server(&mut attached),
            ServerMessage::SearchResult { found: true }
        ) {
            found = true;
            break;
        }
    }
    assert!(found, "search did not return a positive result");

    write_message(&mut attached, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut attached);
}

#[test]
fn searches_in_different_sessions_complete_independently() {
    let test = TestDaemon::start();
    test.create_with_command(
        "search-a",
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "for i in $(seq 1 400); do echo alpha-$i; done; sleep 30".to_string(),
        ],
    );
    test.create_with_command(
        "search-b",
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "for i in $(seq 1 400); do echo beta-$i; done; sleep 30".to_string(),
        ],
    );
    let (mut first, _) = test.attach_with_token("search-a", FIRST_CLIENT_TOKEN);
    let (mut second, _) = test.attach_with_token("search-b", SECOND_CLIENT_TOKEN);
    first
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    write_message(
        &mut first,
        &ClientMessage::Search {
            query: "missing-alpha".to_string(),
            direction: 1,
        },
    )
    .unwrap();
    write_message(
        &mut second,
        &ClientMessage::Search {
            query: "missing-beta".to_string(),
            direction: 1,
        },
    )
    .unwrap();

    for stream in [&mut first, &mut second] {
        loop {
            if matches!(
                next_server(stream),
                ServerMessage::SearchResult { found: false }
            ) {
                break;
            }
        }
    }
}

#[test]
fn heartbeat_does_not_cancel_search() {
    let test = TestDaemon::start();
    assert!(test.cli(&["new", "search-heartbeat"]).status.success());
    let mut attached = test.attach("search-heartbeat");

    write_message(
        &mut attached,
        &ClientMessage::Search {
            query: "not-present".to_string(),
            direction: 1,
        },
    )
    .unwrap();
    write_message(&mut attached, &ClientMessage::Heartbeat).unwrap();

    let mut found_result = false;
    let mut heartbeat = false;
    for _ in 0..10 {
        match next_server(&mut attached) {
            ServerMessage::SearchResult { found: false } => found_result = true,
            ServerMessage::HeartbeatAck => heartbeat = true,
            _ => {}
        }
        if found_result && heartbeat {
            break;
        }
    }
    assert!(found_result, "search result was cancelled by heartbeat");
    assert!(heartbeat, "heartbeat was not serviced during search");

    write_message(&mut attached, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut attached);
}

#[test]
fn terminal_query_reaches_child_through_daemon() {
    let test = TestDaemon::start();
    test.create_with_command(
        "terminal-query",
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            r#"stty raw -echo min 1 time 0; printf '\033[5n'; expected=$(printf '\033[0n'); response=$(for i in 1 2 3 4; do dd bs=1 count=1 2>/dev/null; done); stty sane; if [ "$response" = "$expected" ]; then printf 'DSR_DAEMON_OK\n'; fi; sleep 1"#.to_string(),
        ],
    );
    let (mut attached, initial) = test.attach_with_snapshot("terminal-query");
    let mut saw_marker = initial.contains("DSR_DAEMON_OK");
    for _ in 0..20 {
        if saw_marker {
            break;
        }
        if let ServerMessage::Snapshot { data, .. } = next_server(&mut attached) {
            saw_marker |= data.contains("DSR_DAEMON_OK");
        }
    }
    assert!(
        saw_marker,
        "terminal query reply did not reach child through daemon"
    );

    write_message(&mut attached, &ClientMessage::Detach).unwrap();
    wait_for_detached(&mut attached);
}

#[test]
fn scroll_snapshot_replays_full_pty_history() {
    let test = TestDaemon::start();
    test.create_with_command(
        "history",
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "for i in $(seq 0 119); do printf 'history-line-%03d\\n' \"$i\"; done; sleep 1"
                .to_string(),
        ],
    );

    let (mut attached, initial_snapshot) = test.attach_with_snapshot("history");
    attached
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();

    let mut saw_last = initial_snapshot.contains("history-line-119");
    for _ in 0..40 {
        match read_message(&mut attached) {
            Ok(Some(ServerMessage::Snapshot { data, .. })) => {
                if data.contains("history-line-119") {
                    saw_last = true;
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
            {
                break;
            }
            Err(error) => panic!("reading history snapshot failed: {error}"),
        }
    }
    assert!(saw_last, "latest PTY output did not reach the client");

    write_message(&mut attached, &ClientMessage::ScrollToTop).unwrap();
    let mut saw_first = false;
    for _ in 0..5 {
        match read_message(&mut attached) {
            Ok(Some(ServerMessage::Snapshot { data, .. })) => {
                if data.contains("history-line-000") {
                    saw_first = true;
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
            {
                break;
            }
            Err(error) => panic!("reading history snapshot failed: {error}"),
        }
    }
    assert!(
        saw_first,
        "scroll-to-top snapshot did not contain old history"
    );

    write_message(&mut attached, &ClientMessage::ScrollToBottom).unwrap();
    let mut returned_to_bottom = false;
    for _ in 0..5 {
        match read_message(&mut attached) {
            Ok(Some(ServerMessage::Snapshot { data, .. })) => {
                if data.contains("history-line-119") {
                    returned_to_bottom = true;
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
            {
                break;
            }
            Err(error) => panic!("reading history snapshot failed: {error}"),
        }
    }
    assert!(
        returned_to_bottom,
        "scroll-to-bottom snapshot did not restore latest output"
    );
}

#[test]
fn scrollback_preserves_top_anchored_partial_region_output() {
    let test = TestDaemon::start();
    test.create_with_command(
        "partial-region-history",
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "for i in $(seq 0 29); do printf 'shell-line-%02d\\n' \"$i\"; done; \
             printf '\\033[1;20r'; \
             for i in $(seq 0 59); do printf 'codex-line-%02d\\r\\n' \"$i\"; done; \
             printf '\\033[r'; \
             sleep 1"
                .to_string(),
        ],
    );

    let (mut attached, initial_snapshot) = test.attach_with_snapshot("partial-region-history");
    attached
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut screen = Parser::new(24, 80, 0);
    screen.process(initial_snapshot.as_bytes());

    let mut saw_latest = screen.screen().contents().contains("codex-line-59");
    for _ in 0..20 {
        if saw_latest {
            break;
        }
        match read_message(&mut attached) {
            Ok(Some(ServerMessage::Snapshot { data, .. })) => {
                screen.process(data.as_bytes());
                saw_latest = screen.screen().contents().contains("codex-line-59");
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) => {}
            Err(error) => panic!("reading partial-region snapshot failed: {error}"),
        }
    }
    assert!(
        saw_latest,
        "latest partial-region output did not reach the client"
    );

    write_message(&mut attached, &ClientMessage::ScrollToTop).unwrap();
    let mut saw_early_codex_output = false;
    for _ in 0..10 {
        match read_message(&mut attached) {
            Ok(Some(ServerMessage::Snapshot { data, .. })) => {
                screen.process(data.as_bytes());
                if screen.screen().contents().contains("codex-line-00") {
                    saw_early_codex_output = true;
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) => {}
            Err(error) => panic!("reading partial-region history failed: {error}"),
        }
    }
    assert!(
        saw_early_codex_output,
        "scrollback lost output scrolled from a top-anchored partial region"
    );
}

#[test]
fn scrolling_stays_at_history_while_pty_continues_output() {
    let test = TestDaemon::start();
    test.create_with_command(
        "live-history",
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "for i in $(seq 0 119); do printf 'live-line-%03d\\n' \"$i\"; done; sleep 0.2; for i in $(seq 120 400); do printf 'live-line-%03d\\n' \"$i\"; sleep 0.01; done; sleep 1".to_string(),
        ],
    );

    let (mut attached, initial_snapshot) = test.attach_with_snapshot("live-history");
    attached
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut screen = Parser::new(24, 80, 0);
    screen.process(initial_snapshot.as_bytes());

    let mut saw_initial_end = screen.screen().contents().contains("live-line-119");
    for _ in 0..30 {
        if saw_initial_end {
            break;
        }
        match read_message(&mut attached).unwrap() {
            Some(ServerMessage::Snapshot { data, .. }) => {
                screen.process(data.as_bytes());
                saw_initial_end = screen.screen().contents().contains("live-line-119");
            }
            Some(_) => {}
            None => break,
        }
    }
    assert!(saw_initial_end, "initial history did not reach the client");

    write_message(&mut attached, &ClientMessage::ScrollToTop).unwrap();
    let mut saw_top = false;
    for _ in 0..10 {
        match read_message(&mut attached).unwrap() {
            Some(ServerMessage::Snapshot { data, .. }) => {
                screen.process(data.as_bytes());
                if screen.screen().contents().contains("live-line-000") {
                    saw_top = true;
                    break;
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    assert!(saw_top, "scroll-to-top did not show the first history line");

    let mut saw_later_output = false;
    for _ in 0..20 {
        match read_message(&mut attached) {
            Ok(Some(ServerMessage::Snapshot { data, .. })) => {
                let has_unread_label = data.contains("new lines");
                screen.process(data.as_bytes());
                if has_unread_label {
                    saw_later_output = true;
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) => {}
            Err(error) => panic!("reading live history snapshot failed: {error}"),
        }
    }
    assert!(
        saw_later_output,
        "continued PTY output did not reach the client"
    );
    assert!(
        screen.screen().contents().contains("live-line-000"),
        "continued PTY output moved the client back to the live screen"
    );
}
