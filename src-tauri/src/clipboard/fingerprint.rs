use super::file_access::VerifiedPath;
use super::path_policy::{resolve_restored_paths, MAX_ARCHIVE_DEPTH, MAX_ARCHIVE_ENTRIES};
use super::ClipboardError;
use crate::protocol::ClipboardPayload;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const FILE_HASH_BUFFER_BYTES: usize = 64 * 1024;

pub(crate) fn payload_content_hash(payload: &ClipboardPayload) -> Result<String, ClipboardError> {
    match payload {
        ClipboardPayload::Text { text } => Ok(hex_hash(text.as_bytes())),
        ClipboardPayload::ImagePng { png_bytes } => Ok(hex_hash(png_bytes)),
        ClipboardPayload::Html { html } => Ok(hex_hash(html.as_bytes())),
        ClipboardPayload::Rtf { rtf } => Ok(hex_hash(rtf.as_bytes())),
        ClipboardPayload::FileList { paths, .. } => hash_file_list(paths),
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

    let mut records = FileTreeHash::new();
    let mut entry_count = 0usize;
    for (name, path) in entries {
        collect_path_hash_records(&path, Path::new(&name), &mut records, 1, &mut entry_count)?;
    }
    Ok(records.finalize())
}

fn collect_path_hash_records(
    source: &Path,
    relative_path: &Path,
    records: &mut FileTreeHash,
    depth: usize,
    entry_count: &mut usize,
) -> Result<(), ClipboardError> {
    collect_verified_path_hash_records(source, relative_path, records, depth, entry_count, None)
}

fn collect_verified_path_hash_records(
    source: &Path,
    relative_path: &Path,
    records: &mut FileTreeHash,
    depth: usize,
    entry_count: &mut usize,
    parent: Option<(&VerifiedPath, &Path)>,
) -> Result<(), ClipboardError> {
    if depth > MAX_ARCHIVE_DEPTH || *entry_count >= MAX_ARCHIVE_ENTRIES {
        return Err(ClipboardError::Backend(
            "file fingerprint traversal limit exceeded".to_string(),
        ));
    }
    *entry_count = entry_count.saturating_add(1);
    let mut verified = VerifiedPath::open(source)?;
    if let Some((parent, parent_path)) = parent {
        parent.verify_still_at(parent_path)?;
    }
    let metadata = verified.metadata().clone();
    let normalized_path = normalize_hash_path(relative_path);
    if verified.is_dir() {
        records.push_directory_normalized(normalized_path);

        let mut children = fs::read_dir(source)
            .map_err(|e| ClipboardError::Backend(e.to_string()))?
            .map(|child| {
                child
                    .map(|child| (child.file_name(), child.path()))
                    .map_err(|e| ClipboardError::Backend(e.to_string()))
            })
            .collect::<Result<Vec<_>, ClipboardError>>()?;
        verified.verify_unchanged_at(source)?;
        children.sort_by(|left, right| left.0.cmp(&right.0));
        for (child_name, child_path) in children {
            let child_relative_path = relative_path.join(child_name);
            collect_verified_path_hash_records(
                &child_path,
                &child_relative_path,
                records,
                depth + 1,
                entry_count,
                Some((&verified, source)),
            )?;
        }
        verified.verify_unchanged_at(source)?;
        return Ok(());
    }

    let content_hash = hash_reader_contents(verified.file_mut()?)?;
    verified.verify_unchanged_at(source)?;
    records.push_file_normalized(normalized_path, metadata.len(), content_hash);
    Ok(())
}

fn hash_file_bundle_dir(
    bundle_dir: &Path,
    top_level_names: &[String],
) -> Result<String, ClipboardError> {
    let restored_paths = resolve_restored_paths(bundle_dir, top_level_names)?;
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
    content_hash: Option<String>,
}

pub(super) struct FileTreeHash {
    records: Vec<HashRecord>,
}

impl FileTreeHash {
    pub(super) fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    pub(super) fn push_directory(&mut self, path: &Path) {
        self.push_directory_normalized(normalize_hash_path(path));
    }

    fn push_directory_normalized(&mut self, path: String) {
        self.records.push(HashRecord {
            kind: "dir".to_string(),
            path,
            size_bytes: 0,
            content_hash: None,
        });
    }

    pub(super) fn push_file(&mut self, path: &Path, size_bytes: u64, content_hash: String) {
        self.push_file_normalized(normalize_hash_path(path), size_bytes, content_hash);
    }

    fn push_file_normalized(&mut self, path: String, size_bytes: u64, content_hash: String) {
        self.records.push(HashRecord {
            kind: "file".to_string(),
            path,
            size_bytes,
            content_hash: Some(content_hash),
        });
    }

    pub(super) fn finalize(mut self) -> String {
        self.records.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.kind.cmp(&right.kind))
        });

        let mut hasher = Sha256::new();
        for record in self.records {
            hasher.update(record.kind.as_bytes());
            hasher.update([0]);
            hasher.update(record.path.as_bytes());
            hasher.update([0]);
            hasher.update(record.size_bytes.to_le_bytes());
            hasher.update([0]);
            if let Some(content_hash) = record.content_hash {
                hasher.update(content_hash.as_bytes());
            }
            hasher.update([0xff]);
        }
        format!("{:x}", hasher.finalize())
    }
}

fn hash_reader_contents<R: Read>(reader: &mut R) -> Result<String, ClipboardError> {
    let mut buffer = [0u8; FILE_HASH_BUFFER_BYTES];
    let mut hasher = Sha256::new();

    loop {
        let read_bytes = reader
            .read(&mut buffer)
            .map_err(|e| ClipboardError::Backend(e.to_string()))?;
        if read_bytes == 0 {
            break;
        }
        hasher.update(&buffer[..read_bytes]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn full_reader_hash_detects_changes_after_the_old_sample_boundary() {
        let mut original = vec![b'a'; FILE_HASH_BUFFER_BYTES + 1];
        let mut changed = original.clone();
        changed[FILE_HASH_BUFFER_BYTES] = b'b';

        let original_hash = hash_reader_contents(&mut Cursor::new(&original)).unwrap();
        let changed_hash = hash_reader_contents(&mut Cursor::new(&changed)).unwrap();

        assert_ne!(original_hash, changed_hash);

        original[FILE_HASH_BUFFER_BYTES] = b'b';
        let matching_hash = hash_reader_contents(&mut Cursor::new(&original)).unwrap();
        assert_eq!(matching_hash, changed_hash);
    }

    #[test]
    fn file_list_hash_rejects_excessive_directory_depth() {
        let root = std::env::temp_dir().join(format!(
            "lan-clipboard-fingerprint-depth-{}",
            uuid::Uuid::new_v4()
        ));
        let top = root.join("payload");
        fs::create_dir_all(&top).expect("create top-level fixture");
        let mut current = top.clone();
        for index in 0..MAX_ARCHIVE_DEPTH {
            current = current.join(format!("d{index}"));
            fs::create_dir(&current).expect("create nested fixture");
        }

        let error = hash_file_list(&[top]).expect_err("depth limit must be enforced");
        assert!(error.to_string().contains("traversal limit"));
        fs::remove_dir_all(root).expect("remove depth fixture");
    }
}
