use std::path::PathBuf;

/// Application-local clipboard representation.
///
/// This type intentionally does not implement serde. Several variants contain
/// machine-local paths and must never become part of a network or IPC schema.
#[derive(Debug, Clone)]
pub enum ClipboardPayload {
    Text {
        text: String,
    },
    ImagePng {
        png_bytes: Vec<u8>,
    },
    FileBundleDir {
        bundle_dir: PathBuf,
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
            ClipboardPayload::FileBundleDir { .. } | ClipboardPayload::FileList { .. } => {
                "file_bundle"
            }
            ClipboardPayload::Html { .. } => "html",
            ClipboardPayload::Rtf { .. } => "rtf",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClipboardItem {
    pub id: String,
    pub content_hash: String,
    pub created_at_us: u64,
    pub source_device_id: String,
    pub size_bytes: u64,
    pub payload: ClipboardPayload,
}
