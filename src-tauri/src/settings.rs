use rand::{rngs::OsRng, Rng};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

const SHARED_CODE_LENGTH: usize = 26;
const SHARED_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const OBVIOUS_SEQUENCE_LENGTH: usize = 8;
const MAX_ITEM_BYTES: u64 = 1_000 * 1024 * 1024;
const MIN_LISTEN_PORT: u16 = 1024;
const MAX_POLL_INTERVAL_MS: u64 = 60_000;

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("settings file not found: {0}")]
    NotFound(PathBuf),
    #[error("failed to {operation} settings file {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("settings file {path} contains invalid JSON: {source}")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("settings file {path} is invalid: {source}")]
    Validation {
        path: PathBuf,
        #[source]
        source: SettingsValidationError,
    },
    #[error("failed to serialize settings: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl SettingsError {
    pub fn is_legacy_shared_code(&self) -> bool {
        matches!(
            self,
            Self::Validation {
                source: SettingsValidationError::LegacySharedCode,
                ..
            }
        )
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SettingsValidationError {
    #[error("legacy 6-digit shared code requires re-pairing with a new 26-character code")]
    LegacySharedCode,
    #[error("shared code must contain exactly 26 Base32 characters")]
    InvalidSharedCode,
    #[error("shared code is too weak; use an application-generated random code")]
    WeakSharedCode,
    #[error("device_id must be a non-nil UUID")]
    InvalidDeviceId,
    #[error("listen_port must be between 1024 and 65535")]
    InvalidListenPort,
    #[error("max_item_bytes must be between 1 and {MAX_ITEM_BYTES}")]
    InvalidMaxItemBytes,
    #[error("poll_interval_ms must be between 1 and {MAX_POLL_INTERVAL_MS}")]
    InvalidPollInterval,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SizeLimits {
    /// 单条剪贴板内容上限（bytes），对所有可发送内容统一生效
    pub max_item_bytes: u64,
}

impl Default for SizeLimits {
    fn default() -> Self {
        Self {
            max_item_bytes: 256 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SyncConfig {
    pub device_id: String,
    #[serde(alias = "device_code")]
    pub shared_code: String,
    pub enabled: bool,
    pub local_ip: String,
    pub listen_port: u16,
    pub poll_interval_ms: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            device_id: crate::net::new_device_id(),
            shared_code: generate_pairing_key(),
            enabled: true,
            local_ip: String::new(),
            listen_port: 32910,
            poll_interval_ms: 900,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SecurityConfig {
    pub encryption_enabled: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            encryption_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Settings {
    #[serde(default)]
    pub limits: SizeLimits,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

/// User-editable settings accepted by the settings IPC boundary.
///
/// Runtime ownership fields such as the device id, ports, polling interval and
/// security switches are intentionally absent so a stale or compromised UI
/// cannot overwrite them by submitting a full `Settings` object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdate {
    pub max_item_bytes: u64,
    pub shared_code: String,
    pub local_ip: String,
    pub language: String,
    pub launch_at_login: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SettingsNoticeKind {
    LegacyPairingMigrated,
    InvalidSettingsRecovered,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingsNotice {
    pub kind: SettingsNoticeKind,
    pub backup_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UiConfig {
    /// UI language: "", "auto", "zh-CN", "en-US"
    pub language: String,
    pub launch_at_login: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            language: "auto".to_string(),
            launch_at_login: false,
        }
    }
}

impl Settings {
    pub fn sync_device_id(&self) -> String {
        self.sync.device_id.trim().to_string()
    }

    pub fn normalized(mut self) -> Result<Self, SettingsValidationError> {
        self.sync.shared_code = canonicalize_shared_code(&self.sync.shared_code);
        if self.sync.shared_code.len() == 6
            && self
                .sync
                .shared_code
                .bytes()
                .all(|value| value.is_ascii_digit())
        {
            return Err(SettingsValidationError::LegacySharedCode);
        }
        if !is_valid_shared_code(&self.sync.shared_code) {
            return Err(SettingsValidationError::InvalidSharedCode);
        }
        if !is_strong_shared_code(&self.sync.shared_code) {
            return Err(SettingsValidationError::WeakSharedCode);
        }

        self.sync.device_id = self.sync.device_id.trim().to_string();
        if self.sync.device_id.is_empty() {
            self.sync.device_id = crate::net::new_device_id();
        } else {
            let device_id = Uuid::parse_str(&self.sync.device_id)
                .map_err(|_| SettingsValidationError::InvalidDeviceId)?;
            if device_id.is_nil() {
                return Err(SettingsValidationError::InvalidDeviceId);
            }
            self.sync.device_id = device_id.hyphenated().to_string();
        }

        self.sync.local_ip = self.sync.local_ip.trim().to_string();
        if self.sync.listen_port < MIN_LISTEN_PORT {
            return Err(SettingsValidationError::InvalidListenPort);
        }
        if self.sync.poll_interval_ms == 0 || self.sync.poll_interval_ms > MAX_POLL_INTERVAL_MS {
            return Err(SettingsValidationError::InvalidPollInterval);
        }
        if self.limits.max_item_bytes == 0 || self.limits.max_item_bytes > MAX_ITEM_BYTES {
            return Err(SettingsValidationError::InvalidMaxItemBytes);
        }

        // Protocol v5 treats encryption as a mandatory security boundary.
        self.security.encryption_enabled = true;

        self.ui.language = self.ui.language.trim().to_string();
        if self.ui.language.is_empty() {
            self.ui.language = "auto".to_string();
        }
        Ok(self)
    }

    pub fn apply_update(&self, update: SettingsUpdate) -> Result<Self, SettingsValidationError> {
        let mut next = self.clone();
        next.limits.max_item_bytes = update.max_item_bytes;
        next.sync.shared_code = update.shared_code;
        next.sync.local_ip = update.local_ip;
        next.ui.language = update.language;
        next.ui.launch_at_login = update.launch_at_login;

        // Sync and encryption are protocol-v5 invariants, not user-editable
        // preferences. Enforce both before normalization and persistence.
        next.sync.enabled = true;
        next.security.encryption_enabled = true;
        next.normalized()
    }

    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        let value = Self::load_unvalidated(path)?;
        value
            .normalized()
            .map_err(|source| SettingsError::Validation {
                path: path.to_path_buf(),
                source,
            })
    }

    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        let normalized = self
            .clone()
            .normalized()
            .map_err(|source| SettingsError::Validation {
                path: path.to_path_buf(),
                source,
            })?;
        let bytes = serde_json::to_vec_pretty(&normalized)?;
        atomic_write(path, &bytes)
    }

    pub fn migrate_legacy_shared_code(path: &Path) -> Result<(Self, PathBuf), SettingsError> {
        let mut value = Self::load_unvalidated(path)?;
        let legacy_code = canonicalize_shared_code(&value.sync.shared_code);
        if legacy_code.len() != 6 || !legacy_code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(SettingsError::Validation {
                path: path.to_path_buf(),
                source: SettingsValidationError::InvalidSharedCode,
            });
        }

        let original = read_file(path)?;
        let backup_path = next_legacy_backup_path(path);
        atomic_write(&backup_path, &original)?;

        value.sync.shared_code = generate_pairing_key();
        let value = value
            .normalized()
            .map_err(|source| SettingsError::Validation {
                path: path.to_path_buf(),
                source,
            })?;
        value.save(path)?;
        Ok((value, backup_path))
    }

    pub fn recover_invalid(path: &Path) -> Result<(Self, PathBuf), SettingsError> {
        let original = read_file(path)?;
        let backup_path = next_invalid_backup_path(path);
        atomic_write(&backup_path, &original)?;

        let value = Self::default()
            .normalized()
            .map_err(|source| SettingsError::Validation {
                path: path.to_path_buf(),
                source,
            })?;
        value.save(path)?;
        Ok((value, backup_path))
    }

    fn load_unvalidated(path: &Path) -> Result<Self, SettingsError> {
        let bytes = read_file(path)?;
        serde_json::from_slice::<Self>(&bytes).map_err(|source| SettingsError::Corrupt {
            path: path.to_path_buf(),
            source,
        })
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>, SettingsError> {
    std::fs::read(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            SettingsError::NotFound(path.to_path_buf())
        } else {
            SettingsError::Io {
                operation: "read",
                path: path.to_path_buf(),
                source,
            }
        }
    })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SettingsError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| SettingsError::Io {
        operation: "create settings directory for",
        path: path.to_path_buf(),
        source,
    })?;

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("settings.json");
    let temp_path = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let mut temp = PendingTempFile::new(temp_path.clone());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp_path)
        .map_err(|source| SettingsError::Io {
            operation: "create temporary",
            path: temp_path.clone(),
            source,
        })?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|source| SettingsError::Io {
            operation: "write temporary",
            path: temp_path.clone(),
            source,
        })?;
    drop(file);

    replace_file(&temp_path, path).map_err(|source| SettingsError::Io {
        operation: "atomically replace",
        path: path.to_path_buf(),
        source,
    })?;
    temp.committed = true;
    sync_parent_directory(parent);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(target_os = "windows")]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) {
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) {}

fn next_legacy_backup_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let preferred = parent.join("settings.legacy-v3.json");
    if !preferred.exists() {
        return preferred;
    }
    parent.join(format!("settings.legacy-v3.{}.json", Uuid::new_v4()))
}

