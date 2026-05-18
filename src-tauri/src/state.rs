use std::sync::{Arc, Mutex};

use crate::net::{PresenceService, SyncEngine};
use crate::settings::Settings;

#[derive(Default)]
pub struct AppState {
    pub settings: Mutex<Settings>,
    pub sync_engine: Arc<SyncEngine>,
    pub presence_service: Arc<PresenceService>,
}
