use super::types::ClipboardError;
use crate::protocol::ClipboardPayload;
use crate::settings::SizeLimits;
use image::imageops::FilterType as ResizeFilterType;
use image::ImageEncoder;
use image::ImageFormat;
use image::ImageReader;
use image::RgbaImage;
use std::io::Cursor;

const IMAGE_SCALE_NUMERATOR: u32 = 85;
const IMAGE_SCALE_DENOMINATOR: u32 = 100;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_DECODE_BYTES: u64 = 80 * 1024 * 1024;
/// Security bound for encoded image input before decoding. This is separate
/// from the user-selected sync size because decoded clipboard bitmaps can
/// require substantially more memory than their PNG representation.
pub(crate) const MAX_IMAGE_SOURCE_BYTES: u64 = 80 * 1024 * 1024;

pub(super) fn encode_image_payload(
    mut rgba: RgbaImage,
    limits: &SizeLimits,
) -> Result<ClipboardPayload, ClipboardError> {
    validate_image_dimensions(rgba.width(), rgba.height())?;

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
        return write_image_payload_windows(png_bytes, limits);
    }

    #[allow(unreachable_code)]
    write_image_payload_arboard(png_bytes, limits)
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

fn write_image_payload_arboard(
    png_bytes: &[u8],
    _limits: &SizeLimits,
) -> Result<(), ClipboardError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| ClipboardError::Backend(e.to_string()))?;
    let img = decode_image_with_limits(png_bytes, ImageFormat::Png, "decode clipboard png")?;

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

fn decode_image_with_limits(
    encoded_bytes: &[u8],
    format: ImageFormat,
    context: &str,
) -> Result<RgbaImage, ClipboardError> {
    validate_encoded_image_dimensions(encoded_bytes, format, context)?;

    let mut reader = ImageReader::with_format(Cursor::new(encoded_bytes), format);
    reader.limits(image_decode_limits());
    reader
        .decode()
        .map_err(|e| ClipboardError::Backend(format!("{context}: {e}")))
        .map(|image| image.to_rgba8())
}

fn validate_encoded_image_dimensions(
    encoded_bytes: &[u8],
    format: ImageFormat,
    context: &str,
) -> Result<(), ClipboardError> {
    ensure_image_source_size(encoded_bytes.len(), context)?;

    let mut reader = ImageReader::with_format(Cursor::new(encoded_bytes), format);
    reader.limits(image_decode_limits());
    let (width, height) = reader
        .into_dimensions()
        .map_err(|e| ClipboardError::Backend(format!("{context}: {e}")))?;
    validate_image_dimensions(width, height)
}

fn validate_image_dimensions(width: u32, height: u32) -> Result<(), ClipboardError> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| ClipboardError::Backend("image dimensions overflow".to_string()))?;
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || pixels > MAX_IMAGE_PIXELS
    {
        return Err(ClipboardError::Backend(format!(
            "image dimensions exceed decode limits: width={width} height={height} pixels={pixels} max_dimension={MAX_IMAGE_DIMENSION} max_pixels={MAX_IMAGE_PIXELS}"
        )));
    }
    Ok(())
}

fn ensure_image_source_size(size: usize, context: &str) -> Result<(), ClipboardError> {
    let size_bytes = size as u64;
    if size_bytes > MAX_IMAGE_SOURCE_BYTES {
        return Err(ClipboardError::Backend(format!(
            "{context} exceeds image decode source limit: size_bytes={size_bytes} limit_bytes={MAX_IMAGE_SOURCE_BYTES}"
        )));
    }
    Ok(())
}

fn image_decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES);
    limits
}

