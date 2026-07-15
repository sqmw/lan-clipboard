use super::types::ClipboardError;
use std::cmp::Reverse;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

pub(super) const MAX_ARCHIVE_ENTRIES: usize = 20_000;
pub(super) const MAX_ARCHIVE_DEPTH: usize = 32;
const MAX_ARCHIVE_PATH_BYTES: usize = 4_096;
const MAX_TOP_LEVEL_NAMES: usize = 256;
const MAX_TOP_LEVEL_NAME_BYTES: usize = 255;
const MAX_RETAINED_BUNDLES: usize = 8;
const BUNDLE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

fn active_bundles() -> &'static Mutex<HashSet<PathBuf>> {
    static ACTIVE_BUNDLES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    ACTIVE_BUNDLES.get_or_init(|| Mutex::new(HashSet::new()))
}

#[derive(Debug)]
pub(crate) struct ReceivedBundle {
    path: PathBuf,
    remove_on_drop: bool,
}

impl ReceivedBundle {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn into_path(mut self) -> PathBuf {
        self.remove_on_drop = false;
        std::mem::take(&mut self.path)
    }
}

impl Drop for ReceivedBundle {
    fn drop(&mut self) {
        if self.remove_on_drop {
            if let Err(error) = remove_managed_bundle_dir(&self.path) {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %error,
                    "failed to remove abandoned clipboard receive directory"
                );
            }
        }
    }
}

pub(super) fn internal_clipboard_root() -> Result<PathBuf, ClipboardError> {
    crate::storage::secure_runtime_subdir("received").map_err(backend)
}

pub(super) fn create_bundle_dir() -> Result<ReceivedBundle, ClipboardError> {
    let root = internal_clipboard_root()?;
    let mut active = active_bundles().lock().map_err(|_| {
        ClipboardError::Backend("active receive directory lock is poisoned".to_string())
    })?;
    cleanup_old_bundles(&root, &active);
    for _ in 0..8 {
        let candidate = root.join(Uuid::new_v4().to_string());
        match fs::create_dir(&candidate) {
            Ok(()) => {
                if let Err(error) = set_private_directory_permissions(&candidate) {
                    let _ = fs::remove_dir_all(&candidate);
                    return Err(error);
                }
                active.insert(candidate.clone());
                return Ok(ReceivedBundle {
                    path: candidate,
                    remove_on_drop: true,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(backend(error)),
        }
    }
    Err(ClipboardError::Backend(
        "failed to allocate unique receive directory".to_string(),
    ))
}

pub(super) fn validate_top_level_names(names: &[String]) -> Result<(), ClipboardError> {
    if names.is_empty() || names.len() > MAX_TOP_LEVEL_NAMES {
        return Err(ClipboardError::Backend(
            "invalid top-level entry count".to_string(),
        ));
    }
    let mut seen = HashSet::with_capacity(names.len());
    for name in names {
        validate_portable_component(OsStr::new(name))?;
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(ClipboardError::Backend(
                "unsafe top-level entry path".to_string(),
            ));
        }
        if !seen.insert(name.to_lowercase()) {
            return Err(ClipboardError::Backend(
                "duplicate top-level entry name".to_string(),
            ));
        }
    }
    Ok(())
}

pub(super) fn resolve_restored_paths(
    bundle_dir: &Path,
    names: &[String],
) -> Result<Vec<PathBuf>, ClipboardError> {
    validate_top_level_names(names)?;
    let canonical_root = fs::canonicalize(bundle_dir).map_err(backend)?;
    let mut paths = Vec::with_capacity(names.len());
    for name in names {
        let candidate = bundle_dir.join(name);
        let canonical = fs::canonicalize(&candidate).map_err(backend)?;
        if canonical == canonical_root || !canonical.starts_with(&canonical_root) {
            return Err(ClipboardError::Backend(
                "restored file escaped receive directory".to_string(),
            ));
        }
        paths.push(canonical);
    }
    Ok(paths)
}

pub(super) fn validate_archive_path(path: &Path) -> Result<(), ClipboardError> {
    let path_text = path.as_os_str().to_string_lossy();
    if path.as_os_str().is_empty()
        || path_text.len() > MAX_ARCHIVE_PATH_BYTES
        || path_text.contains('\\')
    {
        return Err(ClipboardError::Backend(
            "invalid archive entry path length".to_string(),
        ));
    }
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                validate_portable_component(value)?;
                depth += 1;
            }
            _ => {
                return Err(ClipboardError::Backend(
                    "archive entry path traversal rejected".to_string(),
                ))
            }
        }
    }
    if depth == 0 || depth > MAX_ARCHIVE_DEPTH {
        return Err(ClipboardError::Backend(
            "archive entry depth exceeds limit".to_string(),
        ));
    }
    Ok(())
}

