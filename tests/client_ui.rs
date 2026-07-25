#![cfg(unix)]
//! Real client tests must exercise the terminal loop here instead of replacing
//! it with protocol-only daemon tests.

use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Output},
    sync::{
        mpsc::{self, Receiver},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

struct ClientHarness {
    root: PathBuf,
    runtime: PathBuf,
    config: PathBuf,
    user: String,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Receiver<Vec<u8>>,
    output_flow: Arc<(Mutex<bool>, Condvar)>,
    child: Box<dyn Child + Send>,
}

impl ClientHarness {
    fn new(rows: u16, cols: u16) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            PathBuf::from("/tmp").join(format!("plux-client-{}-{suffix:x}", std::process::id()));
        let runtime = root.join("r");
        let config = root.join("c");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&config).unwrap();
        let user = format!("t{}", std::process::id());

        let pty = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_plux"));
        command.arg("work");
        command.env("XDG_RUNTIME_DIR", &runtime);
        command.env("XDG_CONFIG_HOME", &config);
        command.env("USER", &user);
        command.env("SHELL", "/bin/sh");
        let child = pty.slave.spawn_command(command).unwrap();
        let reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let output_flow = Arc::new((Mutex::new(false), Condvar::new()));
        let reader_flow = output_flow.clone();
        let (tx, output) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = reader;
            let mut buffer = [0_u8; 8192];
            loop {
                let (paused, wake) = &*reader_flow;
                let mut paused = paused.lock().unwrap();
                while *paused {
                    paused = wake.wait(paused).unwrap();
                }
                drop(paused);
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(size) => {
                        if tx.send(buffer[..size].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        Self {
            root,
            runtime,
            config,
            user,
            master: pty.master,
            writer,
            output,
            output_flow,
            child,
        }
    }

    fn run_cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_plux"))
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("USER", &self.user)
            .output()
            .unwrap()
    }

    fn wait_for_output(&self, marker: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        while Instant::now() < deadline {
            if let Ok(chunk) = self.output.recv_timeout(Duration::from_millis(100)) {
                output.extend_from_slice(&chunk);
                assert!(
                    output.len() <= 2 * 1024 * 1024,
                    "client output exceeded test limit"
                );
                let text = String::from_utf8_lossy(&output);
                if text.contains(marker) {
                    return text.into_owned();
                }
            }
        }
        panic!(
            "did not receive {marker:?}; output was:\n{}",
            String::from_utf8_lossy(&output)
        );
    }

    fn resize(&self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let pid = self.child.process_id().unwrap().to_string();
        assert!(
            Command::new("kill")
                .args(["-WINCH", &pid])
                .status()
                .unwrap()
                .success(),
            "failed to signal client {pid}"
        );
    }

    fn pause_output(&self) {
        let (paused, _) = &*self.output_flow;
        *paused.lock().unwrap() = true;
    }

    fn resume_output(&self) {
        let (paused, wake) = &*self.output_flow;
        *paused.lock().unwrap() = false;
        wake.notify_one();
    }

    fn client_is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    fn detach(&mut self) {
        self.writer.write_all(b"\x01d").unwrap();
        self.writer.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "client exited unsuccessfully: {status:?}");
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("client did not exit after detach");
    }
}

