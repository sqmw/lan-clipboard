use super::file_access::VerifiedPath;
use super::fingerprint::{hash_file_list, FileTreeHash};
use super::path_policy::{
    create_bundle_dir, is_internal_path, remove_managed_bundle_dir, resolve_restored_paths,
    retire_managed_bundle_dir, validate_archive_path, validate_top_level_names, ReceivedBundle,
    MAX_ARCHIVE_DEPTH, MAX_ARCHIVE_ENTRIES,
};
use super::types::{AppliedClipboardWrite, ClipboardError};
use crate::protocol::ClipboardPayload;
use crate::settings::SizeLimits;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use tar::{Archive, Builder, Header};

const FILE_ARCHIVE_READ_BUFFER_BYTES: usize = 1024 * 1024;

pub(super) fn encode_file_bundle_payload(
    file_paths: Vec<PathBuf>,
    limits: &SizeLimits,
) -> Result<ClipboardPayload, ClipboardError> {
    let top_level_names = file_paths
        .iter()
        .map(|path| file_name_utf8(path, "clipboard file"))
        .collect::<Result<Vec<_>, _>>()?;
    validate_top_level_names(&top_level_names)?;

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
) -> Result<String, ClipboardError> {
    let top_level_names = file_paths
        .iter()
        .map(|path| file_name_utf8(path, "clipboard file"))
        .collect::<Result<Vec<_>, _>>()?;
    validate_top_level_names(&top_level_names)?;
    let mut builder = Builder::new(writer);
    let mut entry_count = 0usize;
    let mut fingerprint = FileTreeHash::new();
    for path in file_paths {
        let entry_name = file_name_utf8(path, "clipboard file")?;
        append_path_to_archive(
            &mut builder,
            path,
            Path::new(&entry_name),
            1,
            &mut entry_count,
            &mut fingerprint,
        )?;
    }
    builder
        .finish()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    Ok(fingerprint.finalize())
}

pub(crate) fn unpack_file_bundle_archive_reader<R: Read>(
    reader: R,
    expected_top_level_names: &[String],
    max_extracted_bytes: u64,
) -> Result<ReceivedBundle, ClipboardError> {
    let bundle_dir = create_bundle_dir()?;
    unpack_archive_reader_into(
        reader,
        bundle_dir.path(),
        expected_top_level_names,
        max_extracted_bytes,
    )?;
    Ok(bundle_dir)
}

pub(crate) fn retire_internal_file_payload(
    payload: &ClipboardPayload,
) -> Result<(), ClipboardError> {
    if let ClipboardPayload::FileBundleDir { bundle_dir, .. } = payload {
        retire_managed_bundle_dir(bundle_dir)?;
    }
    Ok(())
}

pub(crate) fn remove_internal_file_payload(
    payload: &ClipboardPayload,
) -> Result<(), ClipboardError> {
    if let ClipboardPayload::FileBundleDir { bundle_dir, .. } = payload {
        remove_managed_bundle_dir(bundle_dir)?;
    }
    Ok(())
}

pub(super) fn write_file_bundle_from_dir(
    bundle_dir: &Path,
    top_level_names: &[String],
    limits: &SizeLimits,
) -> Result<AppliedClipboardWrite, ClipboardError> {
    if !is_internal_path(bundle_dir) {
        return Err(ClipboardError::Backend(
            "file bundle directory is outside the managed receive root".to_string(),
        ));
    }
    let actual_size =
        estimate_file_bundle_archive_size(&resolve_restored_paths(bundle_dir, top_level_names)?)?;
    if actual_size > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes: actual_size,
            limit_bytes: limits.max_item_bytes,
        });
    }
    write_restored_bundle_to_clipboard(bundle_dir, top_level_names)
}

pub(super) fn read_file_list() -> Result<Option<Vec<PathBuf>>, ClipboardError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| ClipboardError::Backend(error.to_string()))?;
    match clipboard.get().file_list() {
        Ok(paths) if paths.is_empty() => Ok(None),
        Ok(paths) => Ok(Some(paths)),
        Err(arboard::Error::ClipboardOccupied) => {
            Err(ClipboardError::Backend("clipboard is occupied".to_string()))
        }
        Err(_) => Ok(None),
    }
}

pub(crate) fn is_internal_file_payload(payload: &ClipboardPayload) -> bool {
    let ClipboardPayload::FileList { paths, .. } = payload else {
        return false;
    };
    if paths.is_empty() {
        return false;
    }
    paths.iter().all(|path| is_internal_path(path))
}

