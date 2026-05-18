use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::SizeLimits;
use base64::Engine;
use image::ImageEncoder;
use std::thread;
use std::time::Duration;
use thiserror::Error;

const CLIPBOARD_IO_RETRIES: usize = 10;
const CLIPBOARD_IO_DELAY_MS: u64 = 40;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard backend error: {0}")]
    Backend(String),
    #[error("payload too large: size_bytes={size_bytes} limit_bytes={limit_bytes}")]
    TooLarge { size_bytes: u64, limit_bytes: u64 },
    #[error("unsupported clipboard content")]
    Unsupported,
}

pub fn read_snapshot(limits: &SizeLimits) -> Result<ClipboardPayload, ClipboardError> {
    retry_clipboard_io(|| read_snapshot_once(limits))
}

fn read_snapshot_once(limits: &SizeLimits) -> Result<ClipboardPayload, ClipboardError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| ClipboardError::Backend(e.to_string()))?;

    if let Ok(image) = clipboard.get_image() {
        let rgba = image::RgbaImage::from_raw(
            image.width as u32,
            image.height as u32,
            image.bytes.into_owned(),
        )
        .ok_or_else(|| ClipboardError::Backend("invalid image buffer".to_string()))?;

        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(
                &rgba,
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| ClipboardError::Backend(e.to_string()))?;

        let size_bytes = png_bytes.len() as u64;
        if size_bytes > limits.max_item_bytes {
            return Err(ClipboardError::TooLarge {
                size_bytes,
                limit_bytes: limits.max_item_bytes,
            });
        }

        return Ok(ClipboardPayload::ImagePng {
            png_base64: base64::engine::general_purpose::STANDARD.encode(png_bytes),
        });
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

pub fn write_item(item: &ClipboardItem, limits: &SizeLimits) -> Result<(), ClipboardError> {
    retry_clipboard_io(|| write_item_once(item, limits))
}

fn write_item_once(item: &ClipboardItem, limits: &SizeLimits) -> Result<(), ClipboardError> {
    if item.size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes: item.size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }

    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| ClipboardError::Backend(e.to_string()))?;

    match &item.payload {
        ClipboardPayload::Text { text } => clipboard
            .set_text(text.clone())
            .map_err(|e| ClipboardError::Backend(e.to_string())),
        ClipboardPayload::ImagePng { png_base64 } => {
            let png_bytes = base64::engine::general_purpose::STANDARD
                .decode(png_base64.as_bytes())
                .map_err(|e| ClipboardError::Backend(e.to_string()))?;

            let size_bytes = png_bytes.len() as u64;
            if size_bytes > limits.max_item_bytes {
                return Err(ClipboardError::TooLarge {
                    size_bytes,
                    limit_bytes: limits.max_item_bytes,
                });
            }

            let img = image::load_from_memory(&png_bytes)
                .map_err(|e| ClipboardError::Backend(e.to_string()))?
                .to_rgba8();

            let width = img.width() as usize;
            let height = img.height() as usize;
            let bytes = img.into_raw();

            clipboard
                .set_image(arboard::ImageData {
                    width,
                    height,
                    bytes: std::borrow::Cow::Owned(bytes),
                })
                .map_err(|e| ClipboardError::Backend(e.to_string()))
        }
        ClipboardPayload::Html { .. } | ClipboardPayload::Rtf { .. } => {
            Err(ClipboardError::Unsupported)
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
