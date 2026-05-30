use super::types::ClipboardError;
use crate::protocol::ClipboardPayload;
use crate::settings::SizeLimits;
use image::imageops::FilterType as ResizeFilterType;
use image::ImageEncoder;
use image::RgbaImage;
#[cfg(target_os = "windows")]
use std::io::Cursor;

const IMAGE_SCALE_NUMERATOR: u32 = 85;
const IMAGE_SCALE_DENOMINATOR: u32 = 100;

pub(super) fn encode_image_payload(
    mut rgba: RgbaImage,
    limits: &SizeLimits,
) -> Result<ClipboardPayload, ClipboardError> {
    loop {
        let png_bytes = encode_png(&rgba)?;
        let size_bytes = png_bytes.len() as u64;
        if size_bytes <= limits.max_item_bytes {
            return Ok(ClipboardPayload::ImagePng { png_bytes });
        }

        let next_width =
            (rgba.width().saturating_mul(IMAGE_SCALE_NUMERATOR) / IMAGE_SCALE_DENOMINATOR).max(1);
        let next_height =
            (rgba.height().saturating_mul(IMAGE_SCALE_NUMERATOR) / IMAGE_SCALE_DENOMINATOR).max(1);

        if next_width == rgba.width() && next_height == rgba.height() {
            return Err(ClipboardError::TooLarge {
                size_bytes,
                limit_bytes: limits.max_item_bytes,
            });
        }

        rgba = image::imageops::resize(&rgba, next_width, next_height, ResizeFilterType::Lanczos3);
    }
}

pub(super) fn write_image_payload(
    png_bytes: &[u8],
    limits: &SizeLimits,
) -> Result<(), ClipboardError> {
    let size_bytes = png_bytes.len() as u64;
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }

    #[cfg(target_os = "windows")]
    {
        return write_image_payload_windows(png_bytes);
    }

    #[allow(unreachable_code)]
    write_image_payload_arboard(png_bytes)
}

pub(super) fn read_image_payload(
    _limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    #[cfg(target_os = "windows")]
    {
        return read_image_payload_windows(_limits);
    }

    #[allow(unreachable_code)]
    Ok(None)
}

fn encode_png(rgba: &RgbaImage) -> Result<Vec<u8>, ClipboardError> {
    let mut png_bytes = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new_with_quality(
        &mut png_bytes,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    );
    encoder
        .write_image(
            rgba,
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    Ok(png_bytes)
}

fn write_image_payload_arboard(png_bytes: &[u8]) -> Result<(), ClipboardError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| ClipboardError::Backend(e.to_string()))?;
    let img = image::load_from_memory(png_bytes)
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

#[cfg(target_os = "windows")]
fn read_image_payload_windows(
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    use clipboard_win::{formats, is_format_avail, Clipboard, Getter};

    let Some(png_format) = clipboard_win::register_format("PNG") else {
        return read_bitmap_payload_windows(limits);
    };
    if is_format_avail(png_format.get()) {
        let _clip = Clipboard::new_attempts(super::CLIPBOARD_IO_RETRIES)
            .map_err(|e| ClipboardError::Backend(format!("open windows clipboard for png: {e}")))?;
        let mut png_bytes = Vec::new();
        formats::RawData(png_format.get())
            .read_clipboard(&mut png_bytes)
            .map_err(|e| ClipboardError::Backend(format!("read windows png: {e}")))?;
        return encode_existing_png_payload(png_bytes, limits).map(Some);
    }

    if let Some(payload) = read_dib_payload_windows(limits)? {
        return Ok(Some(payload));
    }

    read_bitmap_payload_windows(limits)
}

#[cfg(target_os = "windows")]
fn read_dib_payload_windows(
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    use clipboard_win::{formats, is_format_avail, Clipboard, Getter};

    let format = if is_format_avail(formats::CF_DIBV5) {
        formats::CF_DIBV5
    } else if is_format_avail(formats::CF_DIB) {
        formats::CF_DIB
    } else {
        return Ok(None);
    };

    let _clip = Clipboard::new_attempts(super::CLIPBOARD_IO_RETRIES)
        .map_err(|e| ClipboardError::Backend(format!("open windows clipboard for dib: {e}")))?;
    let mut dib_bytes = Vec::new();
    formats::RawData(format)
        .read_clipboard(&mut dib_bytes)
        .map_err(|e| ClipboardError::Backend(format!("read windows dib: {e}")))?;
    if dib_bytes.is_empty() {
        return Ok(None);
    }

    let bmp_bytes = dib_to_bmp_bytes(&dib_bytes)?;
    let size_bytes = bmp_bytes.len() as u64;
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }

    let rgba = image::load_from_memory_with_format(&bmp_bytes, image::ImageFormat::Bmp)
        .map_err(|e| ClipboardError::Backend(format!("decode windows dib: {e}")))?
        .to_rgba8();
    encode_image_payload(rgba, limits).map(Some)
}

#[cfg(target_os = "windows")]
fn read_bitmap_payload_windows(
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    use clipboard_win::{formats, is_format_avail, Clipboard, Getter};

    if !is_format_avail(formats::CF_BITMAP) {
        return Ok(None);
    }

    let _clip = Clipboard::new_attempts(super::CLIPBOARD_IO_RETRIES)
        .map_err(|e| ClipboardError::Backend(format!("open windows clipboard for bitmap: {e}")))?;
    let mut bitmap_bytes = Vec::new();
    formats::Bitmap
        .read_clipboard(&mut bitmap_bytes)
        .map_err(|e| ClipboardError::Backend(format!("read windows bitmap: {e}")))?;
    if bitmap_bytes.is_empty() {
        return Ok(None);
    }
    let size_bytes = bitmap_bytes.len() as u64;
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }

    let rgba = image::load_from_memory_with_format(&bitmap_bytes, image::ImageFormat::Bmp)
        .map_err(|e| ClipboardError::Backend(format!("decode windows bitmap: {e}")))?
        .to_rgba8();
    encode_image_payload(rgba, limits).map(Some)
}

