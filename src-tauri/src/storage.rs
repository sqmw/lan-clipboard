#[cfg(not(test))]
use directories::ProjectDirs;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(not(test))]
const QUALIFIER: &str = "com.gmail";
#[cfg(not(test))]
const ORGANIZATION: &str = "kingsun22515";
#[cfg(not(test))]
const APPLICATION: &str = "lanclipboard";

#[cfg(test)]
pub(crate) fn secure_runtime_dir() -> io::Result<PathBuf> {
    use std::sync::OnceLock;
    use uuid::Uuid;

    static TEST_RUNTIME_DIR: OnceLock<PathBuf> = OnceLock::new();
    let path = TEST_RUNTIME_DIR
        .get_or_init(|| {
            std::env::temp_dir().join(format!(
                "lan-clipboard-test-runtime-{}-{}",
                std::process::id(),
                Uuid::new_v4()
            ))
        })
        .clone();
    ensure_private_directory(&path)?;
    Ok(path)
}

#[cfg(not(test))]
pub(crate) fn secure_runtime_dir() -> io::Result<PathBuf> {
    let project_dirs = ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
        .ok_or_else(|| io::Error::other("user cache directory is unavailable"))?;
    let path = project_dirs.cache_dir().join("runtime");
    ensure_private_directory(&path)?;
    Ok(path)
}

pub(crate) fn secure_runtime_subdir(name: &str) -> io::Result<PathBuf> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid runtime subdirectory name",
        ));
    }
    let path = secure_runtime_dir()?.join(name);
    ensure_private_directory(&path)?;
    Ok(path)
}

pub(crate) fn reject_symlink_or_non_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            io::Error::other("runtime file path is a symlink or non-file"),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Opens an append-only runtime file without following a final symlink or
/// reparse point. The handle metadata is authoritative: a path replacement
/// after `open` can only redirect future opens, not the current write.
pub(crate) fn open_private_append_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    configure_private_file_open(&mut options);
    let file = options.open(path)?;
    validate_open_runtime_file(&file)?;
    set_private_file_permissions(&file)?;
    Ok(file)
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(
            "runtime directory path is a symlink or non-directory",
        ));
    }
    set_private_directory_permissions(path)
}

#[cfg(unix)]
fn configure_private_file_open(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
}

#[cfg(windows)]
fn configure_private_file_open(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn configure_private_file_open(_options: &mut OpenOptions) {}

fn validate_open_runtime_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || is_windows_reparse_point(&metadata) {
        return Err(io::Error::other(
            "opened runtime path is a symlink, reparse point, or non-file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(io::Error::other(
                "opened runtime file has an unexpected hard-link count",
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_private_file_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use uuid::Uuid;

    #[test]
    fn runtime_subdirectory_names_cannot_escape_the_cache_root() {
        assert!(secure_runtime_subdir("received").is_ok());
        assert!(secure_runtime_subdir("../outside").is_err());
        assert!(secure_runtime_subdir("a/b").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_subdirectory_rejects_a_preexisting_symlink() {
        use std::os::unix::fs::symlink;

        let root = secure_runtime_dir().expect("runtime root");
        let target = root.join("symlink-target");
        let link = root.join("symlink-runtime");
        fs::create_dir_all(&target).expect("target");
        let _ = fs::remove_file(&link);
        symlink(&target, &link).expect("symlink fixture");

        assert!(secure_runtime_subdir("symlink-runtime").is_err());
        fs::remove_file(link).expect("remove link fixture");
    }

    #[cfg(unix)]
    #[test]
    fn append_open_rejects_symlinks_and_hard_links() {
        use std::os::unix::fs::symlink;

        let root = secure_runtime_dir().expect("runtime root");
        let target = root.join(format!("log-target-{}", Uuid::new_v4()));
        let symlink_path = root.join(format!("log-symlink-{}", Uuid::new_v4()));
        let hardlink_path = root.join(format!("log-hardlink-{}", Uuid::new_v4()));
        fs::write(&target, b"unchanged").expect("write target");
        symlink(&target, &symlink_path).expect("create symlink");
        fs::hard_link(&target, &hardlink_path).expect("create hard link");

        assert!(open_private_append_file(&symlink_path).is_err());
        assert!(open_private_append_file(&hardlink_path).is_err());
        assert_eq!(fs::read(&target).expect("read target"), b"unchanged");

        fs::remove_file(symlink_path).expect("remove symlink");
        fs::remove_file(hardlink_path).expect("remove hard link");
        fs::remove_file(target).expect("remove target");
    }
}