fn write_restored_bundle_to_clipboard(
    bundle_dir: &Path,
    top_level_names: &[String],
) -> Result<AppliedClipboardWrite, ClipboardError> {
    let restored_paths = resolve_restored_paths(bundle_dir, top_level_names)?;
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
    depth: usize,
    entry_count: &mut usize,
    fingerprint: &mut FileTreeHash,
) -> Result<(), ClipboardError> {
    append_verified_path_to_archive(
        builder,
        source,
        archive_path,
        depth,
        entry_count,
        fingerprint,
        None,
    )
}

fn append_verified_path_to_archive<W: Write>(
    builder: &mut Builder<W>,
    source: &Path,
    archive_path: &Path,
    depth: usize,
    entry_count: &mut usize,
    fingerprint: &mut FileTreeHash,
    parent: Option<(&VerifiedPath, &Path)>,
) -> Result<(), ClipboardError> {
    if depth > MAX_ARCHIVE_DEPTH || *entry_count >= MAX_ARCHIVE_ENTRIES {
        return Err(ClipboardError::Backend(
            "file bundle traversal limit exceeded".to_string(),
        ));
    }
    validate_archive_path(archive_path)?;
    let mut verified = VerifiedPath::open(source)?;
    if let Some((parent, parent_path)) = parent {
        parent.verify_unchanged_at(parent_path)?;
    }
    let metadata = verified.metadata().clone();
    *entry_count = entry_count.saturating_add(1);
    if verified.is_dir() {
        fingerprint.push_directory(archive_path);
        let mut header = Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_data(&mut header, archive_path, std::io::empty())
            .map_err(|e| ClipboardError::Backend(e.to_string()))?;

        let mut children = fs::read_dir(source)
            .map_err(|e| ClipboardError::Backend(e.to_string()))?
            .map(|entry| {
                entry
                    .map(|entry| (entry.file_name(), entry.path()))
                    .map_err(|e| ClipboardError::Backend(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        verified.verify_unchanged_at(source)?;
        children.sort_by(|left, right| left.0.cmp(&right.0));
        for (child_name, child_path) in children {
            let child_archive_path = join_portable_archive_path(archive_path, &child_name)?;
            append_verified_path_to_archive(
                builder,
                &child_path,
                &child_archive_path,
                depth + 1,
                entry_count,
                fingerprint,
                Some((&verified, source)),
            )?;
        }
        verified.verify_unchanged_at(source)?;
        return Ok(());
    }

    if !verified.is_file() {
        return Err(ClipboardError::Backend(
            "special filesystem entries are not transferred".to_string(),
        ));
    }

    let mut file = ExactFileReader::new(verified.file_mut()?, metadata.len());
    builder
        .append_data(
            &mut portable_file_archive_header(metadata.len()),
            archive_path,
            &mut file,
        )
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    let content_hash = file.finish()?;
    verified.verify_unchanged_at(source)?;
    fingerprint.push_file(archive_path, metadata.len(), content_hash);
    Ok(())
}

fn portable_file_archive_header(size_bytes: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(size_bytes);
    header.set_cksum();
    header
}

struct ExactFileReader<'a> {
    reader: BufReader<&'a mut File>,
    remaining: u64,
    digest: Sha256,
}

impl<'a> ExactFileReader<'a> {
    fn new(file: &'a mut File, expected_bytes: u64) -> Self {
        Self {
            reader: BufReader::with_capacity(FILE_ARCHIVE_READ_BUFFER_BYTES, file),
            remaining: expected_bytes,
            digest: Sha256::new(),
        }
    }

    fn finish(self) -> Result<String, ClipboardError> {
        if self.remaining != 0 {
            return Err(ClipboardError::Backend(
                "clipboard file changed while it was being archived".to_string(),
            ));
        }
        Ok(format!("{:x}", self.digest.finalize()))
    }
}

impl Read for ExactFileReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 || output.is_empty() {
            return Ok(0);
        }
        let limit =
            usize::try_from(self.remaining.min(output.len() as u64)).unwrap_or(output.len());
        let read = self.reader.read(&mut output[..limit])?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "clipboard file changed while it was being archived",
            ));
        }
        self.remaining = self.remaining.saturating_sub(read as u64);
        self.digest.update(&output[..read]);
        Ok(read)
    }
}