impl Drop for ClientHarness {
    fn drop(&mut self) {
        self.resume_output();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.run_cli(&["stop"]);
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn real_client_attaches_renders_input_and_detaches() {
    let mut test = ClientHarness::new(24, 80);
    test.writer
        .write_all(b"printf 'PLUX_CLIENT_READY\\n'\r")
        .unwrap();
    test.writer.flush().unwrap();
    test.wait_for_output("PLUX_CLIENT_READY", Duration::from_secs(10));

    test.detach();
    assert!(!test.client_is_running());
    let list = test.run_cli(&["list"]);
    assert!(list.status.success(), "list failed: {list:?}");
    assert!(
        String::from_utf8_lossy(&list.stdout)
            .lines()
            .any(|line| line.trim() == "work"),
        "session was not preserved: {}",
        String::from_utf8_lossy(&list.stdout)
    );
}

#[test]
fn real_client_survives_one_resize() {
    let mut test = ClientHarness::new(24, 80);
    test.writer
        .write_all(b"printf 'PLUX_RESIZE_BEFORE\\n'\r")
        .unwrap();
    test.writer.flush().unwrap();
    test.wait_for_output("PLUX_RESIZE_BEFORE", Duration::from_secs(10));

    test.writer.write_all(b"stty -echo\r").unwrap();
    test.writer.flush().unwrap();
    thread::sleep(Duration::from_millis(100));

    test.pause_output();
    test.resize(48, 160);
    thread::sleep(Duration::from_millis(200));
    test.resume_output();
    thread::sleep(Duration::from_millis(500));
    test.writer
        .write_all(b"stty size; printf 'PLUX_RESIZE_AFTER\\n'\r")
        .unwrap();
    test.writer.flush().unwrap();
    let output = test.wait_for_output("PLUX_RESIZE_AFTER", Duration::from_secs(10));
    assert!(test.client_is_running());
    assert!(
        output.contains("48 160"),
        "shell did not observe the resized PTY: {output}"
    );
}

#[test]
fn real_client_coalesces_large_resize_drag() {
    let mut test = ClientHarness::new(24, 80);
    test.writer.write_all(b"stty -echo\r").unwrap();
    test.writer.flush().unwrap();
    thread::sleep(Duration::from_millis(100));
    test.writer
        .write_all(
            b"for i in $(seq 1 240); do printf '\\033[31mROW%03d\\033[0m\\n' \"$i\"; done; printf 'PLUX_LARGE_READY\\n'\r",
        )
        .unwrap();
    test.writer.flush().unwrap();
    test.wait_for_output("PLUX_LARGE_READY", Duration::from_secs(10));

    for index in 0..20 {
        let offset = index as u16;
        let (rows, cols) = if index % 2 == 0 {
            (6 + offset % 3, 24 + offset)
        } else {
            (45 + offset % 4, 160 + offset)
        };
        test.resize(rows, cols);
    }
    test.resize(36, 140);
    thread::sleep(Duration::from_millis(700));
    test.writer
        .write_all(b"stty size; printf 'PLUX_LARGE_RESIZE_AFTER\\n'\r")
        .unwrap();
    test.writer.flush().unwrap();
    let output = test.wait_for_output("PLUX_LARGE_RESIZE_AFTER", Duration::from_secs(10));
    assert!(
        output.contains("36 140"),
        "final PTY size was not rendered: {output}"
    );
    assert!(test.client_is_running());
    test.detach();
}

#[test]
fn real_client_survives_slow_outer_reader() {
    let mut test = ClientHarness::new(24, 80);
    test.writer.write_all(b"stty -echo\r").unwrap();
    test.writer.flush().unwrap();
    thread::sleep(Duration::from_millis(100));
    test.writer
        .write_all(
            b"for i in $(seq 1 5000); do printf '\\033[31mSLOW%04d\\033[0m\\n' \"$i\"; done; printf 'PLUX_SLOW_DONE\\n'\r",
        )
        .unwrap();
    test.writer.flush().unwrap();

    test.pause_output();
    thread::sleep(Duration::from_secs(6));
    let listed = test.run_cli(&["list"]);
    assert!(
        listed.status.success(),
        "list blocked during slow output: {listed:?}"
    );
    test.resume_output();
    test.wait_for_output("PLUX_SLOW_DONE", Duration::from_secs(10));

    test.writer
        .write_all(b"printf 'PLUX_AFTER_SLOW_READER\\n'\r")
        .unwrap();
    test.writer.flush().unwrap();
    test.wait_for_output("PLUX_AFTER_SLOW_READER", Duration::from_secs(10));
    assert!(test.client_is_running());
}

#[test]
fn real_client_split_survives_tiny_resize_and_restores() {
    let mut test = ClientHarness::new(24, 80);
    test.writer
        .write_all(b"stty -echo; printf 'PLUX_LEFT_SPLIT\\n'\r")
        .unwrap();
    test.writer.flush().unwrap();
    test.wait_for_output("PLUX_LEFT_SPLIT", Duration::from_secs(10));

    test.writer.write_all(b"\x01v").unwrap();
    test.writer.flush().unwrap();
    thread::sleep(Duration::from_millis(200));
    test.writer
        .write_all(b"printf 'PLUX_RIGHT_SPLIT\\n'\r")
        .unwrap();
    test.writer.flush().unwrap();
    test.wait_for_output("PLUX_RIGHT_SPLIT", Duration::from_secs(10));

    test.resize(2, 2);
    thread::sleep(Duration::from_millis(300));
    test.resize(24, 80);
    thread::sleep(Duration::from_millis(500));
    assert!(test.client_is_running());
}