fn next_invalid_backup_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let preferred = parent.join("settings.invalid-v4.json");
    if !preferred.exists() {
        return preferred;
    }
    parent.join(format!("settings.invalid-v4.{}.json", Uuid::new_v4()))
}

struct PendingTempFile {
    path: PathBuf,
    committed: bool,
}

impl PendingTempFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }
}

impl Drop for PendingTempFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn is_valid_shared_code(value: &str) -> bool {
    value.len() == SHARED_CODE_LENGTH
        && value
            .bytes()
            .all(|byte| SHARED_CODE_ALPHABET.contains(&byte))
}

fn canonicalize_shared_code(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '-')
        .collect::<String>()
        .to_ascii_uppercase()
}

fn is_strong_shared_code(value: &str) -> bool {
    // This only rejects conspicuous user-chosen patterns. It is not an entropy
    // estimate; generated keys get their unpredictability from the CSPRNG.
    let bytes = value.as_bytes();
    let mut seen = [false; 256];
    let unique_count = bytes
        .iter()
        .filter(|byte| {
            let index = **byte as usize;
            let is_new = !seen[index];
            seen[index] = true;
            is_new
        })
        .count();
    if unique_count < 10 {
        return false;
    }

    let shortest_period = (1..=bytes.len())
        .find(|period| (*period..bytes.len()).all(|index| bytes[index] == bytes[index % *period]))
        .unwrap_or(bytes.len());
    shortest_period == bytes.len() && !has_obvious_alphabet_sequence(bytes)
}