#[cfg(target_os = "windows")]
fn validate_windows_clipboard_raw_size(format: u32, context: &str) -> Result<(), ClipboardError> {
    if let Some(size) = clipboard_win::raw::size(format) {
        ensure_image_source_size(size.get(), context)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn validate_windows_bitmap_dimensions() -> Result<(), ClipboardError> {
    use clipboard_win::{formats, raw, types::BITMAP};
    use std::ffi::c_void;

    #[link(name = "gdi32")]
    extern "system" {
        fn GetObjectW(handle: *mut c_void, buffer_bytes: i32, buffer: *mut c_void) -> i32;
    }

    let handle = raw::get_clipboard_data(formats::CF_BITMAP)
        .map_err(|e| ClipboardError::Backend(format!("inspect windows bitmap handle: {e}")))?;
    let mut bitmap = BITMAP {
        bmType: 0,
        bmWidth: 0,
        bmHeight: 0,
        bmWidthBytes: 0,
        bmPlanes: 0,
        bmBitsPixel: 0,
        bmBits: std::ptr::null_mut(),
    };
    let result = unsafe {
        GetObjectW(
            handle.as_ptr(),
            std::mem::size_of::<BITMAP>() as i32,
            (&mut bitmap as *mut BITMAP).cast(),
        )
    };
    if result == 0 {
        return Err(ClipboardError::Backend(
            "inspect windows bitmap dimensions failed".to_string(),
        ));
    }

    validate_image_dimensions(
        bitmap.bmWidth.unsigned_abs(),
        bitmap.bmHeight.unsigned_abs(),
    )
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
        validate_windows_clipboard_raw_size(png_format.get(), "windows png")?;
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
    validate_windows_clipboard_raw_size(format, "windows dib")?;
    let mut dib_bytes = Vec::new();
    formats::RawData(format)
        .read_clipboard(&mut dib_bytes)
        .map_err(|e| ClipboardError::Backend(format!("read windows dib: {e}")))?;
    if dib_bytes.is_empty() {
        return Ok(None);
    }

    ensure_image_source_size(dib_bytes.len(), "windows dib")?;
    let bmp_bytes = dib_to_bmp_bytes(&dib_bytes)?;
    let rgba = decode_image_with_limits(&bmp_bytes, ImageFormat::Bmp, "decode windows dib")?;
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
    validate_windows_bitmap_dimensions()?;
    let mut bitmap_bytes = Vec::new();
    formats::Bitmap
        .read_clipboard(&mut bitmap_bytes)
        .map_err(|e| ClipboardError::Backend(format!("read windows bitmap: {e}")))?;
    if bitmap_bytes.is_empty() {
        return Ok(None);
    }
    let rgba = decode_image_with_limits(&bitmap_bytes, ImageFormat::Bmp, "decode windows bitmap")?;
    encode_image_payload(rgba, limits).map(Some)
}

#[cfg(target_os = "windows")]
fn write_image_payload_windows(
    png_bytes: &[u8],
    _limits: &SizeLimits,
) -> Result<(), ClipboardError> {
    use clipboard_win::{formats, raw, Clipboard};

    let rgba = decode_image_with_limits(
        png_bytes,
        ImageFormat::Png,
        "decode png for windows image write",
    )?;
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
    write_windows_image_formats(
        || {
            raw::set_without_clear(formats::CF_DIB, &dib_bytes)
                .map_err(|e| ClipboardError::Backend(format!("write windows dib: {e}")))
        },
        || {
            let Some(png_format) = clipboard_win::register_format("PNG") else {
                return Ok(());
            };
            raw::set_without_clear(png_format.get(), png_bytes)
                .map_err(|e| ClipboardError::Backend(format!("write windows png: {e}")))
        },
    )
}

#[cfg(any(target_os = "windows", test))]
fn write_windows_image_formats<E>(
    write_dib: impl FnOnce() -> Result<(), E>,
    write_png: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    write_dib()?;
    // CF_DIB is the baseline Windows clipboard representation. Once it is present, an
    // optional PNG registration/write failure must not cause the caller to retry or echo a
    // clipboard update that is already usable.
    let _ = write_png();
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
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
    validate_encoded_image_dimensions(&png_bytes, ImageFormat::Png, "inspect windows png")?;
    let size_bytes = png_bytes.len() as u64;
    if size_bytes > limits.max_item_bytes {
        let rgba = decode_image_with_limits(&png_bytes, ImageFormat::Png, "decode oversized png")?;
        return encode_image_payload(rgba, limits);
    }
    Ok(ClipboardPayload::ImagePng { png_bytes })
}

#[cfg(any(target_os = "windows", test))]
fn dib_to_bmp_bytes(dib_bytes: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    const BMP_FILE_HEADER_LEN: usize = 14;
    const BITMAPINFOHEADER_LEN: usize = 40;
    const BI_BITFIELDS: u32 = 3;
    const BI_ALPHABITFIELDS: u32 = 6;

    ensure_image_source_size(dib_bytes.len(), "windows dib")?;
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
    let bitfield_mask_bytes = if header_size == BITMAPINFOHEADER_LEN {
        match compression {
            BI_BITFIELDS => 12,
            BI_ALPHABITFIELDS => 16,
            _ => 0,
        }
    } else {
        0
    };
    let palette_bytes = palette_entries
        .checked_mul(4)
        .ok_or_else(|| ClipboardError::Backend("windows dib palette size overflow".to_string()))?;
    let pixel_offset = BMP_FILE_HEADER_LEN
        .checked_add(header_size)
        .and_then(|offset| offset.checked_add(bitfield_mask_bytes))
        .and_then(|offset| offset.checked_add(palette_bytes))
        .ok_or_else(|| ClipboardError::Backend("windows dib pixel offset overflow".to_string()))?;
    let file_size = BMP_FILE_HEADER_LEN
        .checked_add(dib_bytes.len())
        .ok_or_else(|| ClipboardError::Backend("windows dib file size overflow".to_string()))?;
    if pixel_offset > file_size || file_size > u32::MAX as usize {
        return Err(ClipboardError::Backend(format!(
            "windows dib has invalid pixel offset or file size: pixel_offset={pixel_offset} file_size={file_size}"
        )));
    }

    let mut bmp_bytes = Vec::with_capacity(file_size);
    bmp_bytes.extend_from_slice(b"BM");
    bmp_bytes.extend_from_slice(&(file_size as u32).to_le_bytes());
    bmp_bytes.extend_from_slice(&0u16.to_le_bytes());
    bmp_bytes.extend_from_slice(&0u16.to_le_bytes());
    bmp_bytes.extend_from_slice(&(pixel_offset as u32).to_le_bytes());
    bmp_bytes.extend_from_slice(dib_bytes);
    Ok(bmp_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_dimensions_have_an_independent_pixel_limit() {
        assert!(validate_image_dimensions(4096, 4096).is_ok());
        assert!(validate_image_dimensions(4096, 4097).is_err());
        assert!(validate_image_dimensions(MAX_IMAGE_DIMENSION + 1, 1).is_err());
    }

    #[test]
    fn oversized_encoded_image_is_scaled_to_the_item_limit() {
        let rgba = RgbaImage::from_fn(128, 128, |x, y| {
            image::Rgba([
                (x.wrapping_mul(17) ^ y.wrapping_mul(31)) as u8,
                (x.wrapping_mul(7) ^ y.wrapping_mul(13)) as u8,
                (x.wrapping_mul(3) ^ y.wrapping_mul(5)) as u8,
                255,
            ])
        });
        let limits = SizeLimits {
            max_item_bytes: 1024,
        };

        let ClipboardPayload::ImagePng { png_bytes } = encode_image_payload(rgba, &limits).unwrap()
        else {
            panic!("expected image payload");
        };

        assert!(png_bytes.len() as u64 <= limits.max_item_bytes);
        assert!(decode_image_with_limits(&png_bytes, ImageFormat::Png, "test png").is_ok());
    }

    #[test]
    fn bmp_and_dib_conversion_round_trips_without_losing_payload() {
        let rgba = RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
        let mut bmp_bytes = Vec::new();
        image::codecs::bmp::BmpEncoder::new(&mut bmp_bytes)
            .write_image(
                &rgba,
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();

        let dib_bytes = bmp_bytes_to_dib_bytes(&bmp_bytes).unwrap();
        let rebuilt_bmp = dib_to_bmp_bytes(&dib_bytes).unwrap();

        assert_eq!(bmp_bytes_to_dib_bytes(&rebuilt_bmp).unwrap(), dib_bytes);
        assert!(decode_image_with_limits(&rebuilt_bmp, ImageFormat::Bmp, "test bmp").is_ok());
    }

    #[test]
    fn bitmap_source_size_is_independent_from_final_transfer_size() {
        let rgba = RgbaImage::from_fn(64, 64, |x, y| {
            image::Rgba([
                x.wrapping_mul(11) as u8,
                y.wrapping_mul(13) as u8,
                (x ^ y).wrapping_mul(17) as u8,
                255,
            ])
        });
        let mut bmp_bytes = Vec::new();
        image::codecs::bmp::BmpEncoder::new(&mut bmp_bytes)
            .write_image(
                &rgba,
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        let limits = SizeLimits {
            max_item_bytes: 2048,
        };
        assert!(bmp_bytes.len() as u64 > limits.max_item_bytes);

        let decoded =
            decode_image_with_limits(&bmp_bytes, ImageFormat::Bmp, "test bitmap").unwrap();
        let ClipboardPayload::ImagePng { png_bytes } =
            encode_image_payload(decoded, &limits).unwrap()
        else {
            panic!("expected image payload");
        };

        assert!(png_bytes.len() as u64 <= limits.max_item_bytes);
    }

    #[test]
    fn malformed_dib_palette_cannot_overflow_pixel_offset() {
        let mut dib = vec![0u8; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[14..16].copy_from_slice(&8u16.to_le_bytes());
        dib[32..36].copy_from_slice(&u32::MAX.to_le_bytes());

        assert!(dib_to_bmp_bytes(&dib).is_err());
    }

    #[test]
    fn windows_image_write_treats_png_as_best_effort_after_dib() {
        use std::cell::RefCell;

        let writes = RefCell::new(Vec::new());
        let result = write_windows_image_formats(
            || {
                writes.borrow_mut().push("dib");
                Ok(())
            },
            || {
                writes.borrow_mut().push("png");
                Err("png failed")
            },
        );

        assert_eq!(result, Ok(()));
        assert_eq!(*writes.borrow(), ["dib", "png"]);
    }

    #[test]
    fn windows_image_write_requires_dib_before_attempting_png() {
        use std::cell::Cell;

        let png_attempted = Cell::new(false);
        let result = write_windows_image_formats(
            || Err("dib failed"),
            || {
                png_attempted.set(true);
                Ok(())
            },
        );

        assert_eq!(result, Err("dib failed"));
        assert!(!png_attempted.get());
    }
}
