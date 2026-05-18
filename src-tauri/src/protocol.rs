use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipboardPayload {
    Text {
        text: String,
    },
    ImagePng {
        png_bytes: Vec<u8>,
    },
    FileBundle {
        archive_bytes: Vec<u8>,
        top_level_names: Vec<String>,
    },
    FileList {
        paths: Vec<PathBuf>,
        top_level_names: Vec<String>,
        estimated_archive_bytes: u64,
    },
    Html {
        html: String,
    },
    Rtf {
        rtf: String,
    },
}

impl ClipboardPayload {
    pub fn kind(&self) -> &'static str {
        match self {
            ClipboardPayload::Text { .. } => "text",
            ClipboardPayload::ImagePng { .. } => "image_png",
            ClipboardPayload::FileBundle { .. } | ClipboardPayload::FileList { .. } => {
                "file_bundle"
            }
            ClipboardPayload::Html { .. } => "html",
            ClipboardPayload::Rtf { .. } => "rtf",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardItem {
    pub id: String,
    pub content_hash: String,
    pub created_at_ms: u64,
    pub source_device_id: String,
    pub size_bytes: u64,
    pub payload: ClipboardPayload,
}
