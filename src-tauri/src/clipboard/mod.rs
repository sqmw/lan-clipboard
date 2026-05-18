use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::SizeLimits;
use base64::Engine;
use image::imageops::FilterType as ResizeFilterType;
use image::ImageEncoder;
use image::RgbaImage;
use std::fs::{self, File};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;
use tar::{Archive, Builder, Header};
use thiserror::Error;

const CLIPBOARD_IO_RETRIES: usize = 10;
const CLIPBOARD_IO_DELAY_MS: u64 = 40;
const IMAGE_SCALE_NUMERATOR: u32 = 85;
const IMAGE_SCALE_DENOMINATOR: u32 = 100;
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

    match &item.payload {
        ClipboardPayload::Text { text } => {
            let mut clipboard =
                arboard::Clipboard::new().map_err(|e| ClipboardError::Backend(e.to_string()))?;
            clipboard
                .set_text(text.clone())
                .map_err(|e| ClipboardError::Backend(e.to_string()))
        }
        ClipboardPayload::ImagePng { png_bytes } => write_image_payload(png_bytes, limits),
        ClipboardPayload::FileBundle {
            archive_bytes,
            top_level_names,
        } => write_file_bundle(item, archive_bytes, top_level_names, limits),
        ClipboardPayload::Html { html } => write_rich_text_payload("html", html),
        ClipboardPayload::Rtf { rtf } => write_rich_text_payload("rtf", rtf),
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

fn encode_image_payload(
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

fn write_image_payload(png_bytes: &[u8], limits: &SizeLimits) -> Result<(), ClipboardError> {
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

fn read_rich_text_payload(limits: &SizeLimits) -> Result<Option<ClipboardPayload>, ClipboardError> {
    #[cfg(target_os = "macos")]
    {
        return read_rich_text_payload_macos(limits);
    }

    #[cfg(target_os = "windows")]
    {
        return read_rich_text_payload_windows(limits);
    }

    #[allow(unreachable_code)]
    Ok(None)
}

fn read_image_payload(_limits: &SizeLimits) -> Result<Option<ClipboardPayload>, ClipboardError> {
    #[cfg(target_os = "windows")]
    {
        return read_image_payload_windows(_limits);
    }

    #[allow(unreachable_code)]
    Ok(None)
}

fn write_rich_text_payload(format: &str, value: &str) -> Result<(), ClipboardError> {
    #[cfg(target_os = "macos")]
    {
        return write_rich_text_payload_macos(format, value);
    }

    #[cfg(target_os = "windows")]
    {
        return write_rich_text_payload_windows(format, value);
    }

    #[allow(unreachable_code)]
    Err(ClipboardError::Unsupported)
}

fn encode_file_bundle_payload(
    file_paths: Vec<PathBuf>,
    limits: &SizeLimits,
) -> Result<ClipboardPayload, ClipboardError> {
    let top_level_names = file_paths
        .iter()
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect::<Vec<_>>();

    let archive_bytes = build_file_bundle_archive(&file_paths)?;
    let size_bytes = archive_bytes.len() as u64;
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }

    Ok(ClipboardPayload::FileBundle {
        archive_bytes,
        top_level_names,
    })
}

fn build_file_bundle_archive(file_paths: &[PathBuf]) -> Result<Vec<u8>, ClipboardError> {
    let mut bytes = Vec::new();
    {
        let mut builder = Builder::new(&mut bytes);
        for path in file_paths {
            let entry_name = path
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    ClipboardError::Backend("clipboard file missing name".to_string())
                })?;
            append_path_to_archive(&mut builder, path, Path::new(&entry_name))?;
        }
        builder
            .finish()
            .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    }
    Ok(bytes)
}

