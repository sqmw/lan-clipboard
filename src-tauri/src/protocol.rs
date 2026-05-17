use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ClipboardPayload {
    Text { text: String },
    ImagePng { png_base64: String },
    Html { html: String },
    Rtf { rtf: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardItem {
    pub id: String,
    pub created_at_ms: u64,
    pub source_device_id: String,
    pub size_bytes: u64,
    pub payload: ClipboardPayload,
}

