use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::SizeLimits;
use std::thread;
use std::time::Duration;

mod files;
mod fingerprint;
mod image_payload;
mod platform;
mod rich_text;
mod types;

use files::{
    encode_file_bundle_payload, read_file_list, write_file_bundle, write_file_bundle_from_dir,
    write_file_bundle_from_path,
};
pub(crate) use files::{
    is_internal_file_payload, stream_file_bundle_archive, unpack_file_bundle_archive_reader,
};
pub(crate) use fingerprint::payload_content_hash;
use image_payload::{encode_image_payload, read_image_payload, write_image_payload};
use rich_text::{read_rich_text_payload, write_rich_text_payload};
use types::AppliedClipboardWrite;
pub use types::ClipboardError;

const CLIPBOARD_IO_RETRIES: usize = 10;
const CLIPBOARD_IO_DELAY_MS: u64 = 40;

pub fn read_snapshot(limits: &SizeLimits) -> Result<ClipboardPayload, ClipboardError> {
    retry_clipboard_io(|| read_snapshot_once(limits))
}

fn read_snapshot_once(limits: &SizeLimits) -> Result<ClipboardPayload, ClipboardError> {
    if let Some(file_paths) = read_file_list()? {
        return encode_file_bundle_payload(file_paths, limits);
    }

    if let Some(payload) = read_image_payload(limits)? {
        return Ok(payload);
    }

    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| ClipboardError::Backend(e.to_string()))?;

    if let Ok(image) = clipboard.get_image() {
        let rgba = image::RgbaImage::from_raw(
            image.width as u32,
            image.height as u32,
            image.bytes.into_owned(),
        )
        .ok_or_else(|| ClipboardError::Backend("invalid image buffer".to_string()))?;

        return encode_image_payload(rgba, limits);
    }

    if let Some(payload) = read_rich_text_payload(limits)? {
        return Ok(payload);
    }

    if let Ok(text) = clipboard.get_text() {
        let size_bytes = text.as_bytes().len() as u64;
        if size_bytes > limits.max_item_bytes {
            return Err(ClipboardError::TooLarge {
                size_bytes,
                limit_bytes: limits.max_item_bytes,
            });
        }
        return Ok(ClipboardPayload::Text { text });
    }

    Err(ClipboardError::Unsupported)
}

pub fn write_item(
    item: &ClipboardItem,
    limits: &SizeLimits,
) -> Result<AppliedClipboardWrite, ClipboardError> {
    retry_clipboard_io(|| write_item_once(item, limits))
}

fn write_item_once(
    item: &ClipboardItem,
    limits: &SizeLimits,
) -> Result<AppliedClipboardWrite, ClipboardError> {
    if item.size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes: item.size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }

    match &item.payload {
        ClipboardPayload::Text { text } => {
            let mut clipboard =
                arboard::Clipboard::new().map_err(|e| ClipboardError::Backend(e.to_string()))?;
            clipboard
                .set_text(text.clone())
                .map_err(|e| ClipboardError::Backend(e.to_string()))?;
            Ok(AppliedClipboardWrite::default())
        }
        ClipboardPayload::ImagePng { png_bytes } => {
            write_image_payload(png_bytes, limits)?;
            Ok(AppliedClipboardWrite::default())
        }
        ClipboardPayload::FileBundle {
            archive_bytes,
            top_level_names,
        } => write_file_bundle(item, archive_bytes, top_level_names, limits),
        ClipboardPayload::FileBundlePath {
            archive_path,
            top_level_names,
        } => write_file_bundle_from_path(item, archive_path, top_level_names, limits),
        ClipboardPayload::FileBundleDir {
            bundle_dir,
            top_level_names,
        } => write_file_bundle_from_dir(bundle_dir, top_level_names),
        ClipboardPayload::FileList { .. } => Err(ClipboardError::Unsupported),
        ClipboardPayload::Html { html } => {
            write_rich_text_payload("html", html)?;
            Ok(AppliedClipboardWrite::default())
        }
        ClipboardPayload::Rtf { rtf } => {
            write_rich_text_payload("rtf", rtf)?;
            Ok(AppliedClipboardWrite::default())
        }
    }
}

fn retry_clipboard_io<T, F>(mut action: F) -> Result<T, ClipboardError>
where
    F: FnMut() -> Result<T, ClipboardError>,
{
    let mut last_backend_error = None;

    for attempt in 0..CLIPBOARD_IO_RETRIES {
        match action() {
            Ok(value) => return Ok(value),
            Err(error @ ClipboardError::Backend(_)) => {
                last_backend_error = Some(error);
                if attempt + 1 < CLIPBOARD_IO_RETRIES {
                    thread::sleep(Duration::from_millis(CLIPBOARD_IO_DELAY_MS));
                }
            }
            Err(error) => return Err(error),
        }
    }

    Err(last_backend_error.unwrap_or_else(|| {
        ClipboardError::Backend("clipboard backend exhausted retries".to_string())
    }))
}