pub(crate) fn estimate_file_bundle_archive_size(
    file_paths: &[PathBuf],
) -> Result<u64, ClipboardError> {
    let names = file_paths
        .iter()
        .map(|path| file_name_utf8(path, "clipboard file"))
        .collect::<Result<Vec<_>, _>>()?;
    validate_top_level_names(&names)?;
    let mut total = 1024u64;
    let mut entry_count = 0usize;
    for (path, name) in file_paths.iter().zip(names.iter()) {
        estimate_path_archive_size(path, Path::new(name), &mut total, 1, &mut entry_count)?;
    }
    Ok(total)
}

fn estimate_path_archive_size(
    path: &Path,
    archive_path: &Path,
    total: &mut u64,
    depth: usize,
    entry_count: &mut usize,
) -> Result<(), ClipboardError> {
    estimate_verified_path_archive_size(path, archive_path, total, depth, entry_count, None)
}

fn estimate_verified_path_archive_size(
    path: &Path,
    archive_path: &Path,
    total: &mut u64,
    depth: usize,
    entry_count: &mut usize,
    parent: Option<(&VerifiedPath, &Path)>,
) -> Result<(), ClipboardError> {
    if depth > MAX_ARCHIVE_DEPTH || *entry_count >= MAX_ARCHIVE_ENTRIES {
        return Err(ClipboardError::Backend(
            "file bundle traversal limit exceeded".to_string(),
        ));
    }
    let verified = VerifiedPath::open(path)?;
    if let Some((parent, parent_path)) = parent {
        parent.verify_unchanged_at(parent_path)?;
    }
    let metadata = verified.metadata();
    *entry_count = entry_count.saturating_add(1);
    validate_archive_path(archive_path)?;
    add_archive_entry_size(
        total,
        archive_path,
        if verified.is_file() {
            metadata.len()
        } else {
            0
        },
    )?;
    if verified.is_dir() {
        let mut children = fs::read_dir(path)
            .map_err(|e| ClipboardError::Backend(e.to_string()))?
            .map(|entry| {
                entry
                    .map(|entry| (entry.file_name(), entry.path()))
                    .map_err(|e| ClipboardError::Backend(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        verified.verify_unchanged_at(path)?;
        children.sort_by(|left, right| left.0.cmp(&right.0));
        for (child_name, child_path) in children {
            let child_archive_path = join_portable_archive_path(archive_path, &child_name)?;
            estimate_verified_path_archive_size(
                &child_path,
                &child_archive_path,
                total,
                depth + 1,
                entry_count,
                Some((&verified, path)),
            )?;
        }
        verified.verify_unchanged_at(path)?;
    } else if !verified.is_file() {
        return Err(ClipboardError::Backend(
            "special filesystem entries are not transferred".to_string(),
        ));
    }
    Ok(())
}

fn join_portable_archive_path(
    parent: &Path,
    child_name: &std::ffi::OsStr,
) -> Result<PathBuf, ClipboardError> {
    let parent = parent
        .to_str()
        .ok_or_else(|| ClipboardError::Backend("archive path is not valid UTF-8".to_string()))?;
    let child_name = child_name.to_str().ok_or_else(|| {
        ClipboardError::Backend("archive path component is not valid UTF-8".to_string())
    })?;
    validate_archive_path(Path::new(child_name))?;
    Ok(PathBuf::from(format!("{parent}/{child_name}")))
}

fn add_archive_entry_size(
    total: &mut u64,
    archive_path: &Path,
    content_bytes: u64,
) -> Result<(), ClipboardError> {
    let path_text = archive_path
        .to_str()
        .ok_or_else(|| ClipboardError::Backend("clipboard path is not valid UTF-8".to_string()))?;
    let mut probe = Header::new_gnu();
    let long_name_bytes = if probe.set_path(archive_path).is_err() {
        512u64
            .checked_add(
                (path_text.len() as u64 + 1)
                    .div_ceil(512)
                    .saturating_mul(512),
            )
            .ok_or_else(|| ClipboardError::Backend("archive size overflow".to_string()))?
    } else {
        0
    };
    let entry_bytes = 512u64
        .checked_add(content_bytes.div_ceil(512).saturating_mul(512))
        .and_then(|value| value.checked_add(long_name_bytes))
        .ok_or_else(|| ClipboardError::Backend("archive size overflow".to_string()))?;
    *total = total
        .checked_add(entry_bytes)
        .ok_or_else(|| ClipboardError::Backend("archive size overflow".to_string()))?;
    Ok(())
}

fn file_name_utf8(path: &Path, label: &str) -> Result<String, ClipboardError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| ClipboardError::Backend(format!("{label} is missing a UTF-8 name")))
}

fn unpack_archive_reader_into<R: Read>(
    reader: R,
    destination: &Path,
    expected_top_level_names: &[String],
    max_extracted_bytes: u64,
) -> Result<(), ClipboardError> {
    validate_top_level_names(expected_top_level_names)?;
    let expected_top_level = expected_top_level_names
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut observed_top_level = HashSet::with_capacity(expected_top_level.len());
    let mut observed_paths = HashSet::new();
    let mut archive = Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;

    let mut entry_count = 0usize;
    let mut extracted_bytes = 0u64;
    for entry in entries {
        let mut entry = entry.map_err(|e| ClipboardError::Backend(e.to_string()))?;
        entry_count = entry_count.saturating_add(1);
        if entry_count > MAX_ARCHIVE_ENTRIES {
            return Err(ClipboardError::Backend(
                "archive entry count exceeds limit".to_string(),
            ));
        }
        let path = entry
            .path()
            .map_err(|e| ClipboardError::Backend(e.to_string()))?
            .into_owned();
        validate_archive_path(&path)?;
        let path_text = path.to_str().ok_or_else(|| {
            ClipboardError::Backend("archive path is not valid UTF-8".to_string())
        })?;
        let normalized_path = path_text.replace('\\', "/").to_lowercase();
        if !observed_paths.insert(normalized_path) {
            return Err(ClipboardError::Backend(
                "archive contains a duplicate path".to_string(),
            ));
        }
        let top_level = path
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str())
            .ok_or_else(|| {
                ClipboardError::Backend("archive top-level path is invalid".to_string())
            })?;
        if !expected_top_level.contains(top_level) {
            return Err(ClipboardError::Backend(
                "archive contains an undeclared top-level entry".to_string(),
            ));
        }
        observed_top_level.insert(top_level.to_string());
        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() && !entry_type.is_dir() {
            return Err(ClipboardError::Backend(
                "archive links and special files are not allowed".to_string(),
            ));
        }
        extracted_bytes = extracted_bytes.saturating_add(entry.size());
        if extracted_bytes > max_extracted_bytes {
            return Err(ClipboardError::TooLarge {
                size_bytes: extracted_bytes,
                limit_bytes: max_extracted_bytes,
            });
        }
        let unpacked = entry
            .unpack_in(destination)
            .map_err(|e| ClipboardError::Backend(e.to_string()))?;
        if !unpacked {
            return Err(ClipboardError::Backend(
                "archive entry escaped receive directory".to_string(),
            ));
        }
        set_extracted_permissions(&destination.join(&path), entry_type.is_dir())?;
    }
    if observed_top_level.len() != expected_top_level.len()
        || !expected_top_level
            .iter()
            .all(|name| observed_top_level.contains(*name))
    {
        return Err(ClipboardError::Backend(
            "archive is missing a declared top-level entry".to_string(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn set_extracted_permissions(path: &Path, is_directory: bool) -> Result<(), ClipboardError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if is_directory { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| ClipboardError::Backend(error.to_string()))
}

#[cfg(not(unix))]
fn set_extracted_permissions(_path: &Path, _is_directory: bool) -> Result<(), ClipboardError> {
    Ok(())
}

fn write_file_list(paths: &[PathBuf]) -> Result<(), ClipboardError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| ClipboardError::Backend(error.to_string()))?;
    clipboard
        .set()
        .file_list(paths)
        .map_err(|error| ClipboardError::Backend(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("lan-clipboard-file-tests-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn archive_with_file(path: &str, bytes: &[u8]) -> Vec<u8> {
        let mut archive = Vec::new();
        {
            let mut builder = Builder::new(&mut archive);
            let mut header = Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o7777);
            header.set_cksum();
            builder
                .append_data(&mut header, path, Cursor::new(bytes))
                .expect("append test file");
            builder.finish().expect("finish test archive");
        }
        archive
    }

    #[test]
    fn archive_size_estimate_includes_gnu_long_name_records() {
        let directory = TestDirectory::new();
        let long_name = format!("{}.txt", "a".repeat(120));
        let file = directory.0.join(long_name);
        fs::write(&file, b"content").expect("write long-name fixture");

        let expected =
            estimate_file_bundle_archive_size(std::slice::from_ref(&file)).expect("estimate");
        let mut encoded = Vec::new();
        stream_file_bundle_archive(&[file], &mut encoded).expect("encode archive");

        assert_eq!(encoded.len() as u64, expected);
    }

    #[test]
    fn streamed_archive_uses_minimal_portable_metadata() {
        let directory = TestDirectory::new();
        let payload_dir = directory.0.join("payload");
        let payload_file = payload_dir.join("item.txt");
        fs::create_dir(&payload_dir).expect("create payload directory");
        fs::write(&payload_file, b"content").expect("write payload file");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&payload_file, fs::Permissions::from_mode(0o777))
                .expect("set non-portable source mode");
        }

        let mut encoded = Vec::new();
        stream_file_bundle_archive(&[payload_dir], &mut encoded).expect("encode archive");
        let mut archive = Archive::new(Cursor::new(encoded));
        let mut entries = archive.entries().expect("archive entries");

        let directory_entry = entries.next().expect("directory entry").expect("directory");
        assert!(directory_entry.header().entry_type().is_dir());
        assert_eq!(directory_entry.header().mode().unwrap(), 0o755);
        assert_eq!(directory_entry.header().uid().unwrap(), 0);
        assert_eq!(directory_entry.header().gid().unwrap(), 0);
        assert_eq!(directory_entry.header().mtime().unwrap(), 0);

        let file_entry = entries.next().expect("file entry").expect("file");
        assert!(file_entry.header().entry_type().is_file());
        assert_eq!(file_entry.header().mode().unwrap(), 0o644);
        assert_eq!(file_entry.header().uid().unwrap(), 0);
        assert_eq!(file_entry.header().gid().unwrap(), 0);
        assert_eq!(file_entry.header().mtime().unwrap(), 0);
        assert!(entries.next().is_none());
    }

    #[test]
    fn streamed_archive_reports_the_exact_logical_file_tree_hash() {
        let directory = TestDirectory::new();
        let payload_dir = directory.0.join("payload");
        let payload_file = payload_dir.join("item.txt");
        fs::create_dir(&payload_dir).expect("create payload directory");
        fs::write(&payload_file, b"original").expect("write payload file");

        let captured_hash =
            hash_file_list(std::slice::from_ref(&payload_dir)).expect("fingerprint");
        let mut original_archive = Vec::new();
        let streamed_hash =
            stream_file_bundle_archive(std::slice::from_ref(&payload_dir), &mut original_archive)
                .expect("stream original archive");
        assert_eq!(streamed_hash, captured_hash);

        fs::write(&payload_file, b"modified").expect("modify payload file");
        let mut changed_archive = Vec::new();
        let changed_stream_hash =
            stream_file_bundle_archive(std::slice::from_ref(&payload_dir), &mut changed_archive)
                .expect("stream changed archive");
        assert_ne!(changed_stream_hash, captured_hash);
    }

    #[test]
    fn unpack_requires_declared_top_level_entries_and_cleans_staging() {
        let unique_name = format!("unexpected-{}.txt", uuid::Uuid::new_v4());
        let archive = archive_with_file(&unique_name, b"secret");
        let error = unpack_file_bundle_archive_reader(
            Cursor::new(archive),
            &["declared.txt".to_string()],
            1024,
        )
        .expect_err("undeclared top-level entry must fail");

        assert!(error.to_string().contains("undeclared"));
        let root = super::super::path_policy::internal_clipboard_root().expect("receive root");
        let leaked = fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|entry| entry.path().join(&unique_name).exists());
        assert!(
            !leaked,
            "failed extraction must remove its staging directory"
        );
    }

    #[test]
    fn unpack_rejects_symlinks_and_oversized_entries() {
        let mut link_archive = Vec::new();
        {
            let mut builder = Builder::new(&mut link_archive);
            let mut header = Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_cksum();
            builder
                .append_link(&mut header, "link", "target")
                .expect("append symlink");
            builder.finish().expect("finish symlink archive");
        }
        assert!(unpack_file_bundle_archive_reader(
            Cursor::new(link_archive),
            &["link".to_string()],
            1024,
        )
        .is_err());

        let archive = archive_with_file("large.bin", &[0u8; 32]);
        assert!(matches!(
            unpack_file_bundle_archive_reader(Cursor::new(archive), &["large.bin".to_string()], 16,),
            Err(ClipboardError::TooLarge { .. })
        ));
    }

    #[test]
    fn extracted_files_receive_private_permissions() {
        let archive = archive_with_file("private.txt", b"private");
        let bundle = unpack_file_bundle_archive_reader(
            Cursor::new(archive),
            &["private.txt".to_string()],
            1024,
        )
        .expect("unpack safe archive");
        assert_eq!(
            fs::read(bundle.path().join("private.txt")).unwrap(),
            b"private"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(bundle.path().join("private.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
