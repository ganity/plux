use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, SystemTime, UNIX_EPOCH},
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
const REMOTE_UPLOAD_COMMAND: &str = r#"PATH="$HOME/.cargo/bin:$PATH" exec plux __upload"#;
const MAX_REMOTE_UPLOAD_SIZE: u64 = 1024 * 1024 * 1024;
const MAX_REMOTE_UPLOAD_ITEMS: u32 = 1024;
const UPLOAD_BUFFER_SIZE: usize = 64 * 1024;
const UPLOAD_REQUEST_MAGIC: &[u8; 8] = b"PLUXUP1\0";
const UPLOAD_RESPONSE_MAGIC: &[u8; 8] = b"PLUXOK1\0";
const UPLOAD_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
static NEXT_CONTROL_ID: AtomicU64 = AtomicU64::new(1);

pub struct Connection {
    pub reader: Box<dyn Read + Send>,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Option<Child>,
    stderr: Option<Arc<Mutex<String>>>,
    stderr_thread: Option<JoinHandle<()>>,
    uploader: Option<SshUploader>,
}

#[derive(Clone)]
pub struct SshUploader {
    target: String,
    control_path: PathBuf,
}

pub struct UploadItem {
    pub name: String,
    pub size: u64,
    pub reader: Box<dyn Read + Send>,
}

#[derive(Clone, Default)]
pub struct UploadControl {
    state: Arc<UploadControlState>,
}

#[derive(Default)]
struct UploadControlState {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl Connection {
    pub fn local(config: &Config) -> Result<Self> {
        let stream = connect(config)?;
        stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
        Self::from_unix_stream(stream)
    }

    pub fn ssh(config: &Config, target: &str, start: bool) -> Result<Self> {
        let control_path = config.runtime_dir()?.join(format!(
            "ssh-{}-{}",
            std::process::id(),
            NEXT_CONTROL_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&control_path);
        let control_option = format!("ControlPath={}", control_path.display());
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
            ])
            .args(["-o", "ControlMaster=yes", "-o"])
            .arg(&control_option)
            .arg(target)
            .arg(REMOTE_BRIDGE_COMMAND)
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
            uploader: Some(SshUploader {
                target: target.to_string(),
                control_path,
            }),
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
        if let Some(uploader) = self.uploader.as_ref() {
            let _ = fs::remove_file(&uploader.control_path);
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

    pub fn uploader(&self) -> Option<SshUploader> {
        self.uploader.clone()
    }

    fn from_unix_stream(stream: UnixStream) -> Result<Self> {
        Ok(Self {
            reader: Box::new(stream.try_clone()?),
            writer: Arc::new(Mutex::new(Box::new(stream))),
            child: None,
            stderr: None,
            stderr_thread: None,
            uploader: None,
        })
    }
}

impl SshUploader {
    pub fn upload<F>(
        &self,
        mut items: Vec<UploadItem>,
        control: &UploadControl,
        mut progress: F,
    ) -> Result<Vec<String>>
    where
        F: FnMut(&str, u64, u64),
    {
        if control.is_cancelled() {
            return Err("clipboard upload cancelled".into());
        }
        if items.is_empty() || items.len() > MAX_REMOTE_UPLOAD_ITEMS as usize {
            return Err("invalid clipboard upload item count".into());
        }
        let total = items.iter().try_fold(0_u64, |total, item| {
            total
                .checked_add(item.size)
                .ok_or("clipboard upload size overflow")
        })?;
        if total > MAX_REMOTE_UPLOAD_SIZE {
            return Err("clipboard upload is too large".into());
        }
        for item in &mut items {
            item.name = sanitize_upload_name(&item.name);
        }
        let mut child = Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ConnectTimeout=10",
                "-S",
            ])
            .arg(&self.control_path)
            .arg(&self.target)
            .arg(REMOTE_UPLOAD_COMMAND)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("ssh upload stdin pipe was not created".into());
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("ssh upload stdout pipe was not created".into());
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("ssh upload stderr pipe was not created".into());
            }
        };
        let stdout_thread = std::thread::spawn(move || read_bounded(stdout, 1024 * 1024));
        let stderr_thread =
            std::thread::spawn(move || read_bounded(stderr, SSH_STDERR_LIMIT as usize));
        control.install(child)?;
        let transfer = (|| -> Result<()> {
            stdin.write_all(UPLOAD_REQUEST_MAGIC)?;
            stdin.write_all(&(items.len() as u32).to_be_bytes())?;
            let mut buffer = [0_u8; UPLOAD_BUFFER_SIZE];
            let mut sent_total = 0_u64;
            for item in &mut items {
                if control.is_cancelled() {
                    return Err("clipboard upload cancelled".into());
                }
                write_upload_item_header(&mut stdin, &item.name, item.size)?;
                progress(&item.name, sent_total, total);
                let mut sent = 0_u64;
                while sent < item.size {
                    if control.is_cancelled() {
                        return Err("clipboard upload cancelled".into());
                    }
                    let remaining = usize::try_from((item.size - sent).min(buffer.len() as u64))?;
                    let read = item.reader.read(&mut buffer[..remaining])?;
                    if read == 0 {
                        return Err("clipboard file changed or ended during upload".into());
                    }
                    stdin.write_all(&buffer[..read])?;
                    sent += read as u64;
                    sent_total += read as u64;
                    progress(&item.name, sent_total, total);
                }
            }
            stdin.flush()?;
            Ok(())
        })();
        drop(stdin);
        let transfer_error = transfer.err();
        if transfer_error.is_some() {
            control.cancel();
        }
        let status = control.wait()?;
        let stdout = stdout_thread
            .join()
            .map_err(|_| "ssh upload stdout reader panicked")?;
        let stderr = stderr_thread
            .join()
            .map_err(|_| "ssh upload stderr reader panicked")?;
        if let Some(error) = transfer_error {
            return Err(error);
        }
        if !status.success() {
            let message = String::from_utf8_lossy(&stderr).trim().to_string();
            return Err(if message.is_empty() {
                "remote clipboard upload failed".into()
            } else {
                message.into()
            });
        }
        if control.is_cancelled() {
            return Err("clipboard upload cancelled".into());
        }
        read_upload_response(&stdout, items.len())
    }
}

