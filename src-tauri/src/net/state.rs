use super::dedupe::ObservedClipboard;
use super::logs::RuntimeLog;
use super::marker::ItemMarker;
use super::queue::QueueEntry;
use super::transfers::TransferProgress;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeStatus {
    pub running: bool,
    pub device_id: String,
    pub device_name: String,
    pub local_ip: Option<String>,
    pub shared_code: String,
    pub last_error: Option<String>,
    pub recent_log_count: usize,
    pub peer_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkInterfaceOption {
    pub name: String,
    pub ip: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub device_id: String,
    pub device_name: String,
    pub addr: String,
    pub port: u16,
}

#[derive(Debug)]
pub(super) struct RuntimeInner {
    pub(super) running: AtomicBool,
    pub(super) stop_flag: AtomicBool,
    pub(super) worker: Mutex<Option<JoinHandle<()>>>,
    pub(super) last_error: Mutex<Option<String>>,
    pub(super) active_local_ip: Mutex<Option<String>>,
    pub(super) outbound_queue: Mutex<VecDeque<QueueEntry>>,
    pub(super) inbound_queue: Mutex<VecDeque<QueueEntry>>,
    pub(super) latest_item: Mutex<Option<ItemMarker>>,
    pub(super) shared_content_fingerprint: Mutex<Option<String>>,
    pub(super) inflight_content_fingerprints: Mutex<HashSet<String>>,
    pub(super) last_local_observed: Mutex<Option<ObservedClipboard>>,
    pub(super) ignored_local_hashes: Mutex<HashMap<String, Instant>>,
    pub(super) recent_event_ids: Mutex<HashMap<String, Instant>>,
    pub(super) logs: Mutex<Vec<RuntimeLog>>,
    pub(super) transfers: Mutex<Vec<TransferProgress>>,
    pub(super) discovered_devices: Mutex<Vec<DiscoveredDevice>>,
    pub(super) discovered_seen_at: Mutex<HashMap<String, Instant>>,
    pub(super) known_members: Mutex<HashSet<String>>,
}

impl Default for RuntimeInner {
    fn default() -> Self {
        Self {
            running: AtomicBool::new(false),
            stop_flag: AtomicBool::new(false),
            worker: Mutex::new(None),
            last_error: Mutex::new(None),
            active_local_ip: Mutex::new(None),
            outbound_queue: Mutex::new(VecDeque::new()),
            inbound_queue: Mutex::new(VecDeque::new()),
            latest_item: Mutex::new(None),
            shared_content_fingerprint: Mutex::new(None),
            inflight_content_fingerprints: Mutex::new(HashSet::new()),
            last_local_observed: Mutex::new(None),
            ignored_local_hashes: Mutex::new(HashMap::new()),
            recent_event_ids: Mutex::new(HashMap::new()),
            logs: Mutex::new(Vec::new()),
            transfers: Mutex::new(Vec::new()),
            discovered_devices: Mutex::new(Vec::new()),
            discovered_seen_at: Mutex::new(HashMap::new()),
            known_members: Mutex::new(HashSet::new()),
        }
    }
}

#[derive(Debug)]
pub(super) struct PresenceInner {
    pub(super) stop_flag: AtomicBool,
    pub(super) worker: Mutex<Option<JoinHandle<()>>>,
    pub(super) signature: Mutex<Option<String>>,
}

impl Default for PresenceInner {
    fn default() -> Self {
        Self {
            stop_flag: AtomicBool::new(false),
            worker: Mutex::new(None),
            signature: Mutex::new(None),
        }
    }
}
