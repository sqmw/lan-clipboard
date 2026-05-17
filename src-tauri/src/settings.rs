use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use rand::Rng;

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SizeLimits {
    /// 单条剪贴板内容上限（bytes），对所有可发送内容统一生效
    pub max_item_bytes: u64,
}

impl Default for SizeLimits {
    fn default() -> Self {
        Self { max_item_bytes: 256 * 1024 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    pub device_id: String,
    pub device_code: String,
    pub enabled: bool,
    pub listen_port: u16,
    pub peers: Vec<String>,
    pub poll_interval_ms: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            device_id: crate::net::new_device_id(),
            device_code: generate_device_code(),
            enabled: false,
            listen_port: 32910,
            peers: Vec::new(),
            poll_interval_ms: 900,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub encryption_enabled: bool,
    pub require_pairing_code: bool,
    pub pairing_code: String,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            encryption_enabled: true,
            require_pairing_code: false,
            pairing_code: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub limits: SizeLimits,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub security: SecurityConfig,
}

impl Settings {
    pub fn sync_device_id(&self) -> String {
        if self.sync.device_id.trim().is_empty() {
            return crate::net::new_device_id();
        }
        self.sync.device_id.clone()
    }

    pub fn ensure_sync_identifiers(&mut self) -> bool {
        let mut changed = false;
        if self.sync.device_id.trim().is_empty() {
            self.sync.device_id = crate::net::new_device_id();
            changed = true;
        }
        if !is_valid_device_code(&self.sync.device_code) {
            self.sync.device_code = generate_device_code();
            changed = true;
        }
        changed
    }

    pub fn load(path: &PathBuf) -> Result<Option<Self>, SettingsError> {
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(path)?;
        let value = serde_json::from_slice::<Self>(&bytes)?;
        Ok(Some(value))
    }

    pub fn save(&self, path: &PathBuf) -> Result<(), SettingsError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

fn is_valid_device_code(value: &str) -> bool {
    value.len() == 6 && value.chars().all(|c| c.is_ascii_digit())
}

fn generate_device_code() -> String {
    let num: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{:06}", num)
}