impl UploadControl {
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
        if let Ok(mut child) = self.state.child.lock() {
            if let Some(child) = child.as_mut() {
                let _ = child.kill();
            }
        }
    }

    fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    fn install(&self, mut child: Child) -> Result<()> {
        let mut slot = match self.state.child.lock() {
            Ok(slot) => slot,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("upload child lock poisoned".into());
            }
        };
        if slot.is_some() {
            let _ = child.kill();
            let _ = child.wait();
            return Err("clipboard upload already has an ssh child".into());
        }
        *slot = Some(child);
        drop(slot);
        if self.is_cancelled() {
            self.cancel();
        }
        Ok(())
    }

    fn wait(&self) -> Result<ExitStatus> {
        loop {
            let status = {
                let mut slot = self
                    .state
                    .child
                    .lock()
                    .map_err(|_| "upload child lock poisoned")?;
                let child = slot.as_mut().ok_or("ssh upload child was not registered")?;
                match child.try_wait() {
                    Ok(Some(status)) => {
                        slot.take();
                        Ok(Some(status))
                    }
                    Ok(None) => Ok(None),
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        slot.take();
                        Err(error)
                    }
                }
            }?;
            if let Some(status) = status {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn read_bounded<R: Read>(mut reader: R, limit: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    while let Ok(size) = reader.read(&mut buffer) {
        if size == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..size.min(remaining)]);
    }
    kept
}