fn validate_portable_component(value: &OsStr) -> Result<(), ClipboardError> {
    let name = value.to_str().ok_or_else(|| {
        ClipboardError::Backend("archive path component is not valid UTF-8".to_string())
    })?;
    if name.is_empty()
        || name.len() > MAX_TOP_LEVEL_NAME_BYTES
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|character| character.is_control() || r#"<>:"/\|?*"#.contains(character))
    {
        return Err(ClipboardError::Backend(
            "archive path contains a non-portable component".to_string(),
        ));
    }

    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(['.', ' '])
        .to_ascii_uppercase();
    let reserved_numbered_suffix = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    let is_reserved_device =
        matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || reserved_numbered_suffix;
    if is_reserved_device {
        return Err(ClipboardError::Backend(
            "archive path uses a reserved Windows device name".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn is_internal_path(path: &Path) -> bool {
    let Ok(root) = internal_clipboard_root() else {
        return false;
    };
    let Ok(canonical_root) = fs::canonicalize(root) else {
        return false;
    };
    fs::canonicalize(path)
        .map(|canonical| canonical.starts_with(canonical_root))
        .unwrap_or(false)
}

pub(super) fn retire_managed_bundle_dir(path: &Path) -> Result<(), ClipboardError> {
    let validation = inspect_managed_bundle_path(path).map(|_| ());
    let deregistration = deregister_active_bundle(path);
    validation.and(deregistration)
}

pub(super) fn remove_managed_bundle_dir(path: &Path) -> Result<(), ClipboardError> {
    let result = match inspect_managed_bundle_path(path) {
        Ok(None) => Ok(()),
        Ok(Some(metadata)) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            match fs::remove_dir_all(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(backend(error)),
            }
        }
        Ok(Some(_)) => Err(ClipboardError::Backend(
            "managed file bundle path is no longer a directory".to_string(),
        )),
        Err(error) => Err(error),
    };
    let deregistration = deregister_active_bundle(path);
    match (result, deregistration) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn inspect_managed_bundle_path(path: &Path) -> Result<Option<fs::Metadata>, ClipboardError> {
    let canonical_root = fs::canonicalize(internal_clipboard_root()?).map_err(backend)?;
    let parent = path.parent().ok_or_else(|| {
        ClipboardError::Backend("file bundle directory has no parent".to_string())
    })?;
    let canonical_parent = fs::canonicalize(parent).map_err(backend)?;
    let has_uuid_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| Uuid::parse_str(name).ok())
        .is_some();
    if canonical_parent != canonical_root || !has_uuid_name {
        return Err(ClipboardError::Backend(
            "file bundle directory is not managed by lan-clipboard".to_string(),
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ClipboardError::Backend(
            "managed file bundle path became a symbolic link".to_string(),
        )),
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(backend(error)),
    }
}

fn deregister_active_bundle(path: &Path) -> Result<(), ClipboardError> {
    active_bundles()
        .lock()
        .map_err(|_| {
            ClipboardError::Backend("active receive directory lock is poisoned".to_string())
        })?
        .remove(path);
    Ok(())
}

fn cleanup_old_bundles(root: &Path, active: &HashSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        tracing::warn!(path = %root.display(), "failed to scan clipboard receive directory");
        return;
    };
    let mut bundles = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(error) => {
                tracing::warn!(error = %error, "failed to inspect clipboard receive entry");
                None
            }
        })
        .filter_map(|entry| {
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::warn!(path = %path.display(), error = %error, "failed to read clipboard receive metadata");
                    return None;
                }
            };
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            Some((path, metadata.file_type(), modified))
        })
        .collect::<Vec<_>>();
    bundles.retain(|(path, _, _)| !active.contains(path));
    bundles.sort_by_key(|bundle| Reverse(bundle.2));
    let now = SystemTime::now();
    for (index, (path, file_type, modified)) in bundles.into_iter().enumerate() {
        let expired = now
            .duration_since(modified)
            .map(|age| age > BUNDLE_TTL)
            .unwrap_or(false);
        if index < MAX_RETAINED_BUNDLES && !expired {
            continue;
        }
        let result = if file_type.is_dir() && !file_type.is_symlink() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        if let Err(error) = result {
            tracing::warn!(path = %path.display(), error = %error, "failed to clean stale clipboard receive entry");
        }
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), ClipboardError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(backend)
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), ClipboardError> {
    Ok(())
}

