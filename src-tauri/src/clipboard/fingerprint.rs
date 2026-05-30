use super::ClipboardError;
use crate::protocol::ClipboardPayload;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use tar::Archive;

const FILE_HASH_SAMPLE_BYTES: usize = 64 * 1024;

pub(crate) fn payload_content_hash(payload: &ClipboardPayload) -> Result<String, ClipboardError> {
    match payload {
        ClipboardPayload::Text { text } => Ok(hex_hash(text.as_bytes())),
        ClipboardPayload::ImagePng { png_bytes } => Ok(hex_hash(png_bytes)),
        ClipboardPayload::Html { html } => Ok(hex_hash(html.as_bytes())),
        ClipboardPayload::Rtf { rtf } => Ok(hex_hash(rtf.as_bytes())),
        ClipboardPayload::FileList { paths, .. } => hash_file_list(paths),
        ClipboardPayload::FileBundle { archive_bytes, .. } => {
            hash_file_bundle_archive(archive_bytes)
        }
        ClipboardPayload::FileBundlePath { archive_path, .. } => {
            hash_file_bundle_archive_path(archive_path)
        }
        ClipboardPayload::FileBundleDir {
            bundle_dir,
            top_level_names,
        } => hash_file_bundle_dir(bundle_dir, top_level_names),
    }
}

pub(super) fn hash_file_list(file_paths: &[PathBuf]) -> Result<String, ClipboardError> {
    let mut entries = file_paths
        .iter()
        .map(|path| {
            let top_level_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    ClipboardError::Backend("clipboard file missing name".to_string())
                })?;
            Ok((top_level_name, path.clone()))
        })
        .collect::<Result<Vec<_>, ClipboardError>>()?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let mut records = Vec::new();
    for (name, path) in entries {
        collect_path_hash_records(&path, Path::new(&name), &mut records)?;
    }
    Ok(finalize_hash_records(records))
}

fn collect_path_hash_records(
    source: &Path,
    relative_path: &Path,
    records: &mut Vec<HashRecord>,
) -> Result<(), ClipboardError> {
    let metadata = fs::metadata(source).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    let normalized_path = normalize_hash_path(relative_path);
    if metadata.is_dir() {
        records.push(HashRecord {
            kind: "dir".to_string(),
            path: normalized_path,
            size_bytes: 0,
            sample_hash: None,
        });

        let mut children = fs::read_dir(source)
            .map_err(|e| ClipboardError::Backend(e.to_string()))?
            .map(|child| child.map_err(|e| ClipboardError::Backend(e.to_string())))
            .collect::<Result<Vec<_>, ClipboardError>>()?;
        children.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
        for child in children {
            let child_path = child.path();
            let child_relative_path = relative_path.join(child.file_name());
            collect_path_hash_records(&child_path, &child_relative_path, records)?;
        }
        return Ok(());
    }

    records.push(HashRecord {
        kind: "file".to_string(),
        path: normalized_path,
        size_bytes: metadata.len(),
        sample_hash: Some(sample_file_prefix_hash(source)?),
    });
    Ok(())
}

fn hash_file_bundle_archive(archive_bytes: &[u8]) -> Result<String, ClipboardError> {
    let cursor = Cursor::new(archive_bytes);
    hash_file_bundle_archive_reader(cursor)
}

fn hash_file_bundle_archive_path(archive_path: &Path) -> Result<String, ClipboardError> {
    let file = File::open(archive_path).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    hash_file_bundle_archive_reader(file)
}

fn hash_file_bundle_archive_reader<R: Read>(reader: R) -> Result<String, ClipboardError> {
    let mut archive = Archive::new(reader);
    let mut records = Vec::new();

    let entries = archive
        .entries()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| ClipboardError::Backend(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| ClipboardError::Backend(e.to_string()))?
            .into_owned();
        let normalized_path = normalize_hash_path(&path);
        if entry.header().entry_type().is_dir() {
            records.push(HashRecord {
                kind: "dir".to_string(),
                path: normalized_path,
                size_bytes: 0,
                sample_hash: None,
            });
            continue;
        }

        let size_bytes = entry.header().size().unwrap_or(0);
        records.push(HashRecord {
            kind: "file".to_string(),
            path: normalized_path,
            size_bytes,
            sample_hash: Some(sample_reader_prefix_hash(&mut entry)?),
        });
    }

    Ok(finalize_hash_records(records))
}

fn hash_file_bundle_dir(
    bundle_dir: &Path,
    top_level_names: &[String],
) -> Result<String, ClipboardError> {
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
    hash_file_list(&restored_paths)
}

#[derive(Debug)]
struct HashRecord {
    kind: String,
    path: String,
    size_bytes: u64,
    sample_hash: Option<String>,
}

fn finalize_hash_records(mut records: Vec<HashRecord>) -> String {
    records.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.kind.cmp(&right.kind))
    });

    let mut hasher = Sha256::new();
    for record in records {
        hasher.update(record.kind.as_bytes());
        hasher.update([0]);
        hasher.update(record.path.as_bytes());
        hasher.update([0]);
        hasher.update(record.size_bytes.to_le_bytes());
        hasher.update([0]);
        if let Some(sample_hash) = record.sample_hash {
            hasher.update(sample_hash.as_bytes());
        }
        hasher.update([0xff]);
    }
    format!("{:x}", hasher.finalize())
}

fn sample_file_prefix_hash(path: &Path) -> Result<String, ClipboardError> {
    let mut file = File::open(path).map_err(|e| ClipboardError::Backend(e.to_string()))?;
    sample_reader_prefix_hash(&mut file)
}

fn sample_reader_prefix_hash<R: Read>(reader: &mut R) -> Result<String, ClipboardError> {
    let mut remaining = FILE_HASH_SAMPLE_BYTES;
    let mut buffer = [0u8; 8 * 1024];
    let mut hasher = Sha256::new();

    while remaining > 0 {
        let read_limit = remaining.min(buffer.len());
        let read_bytes = reader
            .read(&mut buffer[..read_limit])
            .map_err(|e| ClipboardError::Backend(e.to_string()))?;
        if read_bytes == 0 {
            break;
        }
        hasher.update(&buffer[..read_bytes]);
        remaining -= read_bytes;
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn normalize_hash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn hex_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}
