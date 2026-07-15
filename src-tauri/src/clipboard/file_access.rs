use super::ClipboardError;
use std::fs::{self, File, Metadata, OpenOptions};
use std::path::Path;

pub(super) struct VerifiedPath {
    handle: File,
    metadata: Metadata,
    kind: VerifiedPathKind,
    identity: FileIdentity,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity(u64, u64);

#[derive(Clone, Copy, PartialEq, Eq)]
enum VerifiedPathKind {
    File,
    Directory,
}

impl VerifiedPath {
    pub(super) fn open(path: &Path) -> Result<Self, ClipboardError> {
        let observed = fs::symlink_metadata(path).map_err(backend)?;
        let kind = verified_kind(&observed)?;
        let handle = open_no_follow(path, kind)?;
        let metadata = handle.metadata().map_err(backend)?;
        if verified_kind(&metadata)? != kind {
            return Err(changed_path_error(path));
        }
        let identity = file_identity(&handle, &metadata)?;

        let verified = Self {
            handle,
            metadata,
            kind,
            identity,
        };
        verified.verify_still_at(path)?;
        Ok(verified)
    }

    pub(super) fn is_file(&self) -> bool {
        self.kind == VerifiedPathKind::File
    }

    pub(super) fn is_dir(&self) -> bool {
        self.kind == VerifiedPathKind::Directory
    }

    pub(super) fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    pub(super) fn file_mut(&mut self) -> Result<&mut File, ClipboardError> {
        if !self.is_file() {
            return Err(ClipboardError::Backend(
                "verified path is not a regular file".to_string(),
            ));
        }
        Ok(&mut self.handle)
    }

    pub(super) fn verify_still_at(&self, path: &Path) -> Result<(), ClipboardError> {
        let (current_handle, current_metadata) = open_current(path, self.kind)?;
        if file_identity(&current_handle, &current_metadata)? != self.identity {
            return Err(changed_path_error(path));
        }
        Ok(())
    }

    pub(super) fn verify_unchanged_at(&self, path: &Path) -> Result<(), ClipboardError> {
        let handle_metadata = self.handle.metadata().map_err(backend)?;
        let (current_handle, current_metadata) = open_current(path, self.kind)?;
        if verified_kind(&handle_metadata)? != self.kind
            || file_identity(&self.handle, &handle_metadata)? != self.identity
            || file_identity(&current_handle, &current_metadata)? != self.identity
            || !same_observed_state(&handle_metadata, &self.metadata)
            || !same_observed_state(&current_metadata, &self.metadata)
        {
            return Err(changed_path_error(path));
        }
        Ok(())
    }
}

fn open_current(
    path: &Path,
    expected_kind: VerifiedPathKind,
) -> Result<(File, Metadata), ClipboardError> {
    let observed = fs::symlink_metadata(path).map_err(backend)?;
    if verified_kind(&observed)? != expected_kind {
        return Err(changed_path_error(path));
    }
    let handle = open_no_follow(path, expected_kind)?;
    let metadata = handle.metadata().map_err(backend)?;
    if verified_kind(&metadata)? != expected_kind {
        return Err(changed_path_error(path));
    }
    Ok((handle, metadata))
}

fn verified_kind(metadata: &Metadata) -> Result<VerifiedPathKind, ClipboardError> {
    if metadata.file_type().is_symlink() || is_windows_reparse_point(metadata) {
        return Err(ClipboardError::Backend(
            "symbolic links and reparse points are not transferred".to_string(),
        ));
    }
    if metadata.is_file() {
        return Ok(VerifiedPathKind::File);
    }
    if metadata.is_dir() {
        return Ok(VerifiedPathKind::Directory);
    }
    Err(ClipboardError::Backend(
        "special filesystem entries are not transferred".to_string(),
    ))
}

fn open_no_follow(path: &Path, kind: VerifiedPathKind) -> Result<File, ClipboardError> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options, kind);
    options.open(path).map_err(backend)
}

#[cfg(unix)]
fn configure_no_follow(options: &mut OpenOptions, _kind: VerifiedPathKind) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(windows)]
fn configure_no_follow(options: &mut OpenOptions, kind: VerifiedPathKind) {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let directory_flag = if kind == VerifiedPathKind::Directory {
        FILE_FLAG_BACKUP_SEMANTICS
    } else {
        0
    };
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | directory_flag);
}

#[cfg(not(any(unix, windows)))]
fn configure_no_follow(_options: &mut OpenOptions, _kind: VerifiedPathKind) {}

#[cfg(unix)]
fn file_identity(_handle: &File, metadata: &Metadata) -> Result<FileIdentity, ClipboardError> {
    use std::os::unix::fs::MetadataExt;
    Ok(FileIdentity(metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn same_observed_state(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(windows)]
fn file_identity(handle: &File, _metadata: &Metadata) -> Result<FileIdentity, ClipboardError> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let result =
        unsafe { GetFileInformationByHandle(handle.as_raw_handle(), information.as_mut_ptr()) };
    if result == 0 {
        return Err(backend(std::io::Error::last_os_error()));
    }
    let information = unsafe { information.assume_init() };
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok(FileIdentity(
        u64::from(information.dwVolumeSerialNumber),
        index,
    ))
}

#[cfg(windows)]
fn same_observed_state(left: &Metadata, right: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    left.file_size() == right.file_size()
        && left.last_write_time() == right.last_write_time()
        && left.file_attributes() == right.file_attributes()
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_handle: &File, metadata: &Metadata) -> Result<FileIdentity, ClipboardError> {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default();
    Ok(FileIdentity(metadata.len(), modified))
}

#[cfg(not(any(unix, windows)))]
fn same_observed_state(left: &Metadata, right: &Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_windows_reparse_point(_metadata: &Metadata) -> bool {
    false
}

fn changed_path_error(path: &Path) -> ClipboardError {
    ClipboardError::Backend(format!(
        "clipboard path changed while preparing transfer: {}",
        path.display()
    ))
}

fn backend(error: impl std::fmt::Display) -> ClipboardError {
    ClipboardError::Backend(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use uuid::Uuid;

    fn fixture_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "lan-clipboard-verified-path-{label}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ))
    }

    #[test]
    fn verified_regular_file_keeps_the_opened_identity() {
        let path = fixture_path("file");
        let mut file = File::create(&path).expect("create fixture");
        file.write_all(b"fixture").expect("write fixture");
        drop(file);

        let verified = VerifiedPath::open(&path).expect("verify fixture");
        assert!(verified.is_file());
        verified.verify_still_at(&path).expect("same path identity");
        fs::remove_file(path).expect("remove fixture");
    }

    #[cfg(unix)]
    #[test]
    fn verified_path_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let target = fixture_path("target");
        let link = fixture_path("link");
        File::create(&target).expect("create target");
        symlink(&target, &link).expect("create link");

        assert!(VerifiedPath::open(&link).is_err());
        fs::remove_file(link).expect("remove link");
        fs::remove_file(target).expect("remove target");
    }
}
