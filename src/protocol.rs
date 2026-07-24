use std::io::{self, Read, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::error::Result;

pub const VERSION: u16 = 3;
pub const CLIENT_TOKEN_LENGTH: usize = 32;
const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    Create {
        name: String,
        rows: u16,
        cols: u16,
        command: Option<Vec<String>>,
        temporary: bool,
    },
    Attach {
        name: String,
        rows: u16,
        cols: u16,
        client_token: String,
    },
    Takeover {
        name: String,
        rows: u16,
        cols: u16,
        client_token: String,
    },
    List,
    Kill {
        name: String,
    },
    Shutdown,
    Input {
        bytes: Vec<u8>,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Scroll {
        rows: i32,
    },
    ScrollToTop,
    ScrollToBottom,
    Search {
        query: String,
        direction: i8,
    },
    Copy {
        start_row: u16,
        start_col: u16,
        end_row: u16,
        end_col: u16,
        mode: CopyMode,
    },
    Split {
        vertical: bool,
    },
    ClosePane,
    Rename {
        name: String,
    },
    AdjustRatio {
        delta: i16,
    },
    Focus {
        direction: String,
    },
    Zoom,
    Detach,
    Ping,
    Heartbeat,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    Character,
    Line,
    Rectangle,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    Created {
        name: String,
    },
    Attached {
        name: String,
        pane_id: u64,
    },
    Snapshot {
        rows: u16,
        cols: u16,
        data: String,
        mouse_enabled: bool,
        #[serde(default)]
        alternate_screen: bool,
    },
    Sessions {
        names: Vec<String>,
    },
    SearchResult {
        found: bool,
    },
    Copied {
        text: String,
    },
    ProcessExited {
        pane_id: u64,
        status: String,
        #[serde(default)]
        session_finished: bool,
    },
    Detached,
    Ok,
    Pong,
    HeartbeatAck,
    Error {
        message: String,
    },
}

pub fn validate_client_token(token: &str) -> Result<()> {
    if token.len() != CLIENT_TOKEN_LENGTH
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("invalid client token".into());
    }
    Ok(())
}

pub fn write_message<W: Write, T: Serialize>(writer: &mut W, message: &T) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err("protocol frame is too large".into());
    }
    let length = u32::try_from(payload.len()).map_err(|_| "protocol frame is too large")?;
    writer.write_all(&VERSION.to_be_bytes())?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_message<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<Option<T>> {
    let mut header = [0; 6];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }

    let version = u16::from_be_bytes([header[0], header[1]]);
    if version != VERSION {
        return Err(format!("unsupported protocol version: {version}").into());
    }
    let length = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;
    if length > MAX_FRAME_SIZE {
        return Err("protocol frame is too large".into());
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok(Some(serde_json::from_slice(&payload)?))
}

#[cfg(test)]
mod tests {
    use super::{read_message, validate_client_token, write_message, ClientMessage, VERSION};

    #[test]
    fn round_trips_messages() {
        let message = ClientMessage::Input {
            bytes: vec![0, 1, 27, 255],
        };
        let mut encoded = Vec::new();
        write_message(&mut encoded, &message).unwrap();
        let decoded: ClientMessage = read_message(&mut encoded.as_slice()).unwrap().unwrap();
        assert!(matches!(decoded, ClientMessage::Input { bytes } if bytes == vec![0, 1, 27, 255]));
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(&(VERSION + 1).to_be_bytes());
        encoded.extend_from_slice(&0_u32.to_be_bytes());
        let error = read_message::<_, ClientMessage>(&mut encoded.as_slice()).unwrap_err();
        assert!(error.to_string().contains("unsupported protocol version"));
    }

    #[test]
    fn rejects_unknown_message_variant() {
        let payload = br#"{"FutureMessage":{}}"#;
        let mut encoded = Vec::new();
        encoded.extend_from_slice(&VERSION.to_be_bytes());
        encoded.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        encoded.extend_from_slice(payload);
        assert!(read_message::<_, ClientMessage>(&mut encoded.as_slice()).is_err());
    }

    #[test]
    fn rejects_oversized_frame_before_allocation() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(&VERSION.to_be_bytes());
        encoded.extend_from_slice(&u32::MAX.to_be_bytes());
        let error = read_message::<_, ClientMessage>(&mut encoded.as_slice()).unwrap_err();
        assert_eq!(error.to_string(), "protocol frame is too large");
    }

    #[test]
    fn validates_client_tokens() {
        assert!(validate_client_token(&"a".repeat(32)).is_ok());
        assert!(validate_client_token(&"A".repeat(32)).is_err());
        assert!(validate_client_token(&"a".repeat(31)).is_err());
        assert!(validate_client_token(&"g".repeat(32)).is_err());
    }
}