fn has_obvious_alphabet_sequence(bytes: &[u8]) -> bool {
    if bytes.len() < OBVIOUS_SEQUENCE_LENGTH {
        return false;
    }
    let indices = bytes
        .iter()
        .filter_map(|byte| {
            SHARED_CODE_ALPHABET
                .iter()
                .position(|candidate| candidate == byte)
        })
        .collect::<Vec<_>>();
    if indices.len() != bytes.len() {
        return true;
    }

    for window in indices.windows(OBVIOUS_SEQUENCE_LENGTH) {
        let ascending = window
            .windows(2)
            .all(|pair| pair[1] == (pair[0] + 1) % SHARED_CODE_ALPHABET.len());
        let descending = window.windows(2).all(|pair| {
            pair[1] == (pair[0] + SHARED_CODE_ALPHABET.len() - 1) % SHARED_CODE_ALPHABET.len()
        });
        if ascending || descending {
            return true;
        }
    }
    false
}

pub fn generate_pairing_key() -> String {
    let mut rng = OsRng;
    loop {
        let candidate = (0..SHARED_CODE_LENGTH)
            .map(|_| {
                let index = rng.gen_range(0..SHARED_CODE_ALPHABET.len());
                SHARED_CODE_ALPHABET[index] as char
            })
            .collect::<String>();
        if is_strong_shared_code(&candidate) {
            return candidate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("lan-clipboard-settings-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn default_settings_use_high_entropy_shared_code() {
        let settings = Settings::default().normalized().expect("valid defaults");
        assert_eq!(settings.sync.shared_code.len(), SHARED_CODE_LENGTH);
        assert!(is_valid_shared_code(&settings.sync.shared_code));
        assert!(is_strong_shared_code(&settings.sync.shared_code));
        assert!(Uuid::parse_str(&settings.sync.device_id).is_ok());
    }

    #[test]
    fn generated_pairing_keys_are_valid_and_not_reused() {
        let keys = (0..16)
            .map(|_| generate_pairing_key())
            .collect::<HashSet<_>>();
        assert_eq!(keys.len(), 16);
        assert!(keys
            .iter()
            .all(|key| is_valid_shared_code(key) && is_strong_shared_code(key)));
    }

    #[test]
    fn weak_shared_codes_are_rejected_after_format_validation() {
        for weak_code in ["AAAAAAAAAAAAAAAAAAAAAAAAAA", "ABCDEFGHJKLMABCDEFGHJKLMAB"] {
            let mut settings = Settings::default();
            settings.sync.shared_code = weak_code.to_string();
            assert_eq!(
                settings.normalized().expect_err("weak code must fail"),
                SettingsValidationError::WeakSharedCode
            );
        }

        for weak_code in ["ABCDEFGHJKLMNABCDEFGHJKLMN", "ABCDEFGHJKLMNPQRSTUVWXYZ23"] {
            let mut settings = Settings::default();
            settings.sync.shared_code = weak_code.to_string();
            assert_eq!(
                settings
                    .normalized()
                    .expect_err("obvious pattern must fail"),
                SettingsValidationError::WeakSharedCode
            );
        }
    }

    #[test]
    fn normalization_trims_and_canonicalizes_identifiers() {
        let mut settings = Settings::default();
        let lowercase_code = settings.sync.shared_code.to_lowercase();
        settings.sync.shared_code =
            format!("  {}-{}  ", &lowercase_code[..13], &lowercase_code[13..]);
        settings.sync.device_id = settings.sync.device_id.to_uppercase();
        settings.sync.local_ip = " 192.168.1.20 ".to_string();

        let normalized = settings.normalized().expect("normalize settings");
        assert!(is_valid_shared_code(&normalized.sync.shared_code));
        assert_eq!(normalized.sync.local_ip, "192.168.1.20");
        assert_eq!(
            normalized.sync.device_id,
            Uuid::parse_str(&normalized.sync.device_id)
                .expect("canonical UUID")
                .hyphenated()
                .to_string()
        );
    }

    #[test]
    fn legacy_shared_code_requires_migration() {
        let mut settings = Settings::default();
        settings.sync.shared_code = "123456".to_string();
        assert_eq!(
            settings.normalized().expect_err("legacy code must fail"),
            SettingsValidationError::LegacySharedCode
        );
    }

    #[test]
    fn invalid_limits_and_ports_are_rejected() {
        let mut invalid_port = Settings::default();
        invalid_port.sync.listen_port = 0;
        assert_eq!(
            invalid_port.normalized().expect_err("port must fail"),
            SettingsValidationError::InvalidListenPort
        );

        let mut invalid_size = Settings::default();
        invalid_size.limits.max_item_bytes = MAX_ITEM_BYTES + 1;
        assert_eq!(
            invalid_size.normalized().expect_err("size must fail"),
            SettingsValidationError::InvalidMaxItemBytes
        );

        let mut empty_size = Settings::default();
        empty_size.limits.max_item_bytes = 0;
        assert_eq!(
            empty_size.normalized().expect_err("zero size must fail"),
            SettingsValidationError::InvalidMaxItemBytes
        );
    }

    #[test]
    fn settings_update_preserves_owned_fields_and_exact_byte_limit() {
        let mut current = Settings::default();
        current.sync.enabled = false;
        current.sync.listen_port = 43123;
        current.sync.poll_interval_ms = 1_234;
        current.security.encryption_enabled = false;
        let device_id = current.sync.device_id.clone();
        let shared_code = current.sync.shared_code.clone();

        let next = current
            .apply_update(SettingsUpdate {
                max_item_bytes: 256 * 1024,
                shared_code,
                local_ip: " 192.168.50.4 ".to_string(),
                language: " zh-CN ".to_string(),
                launch_at_login: true,
            })
            .expect("apply settings update");

        assert_eq!(next.limits.max_item_bytes, 256 * 1024);
        assert_eq!(next.sync.device_id, device_id);
        assert_eq!(next.sync.listen_port, 43123);
        assert_eq!(next.sync.poll_interval_ms, 1_234);
        assert_eq!(next.sync.local_ip, "192.168.50.4");
        assert!(next.sync.enabled);
        assert!(next.security.encryption_enabled);
        assert_eq!(next.ui.language, "zh-CN");
        assert!(next.ui.launch_at_login);
    }

    #[test]
    fn load_distinguishes_missing_corrupt_and_io_errors() {
        let directory = TestDirectory::new();
        let missing = directory.path().join("missing.json");
        assert!(matches!(
            Settings::load(&missing),
            Err(SettingsError::NotFound(_))
        ));

        let corrupt = directory.path().join("corrupt.json");
        std::fs::write(&corrupt, b"{not-json").expect("write corrupt fixture");
        assert!(matches!(
            Settings::load(&corrupt),
            Err(SettingsError::Corrupt { .. })
        ));

        assert!(matches!(
            Settings::load(directory.path()),
            Err(SettingsError::Io { .. })
        ));
    }

    #[test]
    fn save_is_atomic_and_round_trips_normalized_settings() {
        let directory = TestDirectory::new();
        let path = directory.path().join("settings.json");
        let settings = Settings::default().normalized().expect("valid settings");
        settings.save(&path).expect("save settings");

        assert_eq!(Settings::load(&path).expect("load settings"), settings);
        let temporary_files = std::fs::read_dir(directory.path())
            .expect("read test directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temporary_files, 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("read settings metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn legacy_migration_preserves_exact_backup_and_writes_new_code() {
        let directory = TestDirectory::new();
        let path = directory.path().join("settings.json");
        let mut legacy = Settings::default();
        legacy.sync.shared_code = "123456".to_string();
        let original = serde_json::to_vec_pretty(&legacy).expect("serialize legacy settings");
        std::fs::write(&path, &original).expect("write legacy settings");

        let error = Settings::load(&path).expect_err("legacy load must request migration");
        assert!(error.is_legacy_shared_code());
        let (migrated, backup) =
            Settings::migrate_legacy_shared_code(&path).expect("migrate legacy settings");

        assert_eq!(std::fs::read(backup).expect("read backup"), original);
        assert!(is_valid_shared_code(&migrated.sync.shared_code));
        assert_ne!(migrated.sync.shared_code, "123456");
        assert_eq!(
            Settings::load(&path)
                .expect("load migrated settings")
                .sync
                .shared_code,
            migrated.sync.shared_code
        );
    }

    #[test]
    fn legacy_migration_never_overwrites_an_existing_backup() {
        let directory = TestDirectory::new();
        let path = directory.path().join("settings.json");
        let preferred_backup = directory.path().join("settings.legacy-v3.json");
        let mut legacy = Settings::default();
        legacy.sync.shared_code = "123456".to_string();
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&legacy).expect("serialize legacy settings"),
        )
        .expect("write legacy settings");
        std::fs::write(&preferred_backup, b"existing-backup").expect("write existing backup");

        let (_, backup) =
            Settings::migrate_legacy_shared_code(&path).expect("migrate legacy settings");

        assert_ne!(backup, preferred_backup);
        assert_eq!(
            std::fs::read(preferred_backup).expect("read preferred backup"),
            b"existing-backup"
        );
        assert!(backup.exists());
    }

    #[test]
    fn legacy_migration_uses_the_same_canonicalization_as_validation() {
        let directory = TestDirectory::new();
        let path = directory.path().join("settings.json");
        let mut legacy = Settings::default();
        legacy.sync.shared_code = " 123-456 ".to_string();
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&legacy).expect("serialize formatted legacy settings"),
        )
        .expect("write formatted legacy settings");

        assert!(Settings::load(&path)
            .expect_err("formatted legacy key must request migration")
            .is_legacy_shared_code());
        let (migrated, _) =
            Settings::migrate_legacy_shared_code(&path).expect("migrate formatted legacy key");
        assert!(is_valid_shared_code(&migrated.sync.shared_code));
        assert!(is_strong_shared_code(&migrated.sync.shared_code));
    }
}