#[cfg(target_os = "windows")]
fn write_image_payload_windows(png_bytes: &[u8]) -> Result<(), ClipboardError> {
    use clipboard_win::{formats, raw, Clipboard, Setter};

    let image = image::load_from_memory_with_format(png_bytes, image::ImageFormat::Png)
        .map_err(|e| ClipboardError::Backend(format!("decode png for windows image write: {e}")))?;
    let rgba = image.to_rgba8();
    let mut bmp_bytes = Vec::new();
    let mut cursor = Cursor::new(&mut bmp_bytes);
    let encoder = image::codecs::bmp::BmpEncoder::new(&mut cursor);
    encoder
        .write_image(
            &rgba,
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| ClipboardError::Backend(format!("encode windows bitmap: {e}")))?;
    let dib_bytes = bmp_bytes_to_dib_bytes(&bmp_bytes)?;

    let _clip = Clipboard::new_attempts(super::CLIPBOARD_IO_RETRIES).map_err(|e| {
        ClipboardError::Backend(format!("open windows clipboard for image write: {e}"))
    })?;
    raw::empty().map_err(|e| ClipboardError::Backend(format!("clear windows clipboard: {e}")))?;
    if let Some(png_format) = clipboard_win::register_format("PNG") {
        formats::RawData(png_format.get())
            .write_clipboard(&png_bytes)
            .map_err(|e| ClipboardError::Backend(format!("write windows png: {e}")))?;
    }
    formats::RawData(formats::CF_DIB)
        .write_clipboard(&dib_bytes)
        .map_err(|e| ClipboardError::Backend(format!("write windows dib: {e}")))
}

#[cfg(target_os = "windows")]
fn bmp_bytes_to_dib_bytes(bmp_bytes: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    const BMP_FILE_HEADER_LEN: usize = 14;
    if bmp_bytes.len() <= BMP_FILE_HEADER_LEN {
        return Err(ClipboardError::Backend(
            "encoded windows bmp is missing file header".to_string(),
        ));
    }
    if &bmp_bytes[..2] != b"BM" {
        return Err(ClipboardError::Backend(
            "encoded windows bmp has invalid signature".to_string(),
        ));
    }
    Ok(bmp_bytes[BMP_FILE_HEADER_LEN..].to_vec())
}

#[cfg(target_os = "windows")]
fn encode_existing_png_payload(
    png_bytes: Vec<u8>,
    limits: &SizeLimits,
) -> Result<ClipboardPayload, ClipboardError> {
    let size_bytes = png_bytes.len() as u64;
    if size_bytes > limits.max_item_bytes {
        let rgba = image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png)
            .map_err(|e| ClipboardError::Backend(format!("decode oversized png: {e}")))?
            .to_rgba8();
        return encode_image_payload(rgba, limits);
    }
    Ok(ClipboardPayload::ImagePng { png_bytes })
}

#[cfg(target_os = "windows")]
fn dib_to_bmp_bytes(dib_bytes: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    const BMP_FILE_HEADER_LEN: usize = 14;
    const BITMAPINFOHEADER_LEN: usize = 40;
    const BI_BITFIELDS: u32 = 3;

    if dib_bytes.len() < BITMAPINFOHEADER_LEN {
        return Err(ClipboardError::Backend("windows dib too short".to_string()));
    }

    let header_size = u32::from_le_bytes(
        dib_bytes[0..4]
            .try_into()
            .map_err(|_| ClipboardError::Backend("invalid dib header size".to_string()))?,
    ) as usize;
    if header_size < BITMAPINFOHEADER_LEN || header_size > dib_bytes.len() {
        return Err(ClipboardError::Backend(format!(
            "invalid dib header size: {header_size}"
        )));
    }

    let bit_count = u16::from_le_bytes(
        dib_bytes[14..16]
            .try_into()
            .map_err(|_| ClipboardError::Backend("invalid dib bit count".to_string()))?,
    );
    let compression = u32::from_le_bytes(
        dib_bytes[16..20]
            .try_into()
            .map_err(|_| ClipboardError::Backend("invalid dib compression".to_string()))?,
    );
    let colors_used = u32::from_le_bytes(
        dib_bytes[32..36]
            .try_into()
            .map_err(|_| ClipboardError::Backend("invalid dib color count".to_string()))?,
    ) as usize;
    let palette_entries = if colors_used > 0 {
        colors_used
    } else if bit_count <= 8 {
        1usize << bit_count
    } else {
        0
    };
    let bitfield_mask_bytes = if header_size == BITMAPINFOHEADER_LEN && compression == BI_BITFIELDS
    {
        12
    } else {
        0
    };
    let pixel_offset =
        BMP_FILE_HEADER_LEN + header_size + bitfield_mask_bytes + palette_entries * 4;
    let file_size = BMP_FILE_HEADER_LEN + dib_bytes.len();

    let mut bmp_bytes = Vec::with_capacity(file_size);
    bmp_bytes.extend_from_slice(b"BM");
    bmp_bytes.extend_from_slice(&(file_size as u32).to_le_bytes());
    bmp_bytes.extend_from_slice(&0u16.to_le_bytes());
    bmp_bytes.extend_from_slice(&0u16.to_le_bytes());
    bmp_bytes.extend_from_slice(&(pixel_offset as u32).to_le_bytes());
    bmp_bytes.extend_from_slice(dib_bytes);
    Ok(bmp_bytes)
}