pub fn receive_upload(config: &Config) -> Result<()> {
    let directory = config.runtime_dir()?.join("uploads");
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    cleanup_expired_uploads(&directory);

    let paths = receive_upload_batch(&directory, &mut io::stdin().lock())?;
    let result = (|| -> Result<()> {
        let mut stdout = io::stdout().lock();
        write_upload_response(&mut stdout, &paths)?;
        stdout.flush()?;
        Ok(())
    })();
    if let Err(error) = result {
        for path in paths {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    Ok(())
}

fn write_upload_item_header<W: Write>(writer: &mut W, name: &str, size: u64) -> Result<()> {
    validate_upload_name(name)?;
    let name = name.as_bytes();
    let name_len = u16::try_from(name.len()).map_err(|_| "clipboard file name is too long")?;
    writer.write_all(&name_len.to_be_bytes())?;
    writer.write_all(&size.to_be_bytes())?;
    writer.write_all(name)?;
    Ok(())
}

fn read_upload_item_header<R: Read>(reader: &mut R) -> Result<(String, u64)> {
    let mut name_len = [0_u8; 2];
    reader.read_exact(&mut name_len)?;
    let name_len = usize::from(u16::from_be_bytes(name_len));
    if name_len == 0 || name_len > 96 {
        return Err("invalid clipboard upload name length".into());
    }
    let mut size = [0_u8; 8];
    reader.read_exact(&mut size)?;
    let mut name = vec![0_u8; name_len];
    reader.read_exact(&mut name)?;
    let name = String::from_utf8(name).map_err(|_| "clipboard upload name is not UTF-8")?;
    validate_upload_name(&name)?;
    Ok((name, u64::from_be_bytes(size)))
}

fn receive_upload_batch<R: Read>(directory: &Path, reader: &mut R) -> Result<Vec<String>> {
    let mut magic = [0_u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != UPLOAD_REQUEST_MAGIC {
        return Err("invalid clipboard upload header".into());
    }
    let mut count = [0_u8; 4];
    reader.read_exact(&mut count)?;
    let count = u32::from_be_bytes(count);
    if count == 0 || count > MAX_REMOTE_UPLOAD_ITEMS {
        return Err("invalid clipboard upload item count".into());
    }

    let mut created = Vec::with_capacity(count as usize);
    let result = (|| -> Result<Vec<String>> {
        let mut total = 0_u64;
        for _ in 0..count {
            let (name, size) = read_upload_item_header(reader)?;
            total = total
                .checked_add(size)
                .ok_or("clipboard upload size overflow")?;
            if total > MAX_REMOTE_UPLOAD_SIZE {
                return Err("clipboard upload is too large".into());
            }
            let (path, mut file) = create_upload_file(directory, &name)?;
            created.push(path.clone());
            let written = io::copy(&mut reader.take(size), &mut file)?;
            if written != size {
                return Err("clipboard upload ended before the declared size".into());
            }
            file.flush()?;
        }

        created
            .iter()
            .map(|path| Ok(fs::canonicalize(path)?.to_string_lossy().into_owned()))
            .collect()
    })();
    if result.is_err() {
        for path in created {
            let _ = fs::remove_file(path);
        }
    }
    result
}

fn create_upload_file(directory: &Path, name: &str) -> Result<(PathBuf, File)> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for attempt in 0..100_u8 {
        let path = directory.join(format!("{stamp}-{}-{attempt}-{name}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err("failed to allocate a remote clipboard path".into())
}

fn cleanup_expired_uploads(directory: &Path) {
    cleanup_expired_uploads_at(directory, SystemTime::now());
}

fn cleanup_expired_uploads_at(directory: &Path, now: SystemTime) {
    let cutoff = now.checked_sub(UPLOAD_RETENTION).unwrap_or(UNIX_EPOCH);
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_file() && metadata.modified().is_ok_and(|modified| modified < cutoff) {
            let _ = fs::remove_file(path);
        }
    }
}

fn write_upload_response<W: Write>(writer: &mut W, paths: &[String]) -> Result<()> {
    writer.write_all(UPLOAD_RESPONSE_MAGIC)?;
    writer.write_all(&(paths.len() as u32).to_be_bytes())?;
    for path in paths {
        let bytes = path.as_bytes();
        let length = u32::try_from(bytes.len()).map_err(|_| "remote clipboard path is too long")?;
        writer.write_all(&length.to_be_bytes())?;
        writer.write_all(bytes)?;
    }
    Ok(())
}

fn read_upload_response(bytes: &[u8], expected_count: usize) -> Result<Vec<String>> {
    let mut reader = io::Cursor::new(bytes);
    let mut magic = [0_u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != UPLOAD_RESPONSE_MAGIC {
        return Err("invalid remote clipboard upload response".into());
    }
    let mut count = [0_u8; 4];
    reader.read_exact(&mut count)?;
    if u32::from_be_bytes(count) as usize != expected_count {
        return Err("remote clipboard upload returned the wrong path count".into());
    }
    let mut paths = Vec::with_capacity(expected_count);
    for _ in 0..expected_count {
        let mut length = [0_u8; 4];
        reader.read_exact(&mut length)?;
        let length = usize::try_from(u32::from_be_bytes(length))?;
        if length == 0 || length > 16 * 1024 {
            return Err("remote clipboard upload returned an invalid path".into());
        }
        let mut path = vec![0_u8; length];
        reader.read_exact(&mut path)?;
        let path = String::from_utf8(path)?;
        if path.chars().any(char::is_control) {
            return Err("remote clipboard upload returned an invalid path".into());
        }
        paths.push(path);
    }
    if reader.position() != bytes.len() as u64 {
        return Err("remote clipboard upload returned trailing data".into());
    }
    Ok(paths)
}

fn sanitize_upload_name(name: &str) -> String {
    let mut safe = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(96)
        .collect::<String>();
    if safe.is_empty() || safe == "." || safe == ".." {
        safe = "clipboard".to_string();
    }
    safe
}

fn validate_upload_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 96
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("invalid clipboard upload name".into());
    }
    Ok(())
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
    use std::{fs, io::Cursor, path::PathBuf, process::Command, sync::atomic::Ordering};

    use super::{
        read_upload_response, receive_upload_batch, sanitize_upload_name, validate_upload_name,
        write_upload_item_header, write_upload_response, Connection, UploadControl,
        NEXT_CONTROL_ID, REMOTE_BRIDGE_COMMAND, REMOTE_UPLOAD_COMMAND, UPLOAD_REQUEST_MAGIC,
        UPLOAD_RETENTION,
    };

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "plux-upload-test-{}-{}",
                std::process::id(),
                NEXT_CONTROL_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn remote_bridge_finds_cargo_installed_plux() {
        assert_eq!(
            REMOTE_BRIDGE_COMMAND,
            r#"PATH="$HOME/.cargo/bin:$PATH" exec plux __bridge"#
        );
        assert_eq!(
            REMOTE_UPLOAD_COMMAND,
            r#"PATH="$HOME/.cargo/bin:$PATH" exec plux __upload"#
        );
    }

    #[test]
    fn local_connection_interface_is_available() {
        assert!(std::mem::size_of::<Connection>() > 0);
    }

    #[test]
    fn upload_names_are_sanitized_at_the_boundary() {
        assert_eq!(sanitize_upload_name("screen shot.png"), "screen_shot.png");
        assert_eq!(sanitize_upload_name("../"), ".._");
        assert!(validate_upload_name("screen_shot.png").is_ok());
        assert!(validate_upload_name("../secret").is_err());
    }

    #[test]
    fn upload_batch_round_trips_files_and_response_paths() {
        let directory = TestDir::new();
        let mut request = Vec::new();
        request.extend_from_slice(UPLOAD_REQUEST_MAGIC);
        request.extend_from_slice(&2_u32.to_be_bytes());
        write_upload_item_header(&mut request, "one.txt", 3).unwrap();
        request.extend_from_slice(b"one");
        write_upload_item_header(&mut request, "two.txt", 3).unwrap();
        request.extend_from_slice(b"two");

        let paths = receive_upload_batch(&directory.0, &mut Cursor::new(request)).unwrap();
        assert_eq!(fs::read(&paths[0]).unwrap(), b"one");
        assert_eq!(fs::read(&paths[1]).unwrap(), b"two");

        let mut response = Vec::new();
        write_upload_response(&mut response, &paths).unwrap();
        assert_eq!(read_upload_response(&response, 2).unwrap(), paths);
    }

    #[test]
    fn failed_upload_batch_removes_every_partial_file() {
        let directory = TestDir::new();
        let mut request = Vec::new();
        request.extend_from_slice(UPLOAD_REQUEST_MAGIC);
        request.extend_from_slice(&2_u32.to_be_bytes());
        write_upload_item_header(&mut request, "one.txt", 3).unwrap();
        request.extend_from_slice(b"one");
        write_upload_item_header(&mut request, "two.txt", 5).unwrap();
        request.extend_from_slice(b"no");

        assert!(receive_upload_batch(&directory.0, &mut Cursor::new(request)).is_err());
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
    }

    #[test]
    fn upload_control_cancels_and_reaps_its_ssh_child() {
        let control = UploadControl::default();
        control
            .install(Command::new("sh").args(["-c", "sleep 5"]).spawn().unwrap())
            .unwrap();
        control.cancel();
        assert!(!control.wait().unwrap().success());
    }

    #[test]
    fn expired_uploads_are_removed() {
        let directory = TestDir::new();
        let path = directory.0.join("old.png");
        fs::write(&path, b"old").unwrap();

        super::cleanup_expired_uploads_at(
            &directory.0,
            std::time::SystemTime::now() + UPLOAD_RETENTION + std::time::Duration::from_secs(1),
        );

        assert!(!path.exists());
    }
}
