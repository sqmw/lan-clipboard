use super::metrics::now_us;
use crate::clipboard;
use crate::protocol::{ClipboardItem, ClipboardPayload};
use std::path::Path;
use uuid::Uuid;

pub fn build_item(
    payload: &ClipboardPayload,
    device_id: &str,
) -> Result<Option<ClipboardItem>, crate::clipboard::ClipboardError> {
    let size_bytes = match payload {
        ClipboardPayload::FileList {
            estimated_archive_bytes,
            ..
        } => *estimated_archive_bytes,
        ClipboardPayload::Text { text } => text.len() as u64,
        ClipboardPayload::ImagePng { png_bytes } => png_bytes.len() as u64,
        ClipboardPayload::FileBundleDir { bundle_dir, .. } => dir_size_bytes(bundle_dir),
        ClipboardPayload::Html { html } => html.len() as u64,
        ClipboardPayload::Rtf { rtf } => rtf.len() as u64,
    };
    if size_bytes == 0 {
        return Ok(None);
    }

    let created_at_us = now_us();
    let content_hash = clipboard::payload_content_hash(payload)?;

    Ok(Some(ClipboardItem {
        id: Uuid::new_v4().to_string(),
        content_hash,
        created_at_us,
        source_device_id: device_id.to_string(),
        size_bytes,
        payload: payload.clone(),
    }))
}

pub fn new_device_id() -> String {
    Uuid::new_v4().to_string()
}

fn dir_size_bytes(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    if metadata.is_file() {
        return metadata.len();
    }
    if !metadata.is_dir() {
        return 0;
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| dir_size_bytes(&entry.path()))
                .sum()
        })
        .unwrap_or(0)
}
