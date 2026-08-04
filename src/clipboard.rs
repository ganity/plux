use std::{
    fs::{self, File},
    io::{Cursor, Read},
    path::PathBuf,
};

use clipboard_rs::{common::RustImage, Clipboard, ClipboardContext, ContentFormat};

use crate::error::Result;

pub enum ClipboardPayload {
    Text,
    Items(Vec<ClipboardItem>),
}

pub enum ClipboardItem {
    File { path: PathBuf, size: u64 },
    Bytes { name: String, bytes: Vec<u8> },
}

impl ClipboardItem {
    pub fn size(&self) -> u64 {
        match self {
            Self::File { size, .. } => *size,
            Self::Bytes { bytes, .. } => bytes.len() as u64,
        }
    }

    pub fn into_reader(self) -> Result<(String, u64, Box<dyn Read + Send>)> {
        match self {
            Self::File { path, size } => {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or("clipboard file name is not valid UTF-8")?
                    .to_string();
                Ok((name, size, Box::new(File::open(path)?)))
            }
            Self::Bytes { name, bytes } => {
                let size = bytes.len() as u64;
                Ok((name, size, Box::new(Cursor::new(bytes))))
            }
        }
    }
}

pub fn read() -> Result<ClipboardPayload> {
    let clipboard = ClipboardContext::new()?;

    if clipboard.has(ContentFormat::Files) {
        let files = clipboard.get_files()?;
        if files.is_empty() {
            return Err("clipboard file list is empty".into());
        }
        let mut items = Vec::with_capacity(files.len());
        for file in files {
            let path = PathBuf::from(file);
            let metadata = fs::metadata(&path)?;
            if !metadata.is_file() {
                return Err(
                    format!("clipboard item is not a regular file: {}", path.display()).into(),
                );
            }
            items.push(ClipboardItem::File {
                path,
                size: metadata.len(),
            });
        }
        return Ok(ClipboardPayload::Items(items));
    }

    if clipboard.has(ContentFormat::Image) {
        let image = clipboard.get_image()?;
        return Ok(ClipboardPayload::Items(vec![ClipboardItem::Bytes {
            name: "clipboard.png".to_string(),
            bytes: image.to_png()?.get_bytes().to_vec(),
        }]));
    }

    if clipboard.has(ContentFormat::Text) {
        clipboard.get_text()?;
        return Ok(ClipboardPayload::Text);
    }
    Err("clipboard does not contain text, images, or files".into())
}

pub fn write_text(text: &str) -> Result<()> {
    let clipboard = ClipboardContext::new()?;
    clipboard.set_text(text.to_string())?;
    Ok(())
}
