use super::fingerprint::hash_file_list;
use super::types::{AppliedClipboardWrite, ClipboardError};
use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::SizeLimits;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use tar::{Archive, Builder, Header};

const FILE_ARCHIVE_READ_BUFFER_BYTES: usize = 1024 * 1024;

pub(super) fn encode_file_bundle_payload(
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

    let size_bytes = estimate_file_bundle_archive_size(&file_paths)?;
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }

    Ok(ClipboardPayload::FileList {
        paths: file_paths,
        top_level_names,
        estimated_archive_bytes: size_bytes,
    })
}

pub(crate) fn stream_file_bundle_archive<W: Write>(
    file_paths: &[PathBuf],
    writer: W,
) -> Result<(), ClipboardError> {
    let mut builder = Builder::new(writer);
    for path in file_paths {
        let entry_name = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .ok_or_else(|| ClipboardError::Backend("clipboard file missing name".to_string()))?;
        append_path_to_archive(&mut builder, path, Path::new(&entry_name))?;
    }
    builder
        .finish()
        .map_err(|e| ClipboardError::Backend(e.to_string()))
}

pub(crate) fn unpack_file_bundle_archive_reader<R: Read>(
    reader: R,
    item_id: &str,
) -> Result<PathBuf, ClipboardError> {
    let bundle_dir = internal_clipboard_root().join(item_id);
    if bundle_dir.exists() {
        fs::remove_dir_all(&bundle_dir).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    }
    fs::create_dir_all(&bundle_dir).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    unpack_archive_reader_into(reader, &bundle_dir)?;
    Ok(bundle_dir)
}

pub(super) fn write_file_bundle(
    item: &ClipboardItem,
    archive_bytes: &[u8],
    top_level_names: &[String],
    limits: &SizeLimits,
) -> Result<AppliedClipboardWrite, ClipboardError> {
    let size_bytes = archive_bytes.len() as u64;
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }

    let bundle_dir = prepare_bundle_dir(&item.id)?;
    unpack_archive_bytes_into(archive_bytes, &bundle_dir)?;
    write_restored_bundle_to_clipboard(&bundle_dir, top_level_names)
}

pub(super) fn write_file_bundle_from_path(
    item: &ClipboardItem,
    archive_path: &Path,
    top_level_names: &[String],
    limits: &SizeLimits,
) -> Result<AppliedClipboardWrite, ClipboardError> {
    let size_bytes = fs::metadata(archive_path)
        .map_err(|e| ClipboardError::Backend(e.to_string()))?
        .len();
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }

    let bundle_dir = prepare_bundle_dir(&item.id)?;
    unpack_archive_path_into(archive_path, &bundle_dir)?;
    let result = write_restored_bundle_to_clipboard(&bundle_dir, top_level_names);
    let _ = fs::remove_file(archive_path);
    result
}

pub(super) fn write_file_bundle_from_dir(
    bundle_dir: &Path,
    top_level_names: &[String],
) -> Result<AppliedClipboardWrite, ClipboardError> {
    write_restored_bundle_to_clipboard(bundle_dir, top_level_names)
}

