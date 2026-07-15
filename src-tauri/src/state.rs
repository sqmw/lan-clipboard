use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use crate::net::{PresenceService, SyncEngine};
use crate::settings::{Settings, SettingsNotice};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryRequest {
    pub settings_revision: u64,
    pub selected_local_ip: Option<String>,
}

#[derive(Debug, Default)]
pub struct DiscoverySingleflight {
    pub active: Mutex<Option<DiscoveryRequest>>,
}

#[derive(Clone, Default)]
pub struct AppState {
    pub settings: Arc<Mutex<Settings>>,
    /// Serializes settings persistence, runtime reconfiguration and discovery cache updates.
    pub settings_update: Arc<Mutex<()>>,
    /// Invalidates discovery work only when network or security settings change.
    pub settings_revision: Arc<AtomicU64>,
    /// Allows one discovery scan at a time so callers cannot queue blocking scans.
    pub discovery_singleflight: Arc<DiscoverySingleflight>,
    pub settings_notice: Arc<Mutex<Option<SettingsNotice>>>,
    pub sync_engine: Arc<SyncEngine>,
    pub presence_service: Arc<PresenceService>,
}