fn backend(error: std::io::Error) -> ClipboardError {
    ClipboardError::Backend(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_names_reject_paths_and_case_collisions() {
        assert!(validate_top_level_names(&["../secret".to_string()]).is_err());
        assert!(validate_top_level_names(&["a\\b".to_string()]).is_err());
        assert!(validate_top_level_names(&["A.txt".to_string(), "a.txt".to_string()]).is_err());
        assert!(validate_top_level_names(&["safe.txt".to_string()]).is_ok());
        assert!(validate_top_level_names(&["report:secret.txt".to_string()]).is_err());
        assert!(validate_top_level_names(&["CON.txt".to_string()]).is_err());
        assert!(validate_top_level_names(&["CON .txt".to_string()]).is_err());
        assert!(validate_top_level_names(&["COM¹.log".to_string()]).is_err());
        assert!(validate_top_level_names(&["LPT³".to_string()]).is_err());
        assert!(validate_top_level_names(&["trailing. ".to_string()]).is_err());
    }

    #[test]
    fn archive_paths_must_be_relative_and_shallow() {
        assert!(validate_archive_path(Path::new("folder/file.txt")).is_ok());
        assert!(validate_archive_path(Path::new("../file.txt")).is_err());
        assert!(validate_archive_path(Path::new("/tmp/file.txt")).is_err());
        assert!(validate_archive_path(Path::new("folder\\file.txt")).is_err());
        assert!(validate_archive_path(Path::new("folder/NUL.txt")).is_err());
        assert!(validate_archive_path(Path::new("folder/data:stream")).is_err());
    }

    #[test]
    fn active_receive_directories_are_not_removed_by_retention_cleanup() {
        let mut bundles = Vec::new();
        for _ in 0..(MAX_RETAINED_BUNDLES + 2) {
            bundles.push(create_bundle_dir().expect("create active bundle"));
        }
        assert!(bundles.iter().all(|bundle| bundle.path().exists()));
    }

    #[test]
    fn managed_deletion_cannot_escape_receive_root() {
        let external =
            std::env::temp_dir().join(format!("lan-clipboard-external-{}", Uuid::new_v4()));
        fs::create_dir_all(&external).expect("create external fixture");

        assert!(remove_managed_bundle_dir(&external).is_err());
        assert!(external.exists());
        fs::remove_dir_all(external).expect("remove external fixture");
    }

    #[test]
    fn missing_managed_bundle_is_removed_from_active_registry() {
        let path = create_bundle_dir().expect("create bundle").into_path();
        fs::remove_dir_all(&path).expect("remove bundle outside cleanup helper");

        remove_managed_bundle_dir(&path).expect("missing bundle cleanup is idempotent");
        assert!(!active_bundles().lock().unwrap().contains(&path));
    }

    #[test]
    fn failed_managed_bundle_deletion_is_downgraded_for_retry() {
        let path = create_bundle_dir().expect("create bundle").into_path();
        fs::remove_dir_all(&path).expect("remove bundle directory");
        fs::write(&path, b"replacement").expect("replace bundle with a file");

        assert!(remove_managed_bundle_dir(&path).is_err());
        assert!(!active_bundles().lock().unwrap().contains(&path));
        fs::remove_file(path).expect("remove replacement file");
    }
}