pub(super) fn read_file_list() -> Result<Option<Vec<PathBuf>>, ClipboardError> {
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

pub(crate) fn is_internal_file_payload(payload: &ClipboardPayload) -> bool {
    let ClipboardPayload::FileList { paths, .. } = payload else {
        return false;
    };
    if paths.is_empty() {
        return false;
    }
    let internal_root = normalize_internal_path(&internal_clipboard_root());
    paths
        .iter()
        .all(|path| normalize_internal_path(path).starts_with(&internal_root))
}

fn prepare_bundle_dir(item_id: &str) -> Result<PathBuf, ClipboardError> {
    let bundle_dir = internal_clipboard_root().join(item_id);
    if bundle_dir.exists() {
        fs::remove_dir_all(&bundle_dir).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    }
    fs::create_dir_all(&bundle_dir).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    Ok(bundle_dir)
}

fn write_restored_bundle_to_clipboard(
    bundle_dir: &Path,
    top_level_names: &[String],
) -> Result<AppliedClipboardWrite, ClipboardError> {
    let restored_paths = restored_bundle_paths(bundle_dir, top_level_names);
    if restored_paths.is_empty() {
        return Err(ClipboardError::Backend(
            "restored clipboard file bundle is empty".to_string(),
        ));
    }

    write_file_list(&restored_paths)?;
    Ok(AppliedClipboardWrite {
        content_hash: Some(hash_file_list(&restored_paths)?),
    })
}

fn append_path_to_archive<W: Write>(
    builder: &mut Builder<W>,
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

    let file = File::open(source).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    let mut file = BufReader::with_capacity(FILE_ARCHIVE_READ_BUFFER_BYTES, file);
    builder
        .append_data(
            &mut archive_header_from_metadata(&metadata),
            archive_path,
            &mut file,
        )
        .map_err(|e| ClipboardError::Backend(e.to_string()))
}

fn archive_header_from_metadata(metadata: &fs::Metadata) -> Header {
    let mut header = Header::new_gnu();
    header.set_metadata(metadata);
    header.set_size(metadata.len());
    header.set_cksum();
    header
}

pub(crate) fn estimate_file_bundle_archive_size(
    file_paths: &[PathBuf],
) -> Result<u64, ClipboardError> {
    let mut total = 1024u64;
    for path in file_paths {
        estimate_path_archive_size(path, &mut total)?;
    }
    Ok(total)
}

fn estimate_path_archive_size(path: &Path, total: &mut u64) -> Result<(), ClipboardError> {
    let metadata = fs::metadata(path).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    *total = total.saturating_add(512);
    if metadata.is_dir() {
        for child in fs::read_dir(path).map_err(|e| ClipboardError::Backend(e.to_string()))? {
            let child = child.map_err(|e| ClipboardError::Backend(e.to_string()))?;
            estimate_path_archive_size(&child.path(), total)?;
        }
    } else {
        let size = metadata.len();
        *total = total.saturating_add(size.div_ceil(512).saturating_mul(512));
    }
    Ok(())
}

fn restored_bundle_paths(bundle_dir: &Path, top_level_names: &[String]) -> Vec<PathBuf> {
    top_level_names
        .iter()
        .map(|name| bundle_dir.join(name))
        .filter(|path| path.exists())
        .collect()
}

fn internal_clipboard_root() -> PathBuf {
    std::env::temp_dir().join("lan-clipboard")
}

fn normalize_internal_path(path: &Path) -> String {
    #[cfg(target_os = "windows")]
    {
        path.as_os_str()
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase()
    }

    #[cfg(not(target_os = "windows"))]
    {
        path.as_os_str().to_string_lossy().into_owned()
    }
}

fn unpack_archive_bytes_into(bytes: &[u8], destination: &Path) -> Result<(), ClipboardError> {
    let cursor = std::io::Cursor::new(bytes);
    unpack_archive_reader_into(cursor, destination)
}

fn unpack_archive_path_into(archive_path: &Path, destination: &Path) -> Result<(), ClipboardError> {
    let file = File::open(archive_path).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    unpack_archive_reader_into(file, destination)
}

fn unpack_archive_reader_into<R: Read>(
    reader: R,
    destination: &Path,
) -> Result<(), ClipboardError> {
    let mut archive = Archive::new(reader);
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

#[cfg(target_os = "windows")]
fn read_file_list_windows() -> Result<Option<Vec<PathBuf>>, ClipboardError> {
    use clipboard_win::{formats, is_format_avail, Clipboard, Getter};

    if !is_format_avail(formats::CF_HDROP) {
        return Ok(None);
    }

    let _clip = Clipboard::new_attempts(super::CLIPBOARD_IO_RETRIES).map_err(|e| {
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
    let _clip = Clipboard::new_attempts(super::CLIPBOARD_IO_RETRIES).map_err(|e| {
        ClipboardError::Backend(format!("open windows clipboard for file list write: {e}"))
    })?;
    formats::FileList
        .write_clipboard(path_strings.as_slice())
        .map_err(|e| ClipboardError::Backend(format!("write windows file list: {e}")))
}
