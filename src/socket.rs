use std::{
    env, fs, io,
    os::unix::net::UnixStream,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use crate::{config::Config, error::Result};

pub fn socket_path(config: &Config) -> Result<PathBuf> {
    Ok(config.runtime_dir()?.join("plux.sock"))
}

pub fn connect(config: &Config) -> Result<UnixStream> {
    Ok(UnixStream::connect(socket_path(config)?)?)
}

pub fn connect_or_start(config: &Config) -> Result<UnixStream> {
    match connect(config) {
        Ok(stream) => return Ok(stream),
        Err(error) if can_start_daemon(error.as_ref()) => {}
        Err(error) => return Err(error),
    }

    let executable = env::current_exe()?;
    let mut daemon = Command::new(executable);
    let stderr = if env::var_os("PLUX_DEBUG").is_some() {
        Stdio::inherit()
    } else {
        Stdio::null()
    };
    daemon
        .arg("__daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    daemon.process_group(0).spawn()?;

    for _ in 0..100 {
        if let Ok(stream) = connect(config) {
            return Ok(stream);
        }
        thread::sleep(Duration::from_millis(20));
    }

    Err("timed out waiting for plux daemon".into())
}

fn can_start_daemon(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    })
}

pub fn remove_socket(path: &PathBuf) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn set_private_socket(path: &PathBuf) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::can_start_daemon;

    #[test]
    fn only_starts_for_an_absent_or_stale_socket() {
        assert!(can_start_daemon(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(can_start_daemon(&io::Error::from(
            io::ErrorKind::ConnectionRefused
        )));
        assert!(!can_start_daemon(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }
}