fn append_path_to_archive(
    builder: &mut Builder<&mut Vec<u8>>,
    source: &Path,
    archive_path: &Path,
) -> Result<(), ClipboardError> {
    let metadata = fs::metadata(source).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if metadata.is_dir() {
        let mut header = Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_data(&mut header, archive_path, std::io::empty())
            .map_err(|e| ClipboardError::Backend(e.to_string()))?;

        for child in fs::read_dir(source).map_err(|e| ClipboardError::Backend(e.to_string()))? {
            let child = child.map_err(|e| ClipboardError::Backend(e.to_string()))?;
            let child_path = child.path();
            let child_archive_path = archive_path.join(child.file_name());
            append_path_to_archive(builder, &child_path, &child_archive_path)?;
        }
        return Ok(());
    }

    let mut file = File::open(source).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    builder
        .append_file(archive_path, &mut file)
        .map_err(|e| ClipboardError::Backend(e.to_string()))
}

fn write_file_bundle(
    item: &ClipboardItem,
    archive_bytes: &[u8],
    top_level_names: &[String],
    limits: &SizeLimits,
) -> Result<(), ClipboardError> {
    let size_bytes = archive_bytes.len() as u64;
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }

    let bundle_dir = std::env::temp_dir().join("lan-clipboard").join(&item.id);
    if bundle_dir.exists() {
        fs::remove_dir_all(&bundle_dir).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    }
    fs::create_dir_all(&bundle_dir).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    unpack_archive_into(archive_bytes, &bundle_dir)?;

    let restored_paths = top_level_names
        .iter()
        .map(|name| bundle_dir.join(name))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    if restored_paths.is_empty() {
        return Err(ClipboardError::Backend(
            "restored clipboard file bundle is empty".to_string(),
        ));
    }

    write_file_list(&restored_paths)
}

fn unpack_archive_into(bytes: &[u8], destination: &Path) -> Result<(), ClipboardError> {
    let cursor = Cursor::new(bytes);
    let mut archive = Archive::new(cursor);
    let entries = archive
        .entries()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| ClipboardError::Backend(e.to_string()))?;
        entry
            .unpack_in(destination)
            .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    }
    Ok(())
}

fn read_file_list() -> Result<Option<Vec<PathBuf>>, ClipboardError> {
    #[cfg(target_os = "macos")]
    {
        return read_file_list_macos();
    }

    #[cfg(target_os = "windows")]
    {
        return read_file_list_windows();
    }

    #[allow(unreachable_code)]
    Ok(None)
}

fn write_file_list(paths: &[PathBuf]) -> Result<(), ClipboardError> {
    #[cfg(target_os = "macos")]
    {
        return write_file_list_macos(paths);
    }

    #[cfg(target_os = "windows")]
    {
        return write_file_list_windows(paths);
    }

    #[allow(unreachable_code)]
    Err(ClipboardError::Unsupported)
}

#[cfg(target_os = "macos")]
fn read_file_list_macos() -> Result<Option<Vec<PathBuf>>, ClipboardError> {
    let script = r#"
import AppKit

let pasteboard = NSPasteboard.general
let classes: [AnyClass] = [NSURL.self]
let objects = pasteboard.readObjects(forClasses: classes, options: nil) as? [URL] ?? []
for url in objects {
    print(url.path)
}
"#;
    let output = Command::new("swift")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(None);
    }
    Ok(Some(paths))
}

