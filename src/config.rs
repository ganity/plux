use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::error::Result;

#[derive(Clone, Debug)]
pub struct Config {
    pub default_shell: String,
    pub prefix: String,
    pub scrollback_lines: usize,
    pub scrollback_bytes: usize,
    pub mouse: bool,
    pub refresh_rate: u16,
    pub copy_command: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    default_shell: Option<String>,
    prefix: Option<String>,
    scrollback_lines: Option<usize>,
    scrollback_bytes: Option<String>,
    mouse: Option<bool>,
    refresh_rate: Option<u16>,
    copy_command: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_shell: env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
            prefix: "Ctrl-A".to_string(),
            scrollback_lines: 20_000,
            scrollback_bytes: 64 * 1024 * 1024,
            mouse: true,
            refresh_rate: 60,
            copy_command: None,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let mut config = Self::default();
        let path = config_path();
        if !path.is_file() {
            return Ok(config);
        }

        let contents = fs::read_to_string(&path)?;
        let file: FileConfig = toml::from_str(&contents)
            .map_err(|error| format!("invalid config {}: {error}", path.display()))?;

        if let Some(value) = file.default_shell {
            config.default_shell = value;
        }
        if let Some(value) = file.prefix {
            config.prefix = value;
        }
        if let Some(value) = file.scrollback_lines {
            config.scrollback_lines = value.max(1);
        }
        if let Some(value) = file.scrollback_bytes {
            config.scrollback_bytes = parse_size(&value)?;
        }
        if let Some(value) = file.mouse {
            config.mouse = value;
        }
        if let Some(value) = file.refresh_rate {
            config.refresh_rate = value.clamp(1, 240);
        }
        config.copy_command = file.copy_command;
        Ok(config)
    }

    pub fn runtime_dir(&self) -> Result<PathBuf> {
        let base = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(env::temp_dir);
        let name = env::var("USER").unwrap_or_else(|_| "user".to_string());
        let path = base.join(format!("plux-{name}"));
        fs::create_dir_all(&path)?;
        set_private_permissions(&path)?;
        Ok(path)
    }

    pub fn session_metadata_dir(&self) -> Result<PathBuf> {
        let path = self.runtime_dir()?.join("sessions");
        fs::create_dir_all(&path)?;
        set_private_permissions(&path)?;
        Ok(path)
    }

    pub fn session_metadata_path(&self, name: &str) -> Result<PathBuf> {
        Ok(self.session_metadata_dir()?.join(format!("{name}.json")))
    }

    pub fn prefix_byte(&self) -> Result<u8> {
        parse_prefix(&self.prefix)
    }
}

fn config_path() -> PathBuf {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("plux/config.toml");
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join(".config/plux/config.toml")
}

fn parse_size(value: &str) -> Result<usize> {
    let value = value.trim();
    let split_at = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, suffix) = value.split_at(split_at);
    let number: usize = number
        .parse()
        .map_err(|_| format!("invalid byte size: {value}"))?;
    let multiplier = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "KB" | "KIB" => 1024,
        "MB" | "MIB" => 1024 * 1024,
        "GB" | "GIB" => 1024 * 1024 * 1024,
        _ => return Err(format!("invalid byte size suffix: {value}").into()),
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("byte size is too large: {value}").into())
}

fn parse_prefix(value: &str) -> Result<u8> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("ctrl-space") || value.eq_ignore_ascii_case("ctrl-@") {
        return Ok(0);
    }
    if value.eq_ignore_ascii_case("ctrl-]") {
        return Ok(0x1d);
    }
    let Some(letter) = value
        .strip_prefix("Ctrl-")
        .or_else(|| value.strip_prefix("ctrl-"))
    else {
        return Err(format!("unsupported prefix: {value}").into());
    };
    let [byte] = letter.as_bytes() else {
        return Err(format!("unsupported prefix: {value}").into());
    };
    if byte.is_ascii_alphabetic() {
        return Ok(byte.to_ascii_uppercase() - b'@');
    }
    Err(format!("unsupported prefix: {value}").into())
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_prefix, parse_size, Config};

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("64MB").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_size("2KB").unwrap(), 2 * 1024);
        assert!(parse_size("oops").is_err());
    }

    #[test]
    fn parses_prefixes() {
        assert_eq!(parse_prefix("Ctrl-Space").unwrap(), 0);
        assert_eq!(parse_prefix("Ctrl-A").unwrap(), 1);
        assert_eq!(parse_prefix("Ctrl-]").unwrap(), 0x1d);
        assert_eq!(Config::default().prefix_byte().unwrap(), 1);
        assert!(parse_prefix("Alt-A").is_err());
    }
}