#[cfg(target_os = "macos")]
fn write_file_list_macos(paths: &[PathBuf]) -> Result<(), ClipboardError> {
    let script = r#"
import AppKit
import Foundation

let rawPaths = ProcessInfo.processInfo.environment["LAN_CLIPBOARD_PATHS"] ?? ""
let paths = rawPaths
    .split(separator: "\n", omittingEmptySubsequences: true)
    .map(String.init)
let urls = paths.map { URL(fileURLWithPath: $0) } as [NSURL]
let pasteboard = NSPasteboard.general
pasteboard.clearContents()
if !pasteboard.writeObjects(urls) {
    fputs("failed to write file URLs to pasteboard\n", stderr)
    exit(1)
}
"#;
    let mut command = Command::new("swift");
    command.arg("-e").arg(script);
    command.env(
        "LAN_CLIPBOARD_PATHS",
        paths
            .iter()
            .map(|path| path.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let output = command
        .output()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(ClipboardError::Backend(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

#[cfg(target_os = "macos")]
fn read_rich_text_payload_macos(
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    let script = r#"
import AppKit
import Foundation

let pasteboard = NSPasteboard.general
if let data = pasteboard.data(forType: .html) {
    print("html")
    print(data.base64EncodedString())
} else if let data = pasteboard.data(forType: .rtf) {
    print("rtf")
    print(data.base64EncodedString())
}
"#;
    let output = Command::new("swift")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }

    parse_rich_text_output(&output.stdout, limits)
}

#[cfg(target_os = "macos")]
fn write_rich_text_payload_macos(format: &str, value: &str) -> Result<(), ClipboardError> {
    let script = r#"
import AppKit
import Foundation

let format = ProcessInfo.processInfo.environment["LAN_CLIPBOARD_FORMAT"] ?? ""
let raw = ProcessInfo.processInfo.environment["LAN_CLIPBOARD_RICH_TEXT"] ?? ""
guard let data = Data(base64Encoded: raw) else {
    fputs("failed to decode rich text payload\n", stderr)
    exit(1)
}

let pasteboard = NSPasteboard.general
pasteboard.clearContents()

let type: NSPasteboard.PasteboardType
switch format {
case "html":
    type = .html
case "rtf":
    type = .rtf
default:
    fputs("unsupported rich text format\n", stderr)
    exit(1)
}

if !pasteboard.setData(data, forType: type) {
    fputs("failed to write rich text payload\n", stderr)
    exit(1)
}
"#;
    let output = Command::new("swift")
        .arg("-e")
        .arg(script)
        .env("LAN_CLIPBOARD_FORMAT", format)
        .env(
            "LAN_CLIPBOARD_RICH_TEXT",
            base64::engine::general_purpose::STANDARD.encode(value.as_bytes()),
        )
        .output()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(ClipboardError::Backend(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
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
        let _clip = Clipboard::new_attempts(CLIPBOARD_IO_RETRIES)
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

    let _clip = Clipboard::new_attempts(CLIPBOARD_IO_RETRIES)
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

    let _clip = Clipboard::new_attempts(CLIPBOARD_IO_RETRIES)
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
    use clipboard_win::{formats, Clipboard, Setter};

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

    let _clip = Clipboard::new_attempts(CLIPBOARD_IO_RETRIES).map_err(|e| {
        ClipboardError::Backend(format!("open windows clipboard for image write: {e}"))
    })?;
    formats::Bitmap
        .write_clipboard(&bmp_bytes)
        .map_err(|e| ClipboardError::Backend(format!("write windows bitmap: {e}")))
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

#[cfg(target_os = "windows")]
fn read_file_list_windows() -> Result<Option<Vec<PathBuf>>, ClipboardError> {
    use clipboard_win::{formats, is_format_avail, Clipboard, Getter};

    if !is_format_avail(formats::CF_HDROP) {
        return Ok(None);
    }

    let _clip = Clipboard::new_attempts(CLIPBOARD_IO_RETRIES).map_err(|e| {
        ClipboardError::Backend(format!("open windows clipboard for file list: {e}"))
    })?;
    let mut path_strings = Vec::<String>::new();
    formats::FileList
        .read_clipboard(&mut path_strings)
        .map_err(|e| ClipboardError::Backend(format!("read windows file list: {e}")))?;
    let paths = path_strings
        .into_iter()
        .map(|path| path.trim().to_string())
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(None);
    }
    Ok(Some(paths))
}

#[cfg(target_os = "windows")]
fn write_file_list_windows(paths: &[PathBuf]) -> Result<(), ClipboardError> {
    use clipboard_win::{formats, Clipboard, Setter};

    let path_strings = paths
        .iter()
        .map(|path| path.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let _clip = Clipboard::new_attempts(CLIPBOARD_IO_RETRIES).map_err(|e| {
        ClipboardError::Backend(format!("open windows clipboard for file list write: {e}"))
    })?;
    formats::FileList
        .write_clipboard(path_strings.as_slice())
        .map_err(|e| ClipboardError::Backend(format!("write windows file list: {e}")))
}

#[cfg(target_os = "windows")]
fn read_rich_text_payload_windows(
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    if let Some(payload) = read_html_payload_windows(limits)? {
        return Ok(Some(payload));
    }

    let script = r#"
Add-Type -AssemblyName System.Windows.Forms
if ([System.Windows.Forms.Clipboard]::ContainsData([System.Windows.Forms.DataFormats]::Rtf)) {
  $value = [string][System.Windows.Forms.Clipboard]::GetData([System.Windows.Forms.DataFormats]::Rtf)
  Write-Output "rtf"
  Write-Output ([Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($value)))
}
"#;
    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-STA")
        .arg("-Command")
        .arg(script)
        .output()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }

    parse_rich_text_output(&output.stdout, limits)
}

#[cfg(target_os = "windows")]
fn read_html_payload_windows(
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    use clipboard_win::{formats, is_format_avail, Clipboard, Getter};

    let Some(format) = formats::Html::new() else {
        return Ok(None);
    };
    if !is_format_avail(format.code()) {
        return Ok(None);
    }

    let _clip = Clipboard::new_attempts(CLIPBOARD_IO_RETRIES)
        .map_err(|e| ClipboardError::Backend(format!("open windows clipboard for html: {e}")))?;
    let mut html = String::new();
    format
        .read_clipboard(&mut html)
        .map_err(|e| ClipboardError::Backend(format!("read windows html: {e}")))?;
    let size_bytes = html.as_bytes().len() as u64;
    if size_bytes == 0 {
        return Ok(None);
    }
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }
    Ok(Some(ClipboardPayload::Html { html }))
}

#[cfg(target_os = "windows")]
fn write_rich_text_payload_windows(format: &str, value: &str) -> Result<(), ClipboardError> {
    if format == "html" {
        return write_html_payload_windows(value);
    }

    let script = r#"
Add-Type -AssemblyName System.Windows.Forms
$format = $env:LAN_CLIPBOARD_FORMAT
$raw = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($env:LAN_CLIPBOARD_RICH_TEXT))
$dataObject = New-Object System.Windows.Forms.DataObject
switch ($format) {
  "html" { $dataObject.SetData([System.Windows.Forms.DataFormats]::Html, $raw) }
  "rtf" { $dataObject.SetData([System.Windows.Forms.DataFormats]::Rtf, $raw) }
  default { throw "unsupported rich text format" }
}
[System.Windows.Forms.Clipboard]::SetDataObject($dataObject, $true)
"#;
    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-STA")
        .arg("-Command")
        .arg(script)
        .env("LAN_CLIPBOARD_FORMAT", format)
        .env(
            "LAN_CLIPBOARD_RICH_TEXT",
            base64::engine::general_purpose::STANDARD.encode(value.as_bytes()),
        )
        .output()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(ClipboardError::Backend(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

#[cfg(target_os = "windows")]
fn write_html_payload_windows(value: &str) -> Result<(), ClipboardError> {
    use clipboard_win::{formats, Clipboard, Setter};

    let Some(format) = formats::Html::new() else {
        return Err(ClipboardError::Backend(
            "register windows html clipboard format failed".to_string(),
        ));
    };
    let _clip = Clipboard::new_attempts(CLIPBOARD_IO_RETRIES).map_err(|e| {
        ClipboardError::Backend(format!("open windows clipboard for html write: {e}"))
    })?;
    format
        .write_clipboard(&value)
        .map_err(|e| ClipboardError::Backend(format!("write windows html: {e}")))
}

fn parse_rich_text_output(
    stdout: &[u8],
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    let output = String::from_utf8_lossy(stdout);
    let mut lines = output.lines();
    let Some(kind) = lines.next().map(str::trim).filter(|line| !line.is_empty()) else {
        return Ok(None);
    };
    let Some(payload_base64) = lines.next().map(str::trim).filter(|line| !line.is_empty()) else {
        return Ok(None);
    };

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload_base64.as_bytes())
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    let size_bytes = bytes.len() as u64;
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }
    let value = String::from_utf8(bytes).map_err(|e| ClipboardError::Backend(e.to_string()))?;

    match kind {
        "html" => Ok(Some(ClipboardPayload::Html { html: value })),
        "rtf" => Ok(Some(ClipboardPayload::Rtf { rtf: value })),
        _ => Ok(None),
    }
}
