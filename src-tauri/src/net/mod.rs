use crate::clipboard;
use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::Settings;
use aes_gcm_siv::{Aes256GcmSiv, Nonce as AesNonce};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce as ChaChaNonce};
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
use clipboard_master::{CallbackResult, ClipboardHandler, Master};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use socket2::SockRef;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufWriter, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SERVICE_TYPE: &str = "_lan-clipboard._tcp.local.";
const LOG_LIMIT: usize = 800;
const DISCOVERY_REFRESH_MS: u64 = 3_000;
const DISCOVERY_TIMEOUT_MS: u64 = 900;
const DISCOVERY_MEMBER_TTL_MS: u64 = 30_000;
const DISCOVERY_REACHABILITY_TIMEOUT_MS: u64 = 220;
const UDP_DISCOVERY_PORT: u16 = 32911;
const UDP_ANNOUNCE_MS: u64 = 500;
const PRESENCE_RETRY_MS: u64 = 1_000;
const DISCOVERY_APP: &str = "lan-clipboard";
const APPLIED_HASH_TTL_MS: u64 = 10_000;
const RECENT_EVENT_TTL_MS: u64 = 120_000;
const QUEUE_RETRY_BASE_MS: u64 = 30;
const QUEUE_RETRY_MAX_MS: u64 = 500;
const QUEUE_MAX_RETRIES: u32 = 24;
const QUEUE_MAX_AGE_MS: u64 = 30_000;
const CONNECT_TIMEOUT_MS: u64 = 2_000;
const MIN_WRITE_TIMEOUT_MS: u64 = 8_000;
const MAX_WRITE_TIMEOUT_MS: u64 = 120_000;
const WRITE_TIMEOUT_BYTES_PER_MS: u64 = 512;
const CLIPBOARD_WATCH_INTERVAL_MS: u64 = 50;
const CLIPBOARD_WATCH_MAX_INTERVAL_MS: u64 = 500;
const MAIN_LOOP_ACTIVE_SLEEP_MS: u64 = 15;
const MAIN_LOOP_IDLE_SLEEP_MS: u64 = 80;
const QUEUE_LOOP_ACTIVE_SLEEP_MS: u64 = 5;
const QUEUE_LOOP_IDLE_SLEEP_MS: u64 = 40;
const WIRE_VERSION: u8 = 3;
const RAW_PAYLOAD_ENCRYPTED_FLAG: u8 = 1;
const MAX_WIRE_FRAME_BYTES: usize = 512 * 1024 * 1024;
const TRANSFER_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const FILE_STREAM_CHUNK_BYTES: usize = 16 * 1024 * 1024;
const FILE_STREAM_PROGRESS_EMIT_INTERVAL_MS: u64 = 250;
const TRANSFER_HISTORY_LIMIT: usize = 24;
const TRANSFER_RETENTION_MS: u64 = 15_000;
const HIGH_PRIORITY_YIELD_MS: u64 = 12;
const TCP_BUFFER_BYTES: usize = 16 * 1024 * 1024;

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
pub struct TransferProgress {
    pub id: String,
    pub direction: String,
    pub peer: String,
    pub item_kind: String,
    pub item_label: String,
    pub item_summary: String,
    pub item_id: String,
    pub transferred_bytes: u64,
    pub total_bytes: u64,
    pub percent: u8,
    pub status: String,
    pub updated_at_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkInterfaceOption {
    pub name: String,
    pub ip: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeLog {
    pub ts_ms: u64,
    pub level: String,
    pub message: String,
}

const DISCOVERED_DEVICE_LIMIT: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub device_id: String,
    pub device_name: String,
    pub addr: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireMessage {
    pub v: u8,
    pub encrypted: bool,
    pub source_device_id: String,
    pub nonce: Option<[u8; 12]>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireBody {
    ClipboardItem(ClipboardItem),
    FileStreamStart(FileStreamStart),
    FileStreamChunk { item_id: String, bytes: Vec<u8> },
    FileStreamEnd { item_id: String },
    FileStreamRawStart(FileStreamStart),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileStreamStart {
    item_id: String,
    content_hash: String,
    created_at_us: u64,
    source_device_id: String,
    size_bytes: u64,
    top_level_names: Vec<String>,
}

struct IncomingFileStream {
    meta: FileStreamStart,
    archive_path: PathBuf,
    received_bytes: u64,
    peer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiscoveryAnnouncement {
    pub v: u8,
    pub app: String,
    pub device_id: String,
    pub device_name: String,
    pub shared_code: String,
    pub tcp_port: u16,
}

#[derive(Debug)]
struct RuntimeInner {
    running: AtomicBool,
    stop_flag: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
    last_error: Mutex<Option<String>>,
    active_local_ip: Mutex<Option<String>>,
    outbound_queue: Mutex<VecDeque<QueueEntry>>,
    inbound_queue: Mutex<VecDeque<QueueEntry>>,
    latest_item: Mutex<Option<ItemMarker>>,
    shared_content_fingerprint: Mutex<Option<String>>,
    inflight_content_fingerprints: Mutex<HashSet<String>>,
    last_local_observed: Mutex<Option<ObservedClipboard>>,
    ignored_local_hashes: Mutex<HashMap<String, Instant>>,
    recent_event_ids: Mutex<HashMap<String, Instant>>,
    logs: Mutex<Vec<RuntimeLog>>,
    transfers: Mutex<Vec<TransferProgress>>,
    discovered_devices: Mutex<Vec<DiscoveredDevice>>,
    discovered_seen_at: Mutex<HashMap<String, Instant>>,
    known_members: Mutex<HashSet<String>>,
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

#[derive(Debug, Default)]
pub struct SyncEngine {
    inner: Arc<RuntimeInner>,
}

#[derive(Debug)]
struct PresenceInner {
    stop_flag: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
    signature: Mutex<Option<String>>,
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

#[derive(Debug, Default)]
pub struct PresenceService {
    inner: Arc<PresenceInner>,
}

#[derive(Debug, Clone)]
struct PresenceConfig {
    device_id: String,
    device_name: String,
    shared_code: String,
    local_ip: String,
    listen_port: u16,
}

#[derive(Debug, Clone)]
struct ItemMarker {
    id: String,
    created_at_us: u64,
    source_device_id: String,
}

#[derive(Debug, Clone)]
struct ObservedClipboard {
    content_hash: String,
    observed_at_ms: u64,
}

#[derive(Debug, Clone)]
struct QueueEntry {
    item: ClipboardItem,
    attempts: u32,
    queued_at_ms: u64,
    next_attempt_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum QueueLane {
    Realtime,
    Visual,
    Bulk,
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
struct ClipboardWatchHandler {
    runtime: Arc<RuntimeInner>,
    limits: crate::settings::SizeLimits,
    device_id: String,
    poll_interval: Duration,
}

impl SyncEngine {
    pub fn status(&self, settings: &Settings, selected_local_ip: Option<&str>) -> RuntimeStatus {
        let error = self
            .inner
            .last_error
            .lock()
            .ok()
            .and_then(|guard| (*guard).clone());
        let active_local_ip = self
            .inner
            .active_local_ip
            .lock()
            .ok()
            .and_then(|guard| (*guard).clone());
        let recent_log_count = self.inner.logs.lock().map(|guard| guard.len()).unwrap_or(0);
        let effective_local_ip =
            selected_or_active_local_ip(settings, selected_local_ip, active_local_ip.clone());
        let peer_count = self
            .devices(effective_local_ip.as_deref())
            .len()
            .saturating_add(1);
        RuntimeStatus {
            running: self.inner.running.load(Ordering::SeqCst),
            device_id: settings.sync_device_id(),
            device_name: local_device_name(&settings.sync_device_id()),
            local_ip: effective_local_ip,
            shared_code: settings.sync.shared_code.clone(),
            last_error: error,
            recent_log_count,
            peer_count,
        }
    }

    pub fn logs(&self, limit: usize) -> Vec<RuntimeLog> {
        let target = if limit == 0 {
            200
        } else {
            limit.min(LOG_LIMIT)
        };
        self.inner
            .logs
            .lock()
            .map(|guard| {
                let start = guard.len().saturating_sub(target);
                guard[start..].to_vec()
            })
            .unwrap_or_default()
    }

    pub fn clear_logs(&self) {
        if let Ok(mut guard) = self.inner.logs.lock() {
            guard.clear();
        }
    }

    pub fn transfers(&self) -> Vec<TransferProgress> {
        prune_transfers(&self.inner);
        self.inner
            .transfers
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn devices(&self, selected_local_ip: Option<&str>) -> Vec<DiscoveredDevice> {
        prune_stale_discovered_devices(&self.inner);
        let devices = self
            .inner
            .discovered_devices
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        filter_devices_for_local_ip(devices, selected_local_ip)
    }

    pub fn replace_discovered_devices(
        &self,
        selected_local_ip: Option<&str>,
        devices: Vec<DiscoveredDevice>,
        settings: &Settings,
    ) {
        replace_discovered_devices(&self.inner, selected_local_ip, devices);
        reconcile_member_state(&self.inner, settings);
    }

    pub fn start(&self, settings: Settings, device_id: String) -> anyhow::Result<()> {
        if self.inner.running.load(Ordering::SeqCst) {
            self.log(
                "INFO",
                "sync already running, restarting with latest settings",
            );
            self.stop()?;
        }

        self.inner.running.store(true, Ordering::SeqCst);
        self.inner.stop_flag.store(false, Ordering::SeqCst);
        clear_member_cache(&self.inner);
        if let Ok(mut guard) = self.inner.active_local_ip.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = self.inner.last_error.lock() {
            *guard = None;
        }
        self.log(
            "INFO",
            &format!(
                "sync starting shared_code={} port={}",
                settings.sync.shared_code, settings.sync.listen_port
            ),
        );

        let runtime = Arc::clone(&self.inner);
        let worker = std::thread::Builder::new()
            .name("lan-clipboard-sync".to_string())
            .spawn(move || run_sync_loop(runtime, settings, device_id))?;

        let mut guard = self
            .inner
            .worker
            .lock()
            .map_err(|_| anyhow::anyhow!("sync worker lock poisoned"))?;
        *guard = Some(worker);
        Ok(())
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        self.inner.stop_flag.store(true, Ordering::SeqCst);
        self.inner.running.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = self.inner.worker.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
        self.log("INFO", "sync stopped");
        Ok(())
    }

    fn log(&self, level: &str, message: &str) {
        push_log(&self.inner, level, message);
    }
}

impl PresenceService {
    pub fn ensure(&self, settings: Settings, device_id: String) -> anyhow::Result<()> {
        let config = PresenceConfig {
            device_id: device_id.clone(),
            device_name: local_device_name(&settings.sync_device_id()),
            shared_code: settings.sync.shared_code.trim().to_string(),
            local_ip: settings.sync.local_ip.trim().to_string(),
            listen_port: settings.sync.listen_port,
        };
        let signature = format!(
            "{}:{}:{}:{}:{}",
            config.device_id,
            config.device_name,
            config.shared_code,
            config.local_ip,
            config.listen_port
        );

        let same_signature = self
            .inner
            .signature
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .as_deref()
            == Some(signature.as_str());
        let worker_running = self
            .inner
            .worker
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|worker| !worker.is_finished()))
            .unwrap_or(false);
        if same_signature && worker_running {
            return Ok(());
        }

        self.stop();
        self.inner.stop_flag.store(false, Ordering::SeqCst);
        let runtime = Arc::clone(&self.inner);
        let worker = std::thread::Builder::new()
            .name("lan-clipboard-presence".to_string())
            .spawn(move || run_presence_loop(runtime, config))?;

        if let Ok(mut guard) = self.inner.worker.lock() {
            *guard = Some(worker);
        }
        if let Ok(mut guard) = self.inner.signature.lock() {
            *guard = Some(signature);
        }
        Ok(())
    }

    fn stop(&self) {
        self.inner.stop_flag.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.inner.worker.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for PresenceService {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
impl ClipboardHandler for ClipboardWatchHandler {
    fn on_clipboard_change(&mut self) -> CallbackResult {
        let _ = process_local_clipboard_observation(&self.runtime, &self.limits, &self.device_id);
        CallbackResult::Next
    }

    fn sleep_interval(&self) -> Duration {
        self.poll_interval
    }
}

fn process_local_clipboard_observation(
    runtime: &RuntimeInner,
    limits: &crate::settings::SizeLimits,
    device_id: &str,
) -> bool {
    let payload = match clipboard::read_snapshot(limits) {
        Ok(payload) => payload,
        Err(clipboard::ClipboardError::Unsupported) => return false,
        Err(error) => {
            set_error(runtime, format!("clipboard watcher read failed: {error}"));
            return false;
        }
    };

    if clipboard::is_internal_file_payload(&payload) {
        if let Ok(content_hash) = clipboard::payload_content_hash(&payload) {
            remember_local_observation(runtime, &content_hash, now_ms());
            register_ignored_local_hash(runtime, &content_hash);
        }
        push_log(
            runtime,
            "DEBUG",
            "drop local observation from internal clipboard file payload",
        );
        return false;
    }

    let item = match build_item(&payload, device_id) {
        Ok(Some(item)) => item,
        Ok(None) => return false,
        Err(error) => {
            set_error(
                runtime,
                format!("clipboard watcher build item failed: {error}"),
            );
            return false;
        }
    };

    if should_ignore_local_observation(runtime, &item) {
        return false;
    }

    push_log(
        runtime,
        "INFO",
        &format!(
            "detected local clipboard kind={} size_bytes={} item={}",
            item.payload.kind(),
            item.size_bytes,
            item.id
        ),
    );
    register_recent_event(runtime, &item.id);
    update_latest_item(runtime, &item);
    enqueue_outbound_item(runtime, item);
    true
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn seed_local_clipboard_baseline(runtime: &RuntimeInner, limits: &crate::settings::SizeLimits) {
    let Ok(payload) = clipboard::read_snapshot(limits) else {
        return;
    };
    let Ok(content_hash) = clipboard::payload_content_hash(&payload) else {
        return;
    };
    if let Ok(mut guard) = runtime.last_local_observed.lock() {
        *guard = Some(ObservedClipboard {
            content_hash,
            observed_at_ms: now_ms(),
        });
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_clipboard_watch_poll_loop(
    runtime: Arc<RuntimeInner>,
    limits: crate::settings::SizeLimits,
    device_id: String,
    poll_interval: Duration,
) {
    seed_local_clipboard_baseline(&runtime, &limits);
    let mut current_interval = poll_interval;
    while !runtime.stop_flag.load(Ordering::SeqCst) {
        let changed = process_local_clipboard_observation(&runtime, &limits, &device_id);
        if changed {
            current_interval = poll_interval;
        } else {
            let max_interval = Duration::from_millis(CLIPBOARD_WATCH_MAX_INTERVAL_MS);
            current_interval = (current_interval + current_interval).min(max_interval);
        }
        std::thread::sleep(current_interval);
    }
}

fn run_presence_loop(runtime: Arc<PresenceInner>, config: PresenceConfig) {
    while !runtime.stop_flag.load(Ordering::SeqCst) {
        let mdns = match ServiceDaemon::new() {
            Ok(value) => value,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(PRESENCE_RETRY_MS));
                continue;
            }
        };
        let service = match build_service_info(&config) {
            Ok(value) => value,
            Err(_) => {
                let _ = mdns.shutdown();
                std::thread::sleep(Duration::from_millis(PRESENCE_RETRY_MS));
                continue;
            }
        };

        if mdns.register(service).is_err() {
            let _ = mdns.shutdown();
            std::thread::sleep(Duration::from_millis(PRESENCE_RETRY_MS));
            continue;
        }

        while !runtime.stop_flag.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(400));
        }

        if let Ok(status_rx) = mdns.shutdown() {
            let _ = status_rx.recv_timeout(Duration::from_millis(300));
        }
    }
}

fn run_sync_loop(runtime: Arc<RuntimeInner>, settings: Settings, device_id: String) {
    let addr = format!("0.0.0.0:{}", settings.sync.listen_port);
    let listener = match TcpListener::bind(&addr) {
        Ok(listener) => listener,
        Err(error) => {
            set_error(&runtime, format!("listener bind failed: {error}"));
            runtime.running.store(false, Ordering::SeqCst);
            return;
        }
    };
    let _ = listener.set_nonblocking(true);
    push_log(&runtime, "INFO", &format!("listener ready at {}", addr));

    let udp_socket = match UdpSocket::bind(("0.0.0.0", UDP_DISCOVERY_PORT)) {
        Ok(socket) => {
            let _ = socket.set_nonblocking(true);
            let _ = socket.set_broadcast(true);
            push_log(
                &runtime,
                "INFO",
                &format!("udp discovery ready at 0.0.0.0:{}", UDP_DISCOVERY_PORT),
            );
            Some(socket)
        }
        Err(error) => {
            push_log(
                &runtime,
                "WARN",
                &format!("udp discovery disabled: {error}"),
            );
            None
        }
    };

    let mut last_discovery = Instant::now() - Duration::from_millis(DISCOVERY_REFRESH_MS);
    let mut last_udp_announce = Instant::now() - Duration::from_millis(UDP_ANNOUNCE_MS);
    let device_name = local_device_name(&device_id);
    let watcher_runtime = Arc::clone(&runtime);
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let watcher_stop_runtime = Arc::clone(&runtime);
    let watcher_limits = settings.limits.clone();
    let watcher_device_id = device_id.clone();
    let watcher_poll_interval = Duration::from_millis(CLIPBOARD_WATCH_INTERVAL_MS);
    let watcher = std::thread::Builder::new()
        .name("lan-clipboard-watch".to_string())
        .spawn(move || {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                run_clipboard_watch_poll_loop(
                    watcher_runtime,
                    watcher_limits,
                    watcher_device_id,
                    watcher_poll_interval,
                );
                return;
            }
            #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
            {
                let handler = ClipboardWatchHandler {
                    runtime: watcher_runtime,
                    limits: watcher_limits,
                    device_id: watcher_device_id,
                    poll_interval: watcher_poll_interval,
                };
                let Ok(mut master) = Master::new(handler) else {
                    return;
                };
                let shutdown = master.shutdown_channel();
                let _shutdown_guard = std::thread::Builder::new()
                    .name("lan-clipboard-watch-stop".to_string())
                    .spawn({
                        let runtime = watcher_stop_runtime;
                        move || {
                            while !runtime.stop_flag.load(Ordering::SeqCst) {
                                std::thread::sleep(Duration::from_millis(100));
                            }
                            shutdown.signal();
                        }
                    });
                let _ = master.run();
            }
        })
        .ok();

    let inbound_runtime = Arc::clone(&runtime);
    let inbound_settings = settings.clone();
    let inbound_worker = std::thread::Builder::new()
        .name("lan-clipboard-inbound-apply".to_string())
        .spawn(move || run_inbound_apply_loop(inbound_runtime, inbound_settings))
        .ok();

    let priority_outbound_runtime = Arc::clone(&runtime);
    let priority_outbound_settings = settings.clone();
    let priority_outbound_worker = std::thread::Builder::new()
        .name("lan-clipboard-outbound-priority".to_string())
        .spawn(move || {
            run_outbound_dispatch_loop(
                priority_outbound_runtime,
                priority_outbound_settings,
                &[QueueLane::Realtime, QueueLane::Visual],
            )
        })
        .ok();

    let bulk_outbound_runtime = Arc::clone(&runtime);
    let bulk_outbound_settings = settings.clone();
    let bulk_outbound_worker = std::thread::Builder::new()
        .name("lan-clipboard-outbound-bulk".to_string())
        .spawn(move || {
            run_outbound_dispatch_loop(
                bulk_outbound_runtime,
                bulk_outbound_settings,
                &[QueueLane::Bulk],
            )
        })
        .ok();

    while !runtime.stop_flag.load(Ordering::SeqCst) {
        if let Some(socket) = udp_socket.as_ref() {
            receive_udp_announcements(&runtime, &settings, &device_id, socket);
            if last_udp_announce.elapsed() >= Duration::from_millis(UDP_ANNOUNCE_MS) {
                send_udp_announcement(socket, &settings, &device_id, &device_name);
                last_udp_announce = Instant::now();
            }
        }

        if last_discovery.elapsed() >= Duration::from_millis(DISCOVERY_REFRESH_MS) {
            if !has_active_transfers(&runtime) {
                refresh_discovered_devices(
                    &runtime,
                    &settings,
                    &device_id,
                    Some(settings.sync.local_ip.as_str()),
                    DISCOVERY_TIMEOUT_MS,
                );
            }
            last_discovery = Instant::now();
        }

        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                tune_stream_for_receive(&stream, MAX_WIRE_FRAME_BYTES as u64);
                spawn_incoming_connection_worker(
                    Arc::clone(&runtime),
                    settings.clone(),
                    device_id.clone(),
                    stream,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => set_error(&runtime, format!("listener accept failed: {error}")),
        }

        prune_ignored_local_hashes(&runtime);
        prune_recent_event_ids(&runtime);
        let queue_busy = runtime
            .outbound_queue
            .lock()
            .map(|guard| !guard.is_empty())
            .unwrap_or(true)
            || runtime
                .inbound_queue
                .lock()
                .map(|guard| !guard.is_empty())
                .unwrap_or(true);
        let sleep_ms = if has_active_transfers(&runtime) || queue_busy {
            MAIN_LOOP_ACTIVE_SLEEP_MS
        } else {
            MAIN_LOOP_IDLE_SLEEP_MS
        };
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }

    if let Some(handle) = watcher {
        let _ = handle.join();
    }
    if let Some(handle) = inbound_worker {
        let _ = handle.join();
    }
    if let Some(handle) = priority_outbound_worker {
        let _ = handle.join();
    }
    if let Some(handle) = bulk_outbound_worker {
        let _ = handle.join();
    }
    runtime.running.store(false, Ordering::SeqCst);
}

fn run_inbound_apply_loop(runtime: Arc<RuntimeInner>, settings: Settings) {
    while !runtime.stop_flag.load(Ordering::SeqCst) {
        let did_work = process_inbound_queue(&runtime, &settings);
        let sleep_ms = if did_work {
            QUEUE_LOOP_ACTIVE_SLEEP_MS
        } else {
            QUEUE_LOOP_IDLE_SLEEP_MS
        };
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }
}

fn run_outbound_dispatch_loop(
    runtime: Arc<RuntimeInner>,
    settings: Settings,
    allowed_lanes: &'static [QueueLane],
) {
    while !runtime.stop_flag.load(Ordering::SeqCst) {
        let did_work = process_outbound_queue(&runtime, &settings, allowed_lanes);
        let sleep_ms = if did_work {
            QUEUE_LOOP_ACTIVE_SLEEP_MS
        } else {
            QUEUE_LOOP_IDLE_SLEEP_MS
        };
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }
}

fn spawn_incoming_connection_worker(
    runtime: Arc<RuntimeInner>,
    settings: Settings,
    device_id: String,
    stream: TcpStream,
) {
    let _ = std::thread::Builder::new()
        .name("lan-clipboard-incoming".to_string())
        .spawn(move || {
            if let Err(error) = handle_incoming(&runtime, &settings, stream, &device_id) {
                set_error(&runtime, format!("incoming handler failed: {error}"));
            }
        });
}

fn send_udp_announcement(
    socket: &UdpSocket,
    settings: &Settings,
    device_id: &str,
    device_name: &str,
) {
    let announcement = DiscoveryAnnouncement {
        v: 1,
        app: DISCOVERY_APP.to_string(),
        device_id: device_id.to_string(),
        device_name: device_name.to_string(),
        shared_code: settings.sync.shared_code.trim().to_string(),
        tcp_port: settings.sync.listen_port,
    };
    let Ok(bytes) = serde_json::to_vec(&announcement) else {
        return;
    };
    for target in udp_broadcast_targets(&settings.sync.local_ip) {
        let _ = socket.send_to(&bytes, target);
    }
}

fn receive_udp_announcements(
    runtime: &RuntimeInner,
    settings: &Settings,
    device_id: &str,
    socket: &UdpSocket,
) {
    let mut buffer = [0u8; 2048];
    loop {
        match socket.recv_from(&mut buffer) {
            Ok((size, source)) => {
                let announcement =
                    match serde_json::from_slice::<DiscoveryAnnouncement>(&buffer[..size]) {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
                if let Some(device) = announcement_to_device(
                    &announcement,
                    source,
                    device_id,
                    &settings.sync.shared_code,
                ) {
                    mark_known_member(runtime, "device", &device.device_id);
                    mark_known_member(runtime, "addr", &format!("{}:{}", device.addr, device.port));
                    merge_discovered_devices(runtime, vec![device]);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => {
                push_log(
                    runtime,
                    "WARN",
                    &format!("udp discovery receive failed: {error}"),
                );
                break;
            }
        }
    }
}

fn announcement_to_device(
    announcement: &DiscoveryAnnouncement,
    source: SocketAddr,
    self_device_id: &str,
    shared_code: &str,
) -> Option<DiscoveredDevice> {
    if announcement.v != 1
        || announcement.app != DISCOVERY_APP
        || announcement.device_id == self_device_id
        || announcement.shared_code != shared_code.trim()
    {
        return None;
    }
    let source_ip = source.ip();
    if source_ip.is_loopback() {
        return None;
    }
    Some(DiscoveredDevice {
        device_id: announcement.device_id.clone(),
        device_name: if announcement.device_name.trim().is_empty() {
            "局域网设备".to_string()
        } else {
            announcement.device_name.clone()
        },
        addr: source_ip.to_string(),
        port: announcement.tcp_port,
    })
}

fn refresh_discovered_devices(
    runtime: &RuntimeInner,
    settings: &Settings,
    device_id: &str,
    selected_local_ip: Option<&str>,
    timeout_ms: u64,
) {
    match discover_devices(
        device_id,
        &settings.sync.shared_code,
        selected_local_ip,
        timeout_ms,
    ) {
        Ok(devices) => {
            replace_discovered_devices(runtime, selected_local_ip, devices);
            reconcile_member_state(runtime, settings);
        }
        Err(error) => set_error(runtime, format!("peer discovery failed: {error}")),
    }
}

fn handle_incoming(
    runtime: &RuntimeInner,
    settings: &Settings,
    stream: TcpStream,
    device_id: &str,
) -> anyhow::Result<()> {
    let remote_addr = stream.peer_addr().ok().map(|addr| addr.to_string());
    if let Ok(local_addr) = stream.local_addr() {
        remember_active_local_ip(runtime, local_addr.ip());
    }
    let mut stream = stream;
    let mut incoming_files: HashMap<String, IncomingFileStream> = HashMap::new();
    loop {
        let Some(frame_bytes) = (match read_wire_frame(&mut stream) {
            Ok(frame) => Ok(frame),
            Err(error) => {
                discard_incomplete_incoming_files(
                    runtime,
                    &mut incoming_files,
                    "发送方连接中断，已丢弃未完成传输",
                );
                Err(error)
            }
        })?
        else {
            discard_incomplete_incoming_files(
                runtime,
                &mut incoming_files,
                "发送方已下线，已丢弃未完成传输",
            );
            break;
        };
        let frame = bincode::deserialize::<WireMessage>(&frame_bytes)?;
        let body = decode_wire_body(&frame, settings)?;
        match body {
            WireBody::ClipboardItem(item) => {
                handle_incoming_item(
                    runtime,
                    remote_addr.as_deref().unwrap_or("未知来源"),
                    item,
                    device_id,
                );
            }
            WireBody::FileStreamStart(meta) => {
                let canonical_transfer_id =
                    format!("recv:{}:{}", meta.source_device_id, meta.item_id);
                upsert_transfer(
                    runtime,
                    TransferProgress {
                        id: canonical_transfer_id.clone(),
                        direction: "receive".to_string(),
                        peer: remote_addr.as_deref().unwrap_or("未知来源").to_string(),
                        item_kind: "file_bundle".to_string(),
                        item_label: file_stream_label(&meta.top_level_names),
                        item_summary: file_stream_summary(&meta.top_level_names),
                        item_id: meta.item_id.clone(),
                        transferred_bytes: 0,
                        total_bytes: meta.size_bytes,
                        percent: 0,
                        status: "receiving".to_string(),
                        updated_at_ms: now_ms(),
                        error: None,
                    },
                );
                let marker = file_stream_marker(&meta);
                if is_stale_marker(runtime, &marker) {
                    mark_transfer_failed(
                        runtime,
                        &canonical_transfer_id,
                        "已被更新内容替代".to_string(),
                    );
                    push_log(
                        runtime,
                        "DEBUG",
                        &format!(
                            "drop stale inbound file stream start item={} source_device_id={}",
                            meta.item_id, meta.source_device_id
                        ),
                    );
                    continue;
                }
                if update_latest_marker(runtime, marker) {
                    prune_stale_queue_entries(runtime);
                }

                if let Some(previous) = incoming_files.remove(&meta.item_id) {
                    let _ = std::fs::remove_file(&previous.archive_path);
                }

                let safe_source = sanitize_file_component(&meta.source_device_id);
                let safe_item = sanitize_file_component(&meta.item_id);
                let archive_path = std::env::temp_dir().join(format!(
                    "lan-clipboard-incoming-{safe_source}-{safe_item}.archive"
                ));
                let _ = std::fs::remove_file(&archive_path);
                if let Err(error) = std::fs::File::create(&archive_path) {
                    mark_transfer_failed(runtime, &canonical_transfer_id, error.to_string());
                    push_log(
                        runtime,
                        "WARN",
                        &format!(
                            "create inbound temp archive failed item={} path={} error={}",
                            meta.item_id,
                            archive_path.display(),
                            error
                        ),
                    );
                    continue;
                }

                incoming_files.insert(
                    meta.item_id.clone(),
                    IncomingFileStream {
                        meta,
                        archive_path,
                        received_bytes: 0,
                        peer: remote_addr.as_deref().unwrap_or("未知来源").to_string(),
                    },
                );
            }
            WireBody::FileStreamChunk { item_id, bytes } => {
                if let Some(file_stream) = incoming_files.get_mut(&item_id) {
                    let transfer_id = format!(
                        "recv:{}:{}",
                        file_stream.meta.source_device_id, file_stream.meta.item_id
                    );
                    match std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&file_stream.archive_path)
                        .and_then(|mut file| std::io::Write::write_all(&mut file, &bytes))
                    {
                        Ok(()) => {
                            file_stream.received_bytes = file_stream
                                .received_bytes
                                .saturating_add(bytes.len() as u64);
                            update_transfer_progress(
                                runtime,
                                &transfer_id,
                                file_stream.received_bytes,
                                file_stream.meta.size_bytes,
                            );
                        }
                        Err(error) => {
                            mark_transfer_failed(runtime, &transfer_id, error.to_string());
                            push_log(
                                runtime,
                                "WARN",
                                &format!(
                                    "write inbound file chunk failed item={} path={} error={}",
                                    file_stream.meta.item_id,
                                    file_stream.archive_path.display(),
                                    error
                                ),
                            );
                            let _ = std::fs::remove_file(&file_stream.archive_path);
                            incoming_files.remove(&item_id);
                        }
                    }
                }
            }
            WireBody::FileStreamEnd { item_id } => {
                if let Some(file_stream) = incoming_files.remove(&item_id) {
                    let transfer_id = format!(
                        "recv:{}:{}",
                        file_stream.meta.source_device_id, file_stream.meta.item_id
                    );
                    if file_stream.received_bytes != file_stream.meta.size_bytes {
                        mark_transfer_failed(
                            runtime,
                            &transfer_id,
                            format!(
                                "文件接收不完整：已接收 {} 字节，期望 {} 字节",
                                file_stream.received_bytes, file_stream.meta.size_bytes
                            ),
                        );
                        push_log(
                            runtime,
                            "WARN",
                            &format!(
                                "incomplete inbound file stream item={} received_bytes={} expected_bytes={} peer={}",
                                file_stream.meta.item_id,
                                file_stream.received_bytes,
                                file_stream.meta.size_bytes,
                                file_stream.peer
                            ),
                        );
                        let _ = std::fs::remove_file(&file_stream.archive_path);
                        continue;
                    }

                    let item = ClipboardItem {
                        id: file_stream.meta.item_id.clone(),
                        content_hash: file_stream.meta.content_hash,
                        created_at_us: file_stream.meta.created_at_us,
                        source_device_id: file_stream.meta.source_device_id,
                        size_bytes: file_stream.meta.size_bytes,
                        payload: ClipboardPayload::FileBundlePath {
                            archive_path: file_stream.archive_path,
                            top_level_names: file_stream.meta.top_level_names,
                        },
                    };
                    handle_incoming_item(runtime, &file_stream.peer, item, device_id);
                }
            }
            WireBody::FileStreamRawStart(meta) => {
                receive_raw_file_stream(
                    runtime,
                    settings,
                    &mut stream,
                    remote_addr.as_deref().unwrap_or("未知来源"),
                    meta,
                    device_id,
                )?;
            }
        }
    }
    Ok(())
}

fn receive_raw_file_stream(
    runtime: &RuntimeInner,
    settings: &Settings,
    stream: &mut TcpStream,
    peer: &str,
    meta: FileStreamStart,
    device_id: &str,
) -> anyhow::Result<()> {
    let transfer_id = format!("recv:{}:{}", meta.source_device_id, meta.item_id);
    upsert_transfer(
        runtime,
        TransferProgress {
            id: transfer_id.clone(),
            direction: "receive".to_string(),
            peer: peer.to_string(),
            item_kind: "file_bundle".to_string(),
            item_label: file_stream_label(&meta.top_level_names),
            item_summary: file_stream_summary(&meta.top_level_names),
            item_id: meta.item_id.clone(),
            transferred_bytes: 0,
            total_bytes: meta.size_bytes,
            percent: 0,
            status: "receiving".to_string(),
            updated_at_ms: now_ms(),
            error: None,
        },
    );

    let marker = file_stream_marker(&meta);
    if is_stale_marker(runtime, &marker) {
        mark_transfer_failed(runtime, &transfer_id, "已被更新内容替代".to_string());
        return Err(anyhow::anyhow!("stale inbound raw file stream"));
    }
    if update_latest_marker(runtime, marker) {
        prune_stale_queue_entries(runtime);
    }

    let safe_source = sanitize_file_component(&meta.source_device_id);
    let safe_item = sanitize_file_component(&meta.item_id);
    let archive_path = std::env::temp_dir().join(format!(
        "lan-clipboard-incoming-{safe_source}-{safe_item}.archive"
    ));
    let _ = std::fs::remove_file(&archive_path);
    let archive_file = match std::fs::File::create(&archive_path) {
        Ok(file) => file,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            return Err(error.into());
        }
    };
    let mut writer = BufWriter::with_capacity(FILE_STREAM_CHUNK_BYTES, archive_file);
    let mut received_bytes = 0u64;
    let mut last_progress_update = Instant::now();

    while received_bytes < meta.size_bytes {
        let Some(bytes) = read_wire_payload_frame(stream, settings)? else {
            let _ = std::fs::remove_file(&archive_path);
            mark_transfer_failed(
                runtime,
                &transfer_id,
                "发送方已下线，已丢弃未完成传输".to_string(),
            );
            return Err(anyhow::anyhow!(
                "sender disconnected during raw file stream"
            ));
        };
        if bytes.is_empty() {
            continue;
        }
        let remaining = meta.size_bytes.saturating_sub(received_bytes);
        if bytes.len() as u64 > remaining {
            let _ = std::fs::remove_file(&archive_path);
            mark_transfer_failed(runtime, &transfer_id, "文件接收超过声明大小".to_string());
            return Err(anyhow::anyhow!("raw file stream exceeded expected size"));
        }
        writer.write_all(&bytes)?;
        received_bytes = received_bytes.saturating_add(bytes.len() as u64);
        let progress_now = Instant::now();
        if progress_now.duration_since(last_progress_update)
            >= Duration::from_millis(FILE_STREAM_PROGRESS_EMIT_INTERVAL_MS)
            || received_bytes >= meta.size_bytes
        {
            update_transfer_progress(runtime, &transfer_id, received_bytes, meta.size_bytes);
            last_progress_update = progress_now;
        }
    }

    writer.flush()?;
    drop(writer);

    match read_wire_body_from_stream(stream, settings)? {
        Some(WireBody::FileStreamEnd { item_id }) if item_id == meta.item_id => {}
        Some(_) => {
            let _ = std::fs::remove_file(&archive_path);
            mark_transfer_failed(runtime, &transfer_id, "文件流结束帧不匹配".to_string());
            return Err(anyhow::anyhow!("unexpected raw file stream end frame"));
        }
        None => {
            let _ = std::fs::remove_file(&archive_path);
            mark_transfer_failed(
                runtime,
                &transfer_id,
                "发送方已下线，已丢弃未完成传输".to_string(),
            );
            return Err(anyhow::anyhow!(
                "sender disconnected before raw file stream end"
            ));
        }
    }

    let item = ClipboardItem {
        id: meta.item_id,
        content_hash: meta.content_hash,
        created_at_us: meta.created_at_us,
        source_device_id: meta.source_device_id,
        size_bytes: meta.size_bytes,
        payload: ClipboardPayload::FileBundlePath {
            archive_path,
            top_level_names: meta.top_level_names,
        },
    };
    handle_incoming_item(runtime, peer, item, device_id);
    Ok(())
}

fn discard_incomplete_incoming_files(
    runtime: &RuntimeInner,
    incoming_files: &mut HashMap<String, IncomingFileStream>,
    reason: &str,
) {
    for (_, file_stream) in incoming_files.drain() {
        let _ = std::fs::remove_file(&file_stream.archive_path);
        let transfer_id = format!(
            "recv:{}:{}",
            file_stream.meta.source_device_id, file_stream.meta.item_id
        );
        mark_transfer_failed(runtime, &transfer_id, reason.to_string());
        push_log(
            runtime,
            "WARN",
            &format!(
                "discard incomplete inbound file item={} received_bytes={} expected_bytes={} peer={} reason={}",
                file_stream.meta.item_id,
                file_stream.received_bytes,
                file_stream.meta.size_bytes,
                file_stream.peer,
                reason
            ),
        );
    }
}

fn handle_incoming_item(runtime: &RuntimeInner, peer: &str, item: ClipboardItem, device_id: &str) {
    let canonical_transfer_id = canonical_receive_transfer_id(&item);
    if item.source_device_id == device_id {
        return;
    }
    mark_known_member(runtime, "device", &item.source_device_id);
    if should_skip_remote_item(runtime, &item) {
        return;
    }
    upsert_transfer(
        runtime,
        TransferProgress {
            id: canonical_transfer_id.clone(),
            direction: "receive".to_string(),
            peer: peer.to_string(),
            item_kind: item.payload.kind().to_string(),
            item_label: payload_label(&item.payload),
            item_summary: payload_summary(&item.payload),
            item_id: item.id.clone(),
            transferred_bytes: item.size_bytes,
            total_bytes: item.size_bytes,
            percent: 100,
            status: "received".to_string(),
            updated_at_ms: now_ms(),
            error: None,
        },
    );
    push_log(
        runtime,
        "INFO",
        &format!(
            "received item {} kind={} size_bytes={} from {}",
            item.id,
            item.payload.kind(),
            item.size_bytes,
            item.source_device_id
        ),
    );
    enqueue_inbound_item(runtime, item, &canonical_transfer_id, peer);
}

fn read_wire_frame(stream: &mut TcpStream) -> anyhow::Result<Option<Vec<u8>>> {
    let mut len_bytes = [0u8; 4];
    match stream.read_exact(&mut len_bytes) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return Ok(None),
        Err(error) => return Err(error.into()),
    }

    let frame_len = u32::from_be_bytes(len_bytes) as usize;
    if frame_len == 0 || frame_len > MAX_WIRE_FRAME_BYTES {
        return Err(anyhow::anyhow!("invalid wire frame length: {frame_len}"));
    }

    let mut frame = vec![0u8; frame_len];
    read_exact_with_progress(stream, &mut frame)?;
    Ok(Some(frame))
}

struct BroadcastReport {
    attempted: usize,
    delivered: usize,
    deferred: bool,
}

fn send_to_all_peers(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
) -> BroadcastReport {
    let peers = collect_peer_targets(runtime, settings);
    if matches!(item.payload, ClipboardPayload::FileList { .. }) {
        return send_file_list_to_all_peers(runtime, settings, item, peers);
    }

    let payload = match encode_wire_message(item, settings) {
        Ok(payload) => payload,
        Err(error) => {
            set_error(runtime, format!("encode payload failed: {error}"));
            return BroadcastReport {
                attempted: 0,
                delivered: 0,
                deferred: false,
            };
        }
    };

    let attempted = peers.len();
    let delivered = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(peers.len());
        let payload_ref = &payload;
        for peer in peers {
            handles
                .push(scope.spawn(move || send_payload_to_peer(runtime, item, payload_ref, peer)));
        }

        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .filter(|delivered| *delivered)
            .count()
    });

    if delivered > 0 {
        push_log(
            runtime,
            "DEBUG",
            &format!("broadcast delivered={}", delivered),
        );
    }
    BroadcastReport {
        attempted,
        delivered,
        deferred: false,
    }
}

fn send_file_list_to_all_peers(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
    peers: Vec<String>,
) -> BroadcastReport {
    let attempted = peers.len();
    if attempted == 0 {
        return BroadcastReport {
            attempted,
            delivered: 0,
            deferred: false,
        };
    }

    let runtime_addr = runtime as *const RuntimeInner as usize;
    let settings = settings.clone();
    let item = item.clone();
    let item_id = item.id.clone();
    let content_hash = item.content_hash.clone();
    let spawn_result = std::thread::Builder::new()
        .name(format!("lan-clipboard-file-send-{item_id}"))
        .spawn(move || {
            let runtime = unsafe { &*(runtime_addr as *const RuntimeInner) };
            let delivered = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(peers.len());
                for peer in peers {
                    let settings_ref = &settings;
                    let item_ref = &item;
                    handles.push(scope.spawn(move || {
                        send_file_list_to_peer(runtime, settings_ref, item_ref, &peer)
                    }));
                }

                handles
                    .into_iter()
                    .filter_map(|handle| handle.join().ok())
                    .filter(|delivered| *delivered)
                    .count()
            });

            if delivered > 0 {
                mark_shared_fingerprint(runtime, &item.content_hash);
            }
            clear_content_inflight(runtime, &item.content_hash);
            push_log(
                runtime,
                "DEBUG",
                &format!(
                    "file stream completed item={} delivered={} attempted={}",
                    item.id, delivered, attempted
                ),
            );
        });

    if let Err(error) = spawn_result {
        set_error(
            runtime,
            format!("spawn file stream sender failed: item={item_id} error={error}"),
        );
        clear_content_inflight(runtime, &content_hash);
        return BroadcastReport {
            attempted,
            delivered: 0,
            deferred: false,
        };
    }

    BroadcastReport {
        attempted,
        delivered: attempted,
        deferred: true,
    }
}

fn send_payload_to_peer(
    runtime: &RuntimeInner,
    item: &ClipboardItem,
    payload: &[u8],
    peer: String,
) -> bool {
    let connect_timeout = Duration::from_millis(CONNECT_TIMEOUT_MS);
    let socket_addr = match peer.parse::<SocketAddr>() {
        Ok(socket_addr) => socket_addr,
        Err(_) => {
            push_log(runtime, "WARN", &format!("skip bad peer addr: {}", peer));
            return false;
        }
    };
    let transfer_id = send_transfer_id(&peer, item);
    upsert_transfer(
        runtime,
        TransferProgress {
            id: transfer_id.clone(),
            direction: "send".to_string(),
            peer: peer.clone(),
            item_kind: item.payload.kind().to_string(),
            item_label: payload_label(&item.payload),
            item_summary: payload_summary(&item.payload),
            item_id: item.id.clone(),
            transferred_bytes: 0,
            total_bytes: payload.len() as u64,
            percent: 0,
            status: "sending".to_string(),
            updated_at_ms: now_ms(),
            error: None,
        },
    );
    let stream = TcpStream::connect_timeout(&socket_addr, connect_timeout);
    let mut stream = match stream {
        Ok(stream) => stream,
        Err(error) => {
            push_log(
                runtime,
                "DEBUG",
                &format!("connect peer failed peer={peer} error={error}"),
            );
            return false;
        }
    };
    tune_stream_for_send(&stream, payload.len() as u64);
    if let Ok(local_addr) = stream.local_addr() {
        remember_active_local_ip(runtime, local_addr.ip());
    }
    if write_all_with_progress(runtime, &mut stream, payload, &transfer_id).is_ok() {
        mark_transfer_completed(runtime, &transfer_id);
        mark_known_member(runtime, "addr", &peer);
        true
    } else {
        mark_transfer_failed(runtime, &transfer_id, "发送失败".to_string());
        push_log(
            runtime,
            "DEBUG",
            &format!(
                "write peer failed peer={peer} payload_bytes={} timeout_ms={}",
                payload.len(),
                write_timeout_for_payload(payload.len() as u64).as_millis()
            ),
        );
        false
    }
}

fn send_file_list_to_peer(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
    peer: &str,
) -> bool {
    let ClipboardPayload::FileList {
        paths,
        top_level_names,
        estimated_archive_bytes,
    } = &item.payload
    else {
        return false;
    };
    let socket_addr: SocketAddr = match peer.parse() {
        Ok(socket_addr) => socket_addr,
        Err(error) => {
            push_log(
                runtime,
                "WARN",
                &format!("skip bad file peer addr peer={peer} error={error}"),
            );
            return false;
        }
    };
    let transfer_id = send_transfer_id(peer, item);
    upsert_transfer(
        runtime,
        TransferProgress {
            id: transfer_id.clone(),
            direction: "send".to_string(),
            peer: peer.to_string(),
            item_kind: item.payload.kind().to_string(),
            item_label: payload_label(&item.payload),
            item_summary: payload_summary(&item.payload),
            item_id: item.id.clone(),
            transferred_bytes: 0,
            total_bytes: *estimated_archive_bytes,
            percent: 0,
            status: "sending".to_string(),
            updated_at_ms: now_ms(),
            error: None,
        },
    );
    let mut stream =
        match TcpStream::connect_timeout(&socket_addr, Duration::from_millis(CONNECT_TIMEOUT_MS)) {
            Ok(stream) => stream,
            Err(error) => {
                push_log(
                    runtime,
                    "DEBUG",
                    &format!("connect file peer failed peer={peer} error={error}"),
                );
                return false;
            }
        };
    tune_stream_for_send(&stream, *estimated_archive_bytes);
    if let Ok(local_addr) = stream.local_addr() {
        remember_active_local_ip(runtime, local_addr.ip());
    }

    let start = WireBody::FileStreamRawStart(FileStreamStart {
        item_id: item.id.clone(),
        content_hash: item.content_hash.clone(),
        created_at_us: item.created_at_us,
        source_device_id: item.source_device_id.clone(),
        size_bytes: *estimated_archive_bytes,
        top_level_names: top_level_names.clone(),
    });
    if let Err(error) = write_wire_body_to_stream(&mut stream, settings, &start) {
        mark_transfer_failed(runtime, &transfer_id, error.to_string());
        push_log(
            runtime,
            "DEBUG",
            &format!("stream file start failed peer={peer} error={error}"),
        );
        return false;
    }

    if let Err(error) = stream_file_list_archive_to_peer(
        runtime,
        settings,
        &mut stream,
        item,
        paths,
        *estimated_archive_bytes,
        &transfer_id,
    ) {
        mark_transfer_failed(runtime, &transfer_id, error.to_string());
        push_log(
            runtime,
            "DEBUG",
            &format!("stream file archive failed peer={peer} error={error}"),
        );
        return false;
    }

    if let Err(error) = write_wire_body_to_stream(
        &mut stream,
        settings,
        &WireBody::FileStreamEnd {
            item_id: item.id.clone(),
        },
    ) {
        mark_transfer_failed(runtime, &transfer_id, error.to_string());
        push_log(
            runtime,
            "DEBUG",
            &format!("stream file end failed peer={peer} error={error}"),
        );
        return false;
    }
    mark_transfer_completed(runtime, &transfer_id);
    mark_known_member(runtime, "addr", peer);
    true
}

fn stream_file_list_archive_to_peer(
    runtime: &RuntimeInner,
    settings: &Settings,
    stream: &mut TcpStream,
    item: &ClipboardItem,
    paths: &[PathBuf],
    size_bytes: u64,
    transfer_id: &str,
) -> anyhow::Result<()> {
    let archive_started_at = Instant::now();
    let mut writer = FileStreamNetworkWriter::new(
        runtime,
        settings,
        stream,
        transfer_id,
        size_bytes,
        item_marker(item),
    );
    clipboard::stream_file_bundle_archive(paths, &mut writer)?;
    writer.finish()?;
    let sent_bytes = writer.sent_bytes();
    if sent_bytes != size_bytes {
        return Err(anyhow::anyhow!(
            "streamed archive size mismatch: sent {sent_bytes} bytes, expected {size_bytes} bytes"
        ));
    }
    push_log(
        runtime,
        "DEBUG",
        &format!(
            "streamed file archive item={} size_bytes={} elapsed_ms={}",
            item.id,
            sent_bytes,
            archive_started_at.elapsed().as_millis()
        ),
    );
    Ok(())
}

struct FileStreamNetworkWriter<'a> {
    runtime: &'a RuntimeInner,
    settings: &'a Settings,
    stream: &'a mut TcpStream,
    transfer_id: &'a str,
    total_bytes: u64,
    marker: ItemMarker,
    buffer: Vec<u8>,
    sent_bytes: u64,
    last_progress_update: Instant,
}

impl<'a> FileStreamNetworkWriter<'a> {
    fn new(
        runtime: &'a RuntimeInner,
        settings: &'a Settings,
        stream: &'a mut TcpStream,
        transfer_id: &'a str,
        total_bytes: u64,
        marker: ItemMarker,
    ) -> Self {
        Self {
            runtime,
            settings,
            stream,
            transfer_id,
            total_bytes,
            marker,
            buffer: Vec::with_capacity(FILE_STREAM_CHUNK_BYTES),
            sent_bytes: 0,
            last_progress_update: Instant::now(),
        }
    }

    fn sent_bytes(&self) -> u64 {
        self.sent_bytes
    }

    fn finish(&mut self) -> anyhow::Result<()> {
        self.flush_buffer()?;
        self.stream.flush()?;
        Ok(())
    }

    fn ensure_current(&self) -> anyhow::Result<()> {
        if transfer_should_abort(self.runtime, self.transfer_id) {
            return Err(anyhow::anyhow!("transfer canceled"));
        }
        if is_stale_marker(self.runtime, &self.marker) {
            return Err(anyhow::anyhow!("superseded by newer clipboard item"));
        }
        Ok(())
    }

    fn flush_buffer(&mut self) -> anyhow::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::take(&mut self.buffer);
        self.write_frame(&chunk)?;
        self.buffer = Vec::with_capacity(FILE_STREAM_CHUNK_BYTES);
        Ok(())
    }

    fn write_frame(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.ensure_current()?;
        write_wire_payload_to_stream(self.stream, self.settings, bytes)?;
        self.sent_bytes = self.sent_bytes.saturating_add(bytes.len() as u64);
        self.maybe_update_progress();
        if has_ready_outbound_lane(self.runtime, &[QueueLane::Realtime, QueueLane::Visual]) {
            std::thread::sleep(Duration::from_millis(HIGH_PRIORITY_YIELD_MS));
        }
        Ok(())
    }

    fn maybe_update_progress(&mut self) {
        let progress_now = Instant::now();
        if progress_now.duration_since(self.last_progress_update)
            >= Duration::from_millis(FILE_STREAM_PROGRESS_EMIT_INTERVAL_MS)
            || self.sent_bytes >= self.total_bytes
        {
            update_transfer_progress(
                self.runtime,
                self.transfer_id,
                self.sent_bytes,
                self.total_bytes,
            );
            self.last_progress_update = progress_now;
        }
    }

    fn write_all_inner(&mut self, mut input: &[u8]) -> anyhow::Result<()> {
        while !input.is_empty() {
            if self.buffer.is_empty() && input.len() >= FILE_STREAM_CHUNK_BYTES {
                let (chunk, rest) = input.split_at(FILE_STREAM_CHUNK_BYTES);
                self.write_frame(chunk)?;
                input = rest;
                continue;
            }

            let capacity_left = FILE_STREAM_CHUNK_BYTES - self.buffer.len();
            let take = capacity_left.min(input.len());
            self.buffer.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.buffer.len() >= FILE_STREAM_CHUNK_BYTES {
                self.flush_buffer()?;
            }
        }
        Ok(())
    }
}

impl Write for FileStreamNetworkWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.write_all_inner(buffer)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error.to_string()))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

fn write_wire_body_to_stream(
    stream: &mut TcpStream,
    settings: &Settings,
    body: &WireBody,
) -> anyhow::Result<()> {
    let payload = encode_wire_body(body, settings)?;
    stream.write_all(&payload)?;
    Ok(())
}

fn write_wire_payload_to_stream(
    stream: &mut TcpStream,
    settings: &Settings,
    plain: &[u8],
) -> anyhow::Result<()> {
    if settings.security.encryption_enabled {
        let (nonce, encrypted) =
            encrypt_raw_payload_bytes(plain, &derive_key(&effective_secret(settings)))?;
        let frame_len = 2usize
            .saturating_add(nonce.len())
            .saturating_add(encrypted.len());
        if frame_len > u32::MAX as usize {
            return Err(anyhow::anyhow!("raw payload frame too large"));
        }
        stream.write_all(&(frame_len as u32).to_be_bytes())?;
        stream.write_all(&[WIRE_VERSION, RAW_PAYLOAD_ENCRYPTED_FLAG])?;
        stream.write_all(&nonce)?;
        stream.write_all(&encrypted)?;
    } else {
        let frame_len = 2usize.saturating_add(plain.len());
        if frame_len > u32::MAX as usize {
            return Err(anyhow::anyhow!("raw payload frame too large"));
        }
        stream.write_all(&(frame_len as u32).to_be_bytes())?;
        stream.write_all(&[WIRE_VERSION, 0])?;
        stream.write_all(plain)?;
    }
    Ok(())
}

fn read_wire_payload_frame(
    stream: &mut TcpStream,
    settings: &Settings,
) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(frame_bytes) = read_wire_frame(stream)? else {
        return Ok(None);
    };
    if frame_bytes.len() < 2 {
        return Err(anyhow::anyhow!("raw payload frame too short"));
    }
    let version = frame_bytes[0];
    if version != WIRE_VERSION {
        return Err(anyhow::anyhow!(
            "unsupported raw payload version: {version}"
        ));
    }
    let encrypted = frame_bytes[1] == RAW_PAYLOAD_ENCRYPTED_FLAG;
    if encrypted {
        if frame_bytes.len() < 14 {
            return Err(anyhow::anyhow!("encrypted raw payload frame too short"));
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&frame_bytes[2..14]);
        return decrypt_raw_payload_bytes(
            nonce,
            &frame_bytes[14..],
            &derive_key(&effective_secret(settings)),
        )
        .map(Some);
    }
    if settings.security.encryption_enabled {
        return Err(anyhow::anyhow!(
            "received plain raw payload but encryption enabled"
        ));
    }
    Ok(Some(frame_bytes[2..].to_vec()))
}

fn read_wire_body_from_stream(
    stream: &mut TcpStream,
    settings: &Settings,
) -> anyhow::Result<Option<WireBody>> {
    let Some(frame_bytes) = read_wire_frame(stream)? else {
        return Ok(None);
    };
    let frame = bincode::deserialize::<WireMessage>(&frame_bytes)?;
    Ok(Some(decode_wire_body(&frame, settings)?))
}

fn read_exact_with_progress(stream: &mut TcpStream, buffer: &mut [u8]) -> anyhow::Result<()> {
    let mut offset = 0usize;
    while offset < buffer.len() {
        let end = (offset + TRANSFER_CHUNK_BYTES).min(buffer.len());
        stream.read_exact(&mut buffer[offset..end])?;
        offset = end;
    }
    Ok(())
}

fn write_all_with_progress(
    runtime: &RuntimeInner,
    stream: &mut TcpStream,
    buffer: &[u8],
    transfer_id: &str,
) -> anyhow::Result<()> {
    let total_bytes = buffer.len() as u64;
    let mut offset = 0usize;
    while offset < buffer.len() {
        if transfer_should_abort(runtime, transfer_id) {
            return Err(anyhow::anyhow!("transfer canceled"));
        }
        let end = (offset + TRANSFER_CHUNK_BYTES).min(buffer.len());
        if let Err(error) = stream.write_all(&buffer[offset..end]) {
            mark_transfer_failed(runtime, transfer_id, error.to_string());
            return Err(error.into());
        }
        offset = end;
        update_transfer_progress(runtime, transfer_id, offset as u64, total_bytes);
    }
    Ok(())
}

fn process_inbound_queue(runtime: &RuntimeInner, settings: &Settings) -> bool {
    let mut did_work = false;
    loop {
        let Some(mut entry) = pop_ready_queue_entry(&runtime.inbound_queue) else {
            break;
        };
        did_work = true;
        if is_stale_marker(runtime, &item_marker(&entry.item)) {
            if let Some(transfer_id) = find_receive_transfer_id(runtime, &entry.item.id) {
                mark_transfer_failed(runtime, &transfer_id, "已被更新内容替代".to_string());
            }
            continue;
        }

        register_ignored_local_hash(runtime, &entry.item.content_hash);
        let transfer_id = find_receive_transfer_id(runtime, &entry.item.id);
        if let Some(transfer_id) = transfer_id.as_deref() {
            update_transfer_metadata(
                runtime,
                transfer_id,
                entry.item.payload.kind(),
                &payload_label(&entry.item.payload),
                &payload_summary(&entry.item.payload),
                &entry.item.id,
                "applying",
            );
        }
        match clipboard::write_item(&entry.item, &settings.limits) {
            Ok(applied) => {
                mark_shared_fingerprint(runtime, &entry.item.content_hash);
                if let Some(local_content_hash) = applied.content_hash.as_deref() {
                    register_ignored_local_hash(runtime, &local_content_hash);
                    mark_shared_fingerprint(runtime, local_content_hash);
                }
                if let Some(transfer_id) = transfer_id.as_deref() {
                    mark_transfer_completed(runtime, transfer_id);
                }
                push_log(
                    runtime,
                    "INFO",
                    &format!(
                        "applied item {} kind={} size_bytes={} from {} after {} attempt(s)",
                        entry.item.id,
                        entry.item.payload.kind(),
                        entry.item.size_bytes,
                        entry.item.source_device_id,
                        entry.attempts + 1
                    ),
                )
            }
            Err(crate::clipboard::ClipboardError::Backend(error)) => {
                if schedule_retry(&mut entry) {
                    if let Some(transfer_id) = transfer_id.as_deref() {
                        update_transfer_status(
                            runtime,
                            transfer_id,
                            "retrying",
                            Some(error.clone()),
                        );
                    }
                    push_log(
                        runtime,
                        "WARN",
                        &format!(
                            "apply retry queued for item {}: {} (attempt={})",
                            entry.item.id, error, entry.attempts
                        ),
                    );
                    push_queue_entry(&runtime.inbound_queue, entry);
                } else {
                    if let Some(transfer_id) = transfer_id.as_deref() {
                        mark_transfer_failed(runtime, transfer_id, error.clone());
                    }
                    set_error(
                        runtime,
                        format!(
                            "apply clipboard item failed after retries: item={} error={error}",
                            entry.item.id
                        ),
                    );
                }
            }
            Err(error) => {
                if let Some(transfer_id) = transfer_id.as_deref() {
                    mark_transfer_failed(runtime, transfer_id, error.to_string());
                }
                set_error(
                    runtime,
                    format!(
                        "apply clipboard item failed permanently: item={} error={error}",
                        entry.item.id
                    ),
                )
            }
        }
    }
    did_work
}

fn process_outbound_queue(
    runtime: &RuntimeInner,
    settings: &Settings,
    allowed_lanes: &[QueueLane],
) -> bool {
    let mut did_work = false;
    loop {
        let Some(mut entry) = pop_ready_outbound_entry(&runtime.outbound_queue, allowed_lanes)
        else {
            break;
        };
        did_work = true;
        if is_stale_marker(runtime, &item_marker(&entry.item)) {
            clear_content_inflight(runtime, &entry.item.content_hash);
            push_log(
                runtime,
                "DEBUG",
                &format!("drop stale outbound item {}", entry.item.id),
            );
            continue;
        }

        let report = send_to_all_peers(runtime, settings, &entry.item);
        if report.attempted == 0 {
            clear_content_inflight(runtime, &entry.item.content_hash);
            push_log(
                runtime,
                "DEBUG",
                &format!(
                    "drop outbound item {} because shared domain only contains self",
                    entry.item.id
                ),
            );
            continue;
        }
        if report.deferred {
            push_log(
                runtime,
                "DEBUG",
                &format!("outbound item {} deferred to file sender", entry.item.id),
            );
            continue;
        }
        if report.delivered < report.attempted {
            if schedule_retry(&mut entry) {
                push_log(
                    runtime,
                    "DEBUG",
                    &format!(
                        "outbound item {} pending peers delivered={delivered} attempted={attempted} retry={}",
                        entry.item.id,
                        entry.attempts,
                        delivered = report.delivered,
                        attempted = report.attempted
                    ),
                );
                push_queue_entry(&runtime.outbound_queue, entry);
                continue;
            }
            push_log(
                runtime,
                "WARN",
                &format!(
                    "drop outbound item {} after retries delivered={delivered} attempted={attempted}",
                    entry.item.id,
                    delivered = report.delivered,
                    attempted = report.attempted
                ),
            );
            clear_content_inflight(runtime, &entry.item.content_hash);
            continue;
        }

        mark_shared_fingerprint(runtime, &entry.item.content_hash);
        clear_content_inflight(runtime, &entry.item.content_hash);
        push_log(
            runtime,
            "DEBUG",
            &format!(
                "outbound item {} completed delivered={delivered} attempted={attempted}",
                entry.item.id,
                delivered = report.delivered,
                attempted = report.attempted
            ),
        );
    }
    did_work
}

fn reconcile_member_state(runtime: &RuntimeInner, settings: &Settings) {
    let visible_peers = collect_peer_targets(runtime, settings);
    let visible_peer_ips = visible_peers
        .iter()
        .filter_map(|peer| peer.parse::<SocketAddr>().ok())
        .map(|socket_addr| socket_addr.ip().to_string())
        .collect::<HashSet<_>>();

    if visible_peers.is_empty() {
        if let Ok(mut guard) = runtime.outbound_queue.lock() {
            guard.clear();
        }
        if let Ok(mut guard) = runtime.transfers.lock() {
            for entry in guard.iter_mut() {
                if matches!(entry.direction.as_str(), "send")
                    && matches!(entry.status.as_str(), "sending" | "retrying")
                {
                    entry.status = "failed".to_string();
                    entry.error = Some("共享域只剩本机，已停止发送".to_string());
                    entry.updated_at_ms = now_ms();
                }
            }
        }
        return;
    }

    if let Ok(mut guard) = runtime.transfers.lock() {
        for entry in guard.iter_mut() {
            if entry.direction != "receive" || entry.status != "receiving" {
                continue;
            }
            let Some(socket_addr) = entry.peer.parse::<SocketAddr>().ok() else {
                continue;
            };
            if !visible_peer_ips.contains(&socket_addr.ip().to_string()) {
                entry.status = "failed".to_string();
                entry.error = Some("发送方已离线，已停止接收".to_string());
                entry.updated_at_ms = now_ms();
            }
        }
    }
}

fn pop_ready_queue_entry(queue: &Mutex<VecDeque<QueueEntry>>) -> Option<QueueEntry> {
    let now = now_ms();
    queue.lock().ok().and_then(|mut guard| {
        let position = guard
            .iter()
            .position(|entry| entry.next_attempt_at_ms <= now)?;
        guard.remove(position)
    })
}

fn pop_ready_outbound_entry(
    queue: &Mutex<VecDeque<QueueEntry>>,
    allowed_lanes: &[QueueLane],
) -> Option<QueueEntry> {
    let now = now_ms();
    queue.lock().ok().and_then(|mut guard| {
        let position = guard
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.next_attempt_at_ms <= now)
            .filter(|(_, entry)| allowed_lanes.contains(&outbound_lane(entry)))
            .max_by(|(_, left), (_, right)| compare_outbound_entries(left, right))?
            .0;
        guard.remove(position)
    })
}

fn has_ready_outbound_lane(runtime: &RuntimeInner, lanes: &[QueueLane]) -> bool {
    let now = now_ms();
    runtime
        .outbound_queue
        .lock()
        .map(|guard| {
            guard.iter().any(|entry| {
                entry.next_attempt_at_ms <= now && lanes.contains(&outbound_lane(entry))
            })
        })
        .unwrap_or(false)
}

fn push_queue_entry(queue: &Mutex<VecDeque<QueueEntry>>, entry: QueueEntry) {
    if let Ok(mut guard) = queue.lock() {
        guard.push_back(entry);
    }
}

fn compare_outbound_entries(left: &QueueEntry, right: &QueueEntry) -> std::cmp::Ordering {
    outbound_phase_rank(left)
        .cmp(&outbound_phase_rank(right))
        .reverse()
        .then_with(|| outbound_lane(left).cmp(&outbound_lane(right)).reverse())
        .then_with(|| left.item.created_at_us.cmp(&right.item.created_at_us))
        .then_with(|| right.attempts.cmp(&left.attempts))
        .then_with(|| left.queued_at_ms.cmp(&right.queued_at_ms))
}

fn outbound_phase_rank(entry: &QueueEntry) -> u8 {
    if entry.attempts == 0 {
        1
    } else {
        0
    }
}

fn outbound_lane(entry: &QueueEntry) -> QueueLane {
    match &entry.item.payload {
        ClipboardPayload::Text { .. }
        | ClipboardPayload::Html { .. }
        | ClipboardPayload::Rtf { .. } => QueueLane::Realtime,
        ClipboardPayload::ImagePng { .. } => QueueLane::Visual,
        ClipboardPayload::FileBundle { .. }
        | ClipboardPayload::FileBundlePath { .. }
        | ClipboardPayload::FileList { .. } => QueueLane::Bulk,
    }
}

fn schedule_retry(entry: &mut QueueEntry) -> bool {
    if entry.attempts >= QUEUE_MAX_RETRIES {
        return false;
    }
    if now_ms().saturating_sub(entry.queued_at_ms) >= QUEUE_MAX_AGE_MS {
        return false;
    }

    entry.attempts += 1;
    let retry_delay_ms =
        (QUEUE_RETRY_BASE_MS.saturating_mul(entry.attempts as u64)).min(QUEUE_RETRY_MAX_MS);
    entry.next_attempt_at_ms = now_ms().saturating_add(retry_delay_ms);
    true
}

fn new_queue_entry(item: ClipboardItem) -> QueueEntry {
    let queued_at_ms = now_ms();
    QueueEntry {
        item,
        attempts: 0,
        queued_at_ms,
        next_attempt_at_ms: queued_at_ms,
    }
}

fn collect_peer_targets(runtime: &RuntimeInner, settings: &Settings) -> Vec<String> {
    prune_stale_discovered_devices(runtime);
    let active_local_ip = runtime
        .active_local_ip
        .lock()
        .ok()
        .and_then(|guard| (*guard).clone());
    let effective_local_ip = selected_or_active_local_ip(settings, None, active_local_ip);
    let selected_ipv4 = parse_selected_ipv4(effective_local_ip.as_deref())
        .ok()
        .flatten();
    let self_device_id = settings.sync_device_id();
    let mut peer_by_ip = HashMap::new();
    if let Ok(guard) = runtime.discovered_devices.lock() {
        for device in guard.iter() {
            if device.device_id == self_device_id {
                continue;
            }
            if let Some(selected_ipv4) = selected_ipv4 {
                if let Ok(peer_ipv4) = device.addr.parse::<std::net::Ipv4Addr>() {
                    if !is_same_subnet(selected_ipv4, peer_ipv4) {
                        continue;
                    }
                }
            }
            let peer = format!("{}:{}", device.addr, device.port);
            if let Ok(socket_addr) = peer.parse::<SocketAddr>() {
                if is_self_socket_addr(&socket_addr, effective_local_ip.as_deref()) {
                    continue;
                }
                peer_by_ip.insert(socket_addr.ip().to_string(), peer);
            }
        }
    }
    let mut peers = peer_by_ip.into_values().collect::<Vec<_>>();
    peers.sort();
    peers
}

fn is_self_socket_addr(socket_addr: &SocketAddr, effective_local_ip: Option<&str>) -> bool {
    if socket_addr.ip().is_loopback() {
        return true;
    }
    effective_local_ip
        .and_then(|ip| ip.parse::<IpAddr>().ok())
        .map(|local_ip| socket_addr.ip() == local_ip)
        .unwrap_or(false)
}

fn send_transfer_id(peer: &str, item: &ClipboardItem) -> String {
    let peer_key = peer
        .parse::<SocketAddr>()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| peer.to_string());
    format!("send:{peer_key}:{}", item.id)
}

fn write_timeout_for_payload(payload_bytes: u64) -> Duration {
    let estimated_ms = payload_bytes
        .checked_div(WRITE_TIMEOUT_BYTES_PER_MS)
        .unwrap_or(MAX_WRITE_TIMEOUT_MS)
        .saturating_add(MIN_WRITE_TIMEOUT_MS);
    Duration::from_millis(estimated_ms.clamp(MIN_WRITE_TIMEOUT_MS, MAX_WRITE_TIMEOUT_MS))
}

fn tune_stream_for_send(stream: &TcpStream, payload_bytes: u64) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(write_timeout_for_payload(payload_bytes)));
    let socket = SockRef::from(stream);
    let _ = socket.set_send_buffer_size(TCP_BUFFER_BYTES);
    let _ = socket.set_recv_buffer_size(TCP_BUFFER_BYTES);
}

fn tune_stream_for_receive(stream: &TcpStream, payload_bytes: u64) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(write_timeout_for_payload(payload_bytes)));
    let socket = SockRef::from(stream);
    let _ = socket.set_send_buffer_size(TCP_BUFFER_BYTES);
    let _ = socket.set_recv_buffer_size(TCP_BUFFER_BYTES);
}

fn mark_known_member(runtime: &RuntimeInner, kind: &str, member: &str) {
    let value = member.trim();
    if value.is_empty() {
        return;
    }
    if let Ok(mut guard) = runtime.known_members.lock() {
        guard.insert(format!("{kind}:{value}"));
    }
}

fn clear_member_cache(runtime: &RuntimeInner) {
    if let Ok(mut guard) = runtime.discovered_devices.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.discovered_seen_at.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.known_members.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.outbound_queue.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.inbound_queue.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.latest_item.lock() {
        guard.take();
    }
    if let Ok(mut guard) = runtime.shared_content_fingerprint.lock() {
        guard.take();
    }
    if let Ok(mut guard) = runtime.inflight_content_fingerprints.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.last_local_observed.lock() {
        guard.take();
    }
    if let Ok(mut guard) = runtime.ignored_local_hashes.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.recent_event_ids.lock() {
        guard.clear();
    }
}

fn merge_discovered_devices(runtime: &RuntimeInner, devices: Vec<DiscoveredDevice>) {
    if devices.is_empty() {
        return;
    }

    let now = Instant::now();
    if let Ok(mut seen_at) = runtime.discovered_seen_at.lock() {
        for device in &devices {
            if !device.device_id.trim().is_empty() {
                seen_at.insert(device.device_id.clone(), now);
            }
        }
    }

    if let Ok(mut guard) = runtime.discovered_devices.lock() {
        for device in devices {
            if device.device_id.trim().is_empty() {
                continue;
            }
            if let Some(existing) = guard
                .iter_mut()
                .find(|existing| existing.device_id == device.device_id)
            {
                *existing = device;
            } else {
                guard.push(device);
            }
        }
        guard.sort_by(|left, right| left.device_name.cmp(&right.device_name));
        guard.dedup_by(|left, right| left.device_id == right.device_id);
        if guard.len() > DISCOVERED_DEVICE_LIMIT {
            guard.truncate(DISCOVERED_DEVICE_LIMIT);
        }
    }
}

fn replace_discovered_devices(
    runtime: &RuntimeInner,
    selected_local_ip: Option<&str>,
    devices: Vec<DiscoveredDevice>,
) {
    let now = Instant::now();
    let selected_ipv4 = parse_selected_ipv4(selected_local_ip).ok().flatten();
    let existing_devices = runtime
        .discovered_devices
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    let new_ids = devices
        .iter()
        .filter(|device| !device.device_id.trim().is_empty())
        .map(|device| device.device_id.clone())
        .collect::<HashSet<_>>();

    if let Ok(mut seen_at) = runtime.discovered_seen_at.lock() {
        if let Some(selected_ipv4) = selected_ipv4 {
            seen_at.retain(|device_id, _| {
                if new_ids.contains(device_id) {
                    return true;
                }
                existing_devices
                    .iter()
                    .find(|device| &device.device_id == device_id)
                    .map(|device| !device_matches_selected_subnet(&device, selected_ipv4))
                    .unwrap_or(false)
            });
        } else {
            seen_at.retain(|device_id, _| new_ids.contains(device_id));
        }
        for device in &devices {
            if !device.device_id.trim().is_empty() {
                seen_at.insert(device.device_id.clone(), now);
            }
        }
    }

    if let Ok(mut guard) = runtime.discovered_devices.lock() {
        if let Some(selected_ipv4) = selected_ipv4 {
            guard.retain(|device| {
                !device_matches_selected_subnet(device, selected_ipv4)
                    || new_ids.contains(&device.device_id)
            });
        } else {
            guard.retain(|device| new_ids.contains(&device.device_id));
        }

        for device in devices {
            if device.device_id.trim().is_empty() {
                continue;
            }
            if let Some(existing) = guard
                .iter_mut()
                .find(|existing| existing.device_id == device.device_id)
            {
                *existing = device;
            } else {
                guard.push(device);
            }
        }
        guard.sort_by(|left, right| left.device_name.cmp(&right.device_name));
    }
}

fn prune_stale_discovered_devices(runtime: &RuntimeInner) {
    let active_ids = match runtime.discovered_seen_at.lock() {
        Ok(mut seen_at) => {
            seen_at.retain(|_, last_seen| {
                last_seen.elapsed() < Duration::from_millis(DISCOVERY_MEMBER_TTL_MS)
            });
            seen_at.keys().cloned().collect::<HashSet<_>>()
        }
        Err(_) => return,
    };

    if let Ok(mut guard) = runtime.discovered_devices.lock() {
        guard.retain(|device| active_ids.contains(&device.device_id));
    }
}

fn device_matches_selected_subnet(
    device: &DiscoveredDevice,
    selected_ipv4: std::net::Ipv4Addr,
) -> bool {
    device
        .addr
        .parse::<std::net::Ipv4Addr>()
        .map(|peer_ipv4| is_same_subnet(selected_ipv4, peer_ipv4))
        .unwrap_or(false)
}

pub fn build_item(
    payload: &ClipboardPayload,
    device_id: &str,
) -> Result<Option<ClipboardItem>, crate::clipboard::ClipboardError> {
    let size_bytes = match payload {
        ClipboardPayload::FileList {
            estimated_archive_bytes,
            ..
        } => *estimated_archive_bytes,
        ClipboardPayload::Text { text } => text.as_bytes().len() as u64,
        ClipboardPayload::ImagePng { png_bytes } => png_bytes.len() as u64,
        ClipboardPayload::FileBundle { archive_bytes, .. } => archive_bytes.len() as u64,
        ClipboardPayload::FileBundlePath { archive_path, .. } => std::fs::metadata(archive_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0),
        ClipboardPayload::Html { html } => html.as_bytes().len() as u64,
        ClipboardPayload::Rtf { rtf } => rtf.as_bytes().len() as u64,
    };
    if size_bytes == 0 {
        return Ok(None);
    }

    let created_at_us = now_us();
    let content_hash = clipboard::payload_content_hash(payload)?;

    Ok(Some(ClipboardItem {
        id: Uuid::new_v4().to_string(),
        content_hash,
        created_at_us,
        source_device_id: device_id.to_string(),
        size_bytes,
        payload: payload.clone(),
    }))
}

fn encode_wire_message(item: &ClipboardItem, settings: &Settings) -> anyhow::Result<Vec<u8>> {
    encode_wire_body(&WireBody::ClipboardItem(item.clone()), settings)
}

fn encode_wire_body(body: &WireBody, settings: &Settings) -> anyhow::Result<Vec<u8>> {
    let plain = bincode::serialize(body)?;
    let source_device_id = wire_body_source_device_id(body);
    encode_wire_payload(&plain, &source_device_id, settings)
}

fn encode_wire_payload(
    plain: &[u8],
    source_device_id: &str,
    settings: &Settings,
) -> anyhow::Result<Vec<u8>> {
    let frame = if settings.security.encryption_enabled {
        let secret = effective_secret(settings);
        let (nonce, body) = encrypt_bytes(plain, &derive_key(&secret))?;
        WireMessage {
            v: WIRE_VERSION,
            encrypted: true,
            source_device_id: source_device_id.to_string(),
            nonce: Some(nonce),
            body,
        }
    } else {
        WireMessage {
            v: WIRE_VERSION,
            encrypted: false,
            source_device_id: source_device_id.to_string(),
            nonce: None,
            body: plain.to_vec(),
        }
    };

    let frame_bytes = bincode::serialize(&frame)?;
    if frame_bytes.len() > u32::MAX as usize {
        return Err(anyhow::anyhow!("wire frame too large"));
    }
    let mut payload = Vec::with_capacity(4 + frame_bytes.len());
    payload.extend_from_slice(&(frame_bytes.len() as u32).to_be_bytes());
    payload.extend_from_slice(&frame_bytes);
    Ok(payload)
}

fn wire_body_source_device_id(body: &WireBody) -> String {
    match body {
        WireBody::ClipboardItem(item) => item.source_device_id.clone(),
        WireBody::FileStreamStart(meta) | WireBody::FileStreamRawStart(meta) => {
            meta.source_device_id.clone()
        }
        WireBody::FileStreamChunk { .. } | WireBody::FileStreamEnd { .. } => String::new(),
    }
}

fn payload_summary(payload: &ClipboardPayload) -> String {
    match payload {
        ClipboardPayload::Text { text } => {
            let snippet = text_preview_snippet(text, 20_000);
            if snippet.is_empty() {
                "直接复制文字".to_string()
            } else {
                snippet
            }
        }
        ClipboardPayload::ImagePng { .. } => "图片 PNG".to_string(),
        ClipboardPayload::FileBundle {
            top_level_names, ..
        }
        | ClipboardPayload::FileBundlePath {
            top_level_names, ..
        }
        | ClipboardPayload::FileList {
            top_level_names, ..
        } => {
            if top_level_names.is_empty() {
                "文件".to_string()
            } else if top_level_names.len() == 1 {
                format!("{}：{}", payload_label(payload), top_level_names[0])
            } else {
                format!(
                    "{}：{} +{}",
                    payload_label(payload),
                    top_level_names[0],
                    top_level_names.len() - 1
                )
            }
        }
        ClipboardPayload::Html { html } => {
            let snippet = text_preview_snippet(html, 20_000);
            if snippet.is_empty() {
                "HTML".to_string()
            } else {
                snippet
            }
        }
        ClipboardPayload::Rtf { rtf } => {
            let snippet = text_preview_snippet(rtf, 20_000);
            if snippet.is_empty() {
                "RTF".to_string()
            } else {
                snippet
            }
        }
    }
}

fn payload_label(payload: &ClipboardPayload) -> String {
    match payload {
        ClipboardPayload::Text { .. } => "直接复制文字".to_string(),
        ClipboardPayload::ImagePng { .. } => "图片".to_string(),
        ClipboardPayload::FileBundle {
            top_level_names, ..
        }
        | ClipboardPayload::FileBundlePath {
            top_level_names, ..
        }
        | ClipboardPayload::FileList {
            top_level_names, ..
        } => {
            if top_level_names
                .iter()
                .all(|name| looks_like_text_file(name))
            {
                "文本文件".to_string()
            } else {
                "文件".to_string()
            }
        }
        ClipboardPayload::Html { .. } => "HTML 富文本".to_string(),
        ClipboardPayload::Rtf { .. } => "RTF 富文本".to_string(),
    }
}

fn file_stream_label(top_level_names: &[String]) -> String {
    if top_level_names
        .iter()
        .all(|name| looks_like_text_file(name))
    {
        "文本文件".to_string()
    } else {
        "文件".to_string()
    }
}

fn file_stream_summary(top_level_names: &[String]) -> String {
    if top_level_names.is_empty() {
        return "文件".to_string();
    }
    if top_level_names.len() == 1 {
        return format!(
            "{}：{}",
            file_stream_label(top_level_names),
            top_level_names[0]
        );
    }
    format!(
        "{}：{} +{}",
        file_stream_label(top_level_names),
        top_level_names[0],
        top_level_names.len() - 1
    )
}

fn looks_like_text_file(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    [
        ".txt",
        ".md",
        ".markdown",
        ".json",
        ".yaml",
        ".yml",
        ".toml",
        ".ini",
        ".csv",
        ".log",
        ".xml",
        ".html",
        ".css",
        ".js",
        ".ts",
        ".tsx",
        ".jsx",
        ".rs",
        ".py",
        ".java",
        ".c",
        ".cpp",
        ".h",
        ".hpp",
        ".go",
        ".sh",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

fn text_preview_snippet(value: &str, limit: usize) -> String {
    let trimmed = value.trim();
    let mut chars = trimmed.chars();
    let snippet: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{snippet}…")
    } else {
        snippet
    }
}

fn decode_wire_body(frame: &WireMessage, settings: &Settings) -> anyhow::Result<WireBody> {
    let bytes = decode_wire_payload(frame, settings)?;
    Ok(bincode::deserialize::<WireBody>(&bytes)?)
}

fn decode_wire_payload(frame: &WireMessage, settings: &Settings) -> anyhow::Result<Vec<u8>> {
    if frame.v != WIRE_VERSION {
        return Err(anyhow::anyhow!("unsupported wire version: {}", frame.v));
    }

    let bytes = if frame.encrypted {
        decrypt_bytes(
            frame
                .nonce
                .ok_or_else(|| anyhow::anyhow!("missing nonce"))?,
            &frame.body,
            &derive_key(&effective_secret(settings)),
        )?
    } else {
        if settings.security.encryption_enabled {
            return Err(anyhow::anyhow!(
                "received plain frame but encryption enabled"
            ));
        }
        frame.body.clone()
    };

    Ok(bytes)
}

fn effective_secret(settings: &Settings) -> String {
    settings.sync.shared_code.trim().to_string()
}

fn derive_key(secret: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let out = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out[..32]);
    key
}

fn encrypt_bytes(plain: &[u8], key: &[u8; 32]) -> anyhow::Result<([u8; 12], Vec<u8>)> {
    let cipher = Aes256GcmSiv::new_from_slice(key)?;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = AesNonce::from_slice(&nonce_bytes);
    let encrypted = cipher
        .encrypt(nonce, plain)
        .map_err(|_| anyhow::anyhow!("encrypt failed"))?;
    Ok((nonce_bytes, encrypted))
}

fn decrypt_bytes(nonce_bytes: [u8; 12], body: &[u8], key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256GcmSiv::new_from_slice(key)?;
    let nonce = AesNonce::from_slice(&nonce_bytes);
    let plain = cipher
        .decrypt(nonce, body)
        .map_err(|_| anyhow::anyhow!("decrypt failed (shared code mismatch?)"))?;
    Ok(plain)
}

fn encrypt_raw_payload_bytes(plain: &[u8], key: &[u8; 32]) -> anyhow::Result<([u8; 12], Vec<u8>)> {
    let cipher = ChaCha20Poly1305::new_from_slice(key)?;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = ChaChaNonce::from_slice(&nonce_bytes);
    let encrypted = cipher
        .encrypt(nonce, plain)
        .map_err(|_| anyhow::anyhow!("encrypt raw payload failed"))?;
    Ok((nonce_bytes, encrypted))
}

fn decrypt_raw_payload_bytes(
    nonce_bytes: [u8; 12],
    body: &[u8],
    key: &[u8; 32],
) -> anyhow::Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new_from_slice(key)?;
    let nonce = ChaChaNonce::from_slice(&nonce_bytes);
    let plain = cipher
        .decrypt(nonce, body)
        .map_err(|_| anyhow::anyhow!("decrypt raw payload failed (shared code mismatch?)"))?;
    Ok(plain)
}

fn should_ignore_local_observation(runtime: &RuntimeInner, item: &ClipboardItem) -> bool {
    let observed_at_ms = now_ms();
    let content_hash = item.content_hash.as_str();
    if shared_fingerprint_seen(runtime, content_hash)
        || inflight_fingerprint_seen(runtime, content_hash)
    {
        remember_local_observation(runtime, content_hash, observed_at_ms);
        return true;
    }

    if recent_applied_hash_seen(runtime, content_hash) {
        remember_local_observation(runtime, content_hash, observed_at_ms);
        return true;
    }

    if let Ok(mut guard) = runtime.last_local_observed.lock() {
        if let Some(previous) = guard.as_mut() {
            if previous.content_hash == content_hash {
                previous.observed_at_ms = observed_at_ms;
                return true;
            }
        }
        *guard = Some(ObservedClipboard {
            content_hash: content_hash.to_string(),
            observed_at_ms,
        });
    }
    false
}

fn remember_local_observation(runtime: &RuntimeInner, content_hash: &str, observed_at_ms: u64) {
    if let Ok(mut guard) = runtime.last_local_observed.lock() {
        *guard = Some(ObservedClipboard {
            content_hash: content_hash.to_string(),
            observed_at_ms,
        });
    }
}

fn should_drop_duplicate_outbound(runtime: &RuntimeInner, item: &ClipboardItem) -> bool {
    shared_fingerprint_seen(runtime, &item.content_hash)
        || inflight_fingerprint_seen(runtime, &item.content_hash)
        || recent_applied_hash_seen(runtime, &item.content_hash)
}

fn shared_fingerprint_seen(runtime: &RuntimeInner, content_hash: &str) -> bool {
    runtime
        .shared_content_fingerprint
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .map(|fingerprint| fingerprint == content_hash)
        .unwrap_or(false)
}

fn inflight_fingerprint_seen(runtime: &RuntimeInner, content_hash: &str) -> bool {
    runtime
        .inflight_content_fingerprints
        .lock()
        .map(|guard| guard.contains(content_hash))
        .unwrap_or(false)
}

fn mark_content_inflight(runtime: &RuntimeInner, content_hash: &str) {
    if let Ok(mut guard) = runtime.inflight_content_fingerprints.lock() {
        guard.insert(content_hash.to_string());
    }
}

fn clear_content_inflight(runtime: &RuntimeInner, content_hash: &str) {
    if let Ok(mut guard) = runtime.inflight_content_fingerprints.lock() {
        guard.remove(content_hash);
    }
}

fn mark_shared_fingerprint(runtime: &RuntimeInner, content_hash: &str) {
    if let Ok(mut guard) = runtime.shared_content_fingerprint.lock() {
        *guard = Some(content_hash.to_string());
    }
}

fn register_ignored_local_hash(runtime: &RuntimeInner, content_hash: &str) {
    if let Ok(mut guard) = runtime.ignored_local_hashes.lock() {
        guard.insert(content_hash.to_string(), Instant::now());
    }
}

fn recent_applied_hash_seen(runtime: &RuntimeInner, content_hash: &str) -> bool {
    runtime
        .ignored_local_hashes
        .lock()
        .ok()
        .and_then(|guard| guard.get(content_hash).copied())
        .map(|seen_at| seen_at.elapsed() < Duration::from_millis(APPLIED_HASH_TTL_MS))
        .unwrap_or(false)
}

fn prune_ignored_local_hashes(runtime: &RuntimeInner) {
    if let Ok(mut guard) = runtime.ignored_local_hashes.lock() {
        guard.retain(|_, seen_at| seen_at.elapsed() < Duration::from_millis(APPLIED_HASH_TTL_MS));
    }
}

fn recent_event_seen(runtime: &RuntimeInner, event_id: &str) -> bool {
    runtime
        .recent_event_ids
        .lock()
        .ok()
        .and_then(|guard| guard.get(event_id).copied())
        .map(|seen_at| seen_at.elapsed() < Duration::from_millis(RECENT_EVENT_TTL_MS))
        .unwrap_or(false)
}

fn register_recent_event(runtime: &RuntimeInner, event_id: &str) {
    if let Ok(mut guard) = runtime.recent_event_ids.lock() {
        guard.insert(event_id.to_string(), Instant::now());
    }
}

fn prune_recent_event_ids(runtime: &RuntimeInner) {
    if let Ok(mut guard) = runtime.recent_event_ids.lock() {
        guard.retain(|_, seen_at| seen_at.elapsed() < Duration::from_millis(RECENT_EVENT_TTL_MS));
    }
}

fn item_marker(item: &ClipboardItem) -> ItemMarker {
    ItemMarker {
        id: item.id.clone(),
        created_at_us: item.created_at_us,
        source_device_id: item.source_device_id.clone(),
    }
}

fn sanitize_file_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn file_stream_marker(meta: &FileStreamStart) -> ItemMarker {
    ItemMarker {
        id: meta.item_id.clone(),
        created_at_us: meta.created_at_us,
        source_device_id: meta.source_device_id.clone(),
    }
}

fn compare_markers(left: &ItemMarker, right: &ItemMarker) -> std::cmp::Ordering {
    left.created_at_us
        .cmp(&right.created_at_us)
        .then_with(|| left.source_device_id.cmp(&right.source_device_id))
        .then_with(|| left.id.cmp(&right.id))
}

fn update_latest_marker(runtime: &RuntimeInner, marker: ItemMarker) -> bool {
    if let Ok(mut guard) = runtime.latest_item.lock() {
        let replace = guard
            .as_ref()
            .map(|current| compare_markers(&marker, current).is_gt())
            .unwrap_or(true);
        if replace {
            *guard = Some(marker);
        }
        return replace;
    }
    false
}

fn update_latest_item(runtime: &RuntimeInner, item: &ClipboardItem) -> bool {
    update_latest_marker(runtime, item_marker(item))
}

fn is_stale_marker(runtime: &RuntimeInner, marker: &ItemMarker) -> bool {
    runtime
        .latest_item
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .map(|current| compare_markers(marker, &current).is_lt())
        .unwrap_or(false)
}

fn prune_stale_queue_entries(runtime: &RuntimeInner) {
    let latest = runtime
        .latest_item
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let Some(latest) = latest else {
        return;
    };

    let mut dropped_outbound = 0usize;
    if let Ok(mut guard) = runtime.outbound_queue.lock() {
        guard.retain(|entry| {
            let keep = !compare_markers(&item_marker(&entry.item), &latest).is_lt();
            if !keep {
                dropped_outbound += 1;
                clear_content_inflight(runtime, &entry.item.content_hash);
            }
            keep
        });
    }

    let mut dropped_inbound = 0usize;
    if let Ok(mut guard) = runtime.inbound_queue.lock() {
        guard.retain(|entry| {
            let keep = !compare_markers(&item_marker(&entry.item), &latest).is_lt();
            if !keep {
                dropped_inbound += 1;
            }
            keep
        });
    }

    if dropped_outbound > 0 || dropped_inbound > 0 {
        push_log(
            runtime,
            "DEBUG",
            &format!(
                "pruned stale queue entries outbound={} inbound={}",
                dropped_outbound, dropped_inbound
            ),
        );
    }
}

fn should_skip_remote_item(runtime: &RuntimeInner, item: &ClipboardItem) -> bool {
    if recent_event_seen(runtime, &item.id) {
        return true;
    }

    let marker = item_marker(item);
    let should_skip = is_stale_marker(runtime, &marker);

    if should_skip {
        return true;
    }

    register_recent_event(runtime, &item.id);
    if update_latest_item(runtime, item) {
        prune_stale_queue_entries(runtime);
    }
    false
}

fn enqueue_outbound_item(runtime: &RuntimeInner, item: ClipboardItem) {
    if should_drop_duplicate_outbound(runtime, &item) {
        remember_local_observation(runtime, &item.content_hash, now_ms());
        push_log(
            runtime,
            "DEBUG",
            &format!(
                "drop duplicate outbound item {} kind={} fingerprint={}",
                item.id,
                item.payload.kind(),
                item.content_hash
            ),
        );
        return;
    }

    if update_latest_item(runtime, &item) {
        prune_stale_queue_entries(runtime);
    }
    let item_id = item.id.clone();
    let kind = item.payload.kind();
    let size_bytes = item.size_bytes;
    mark_content_inflight(runtime, &item.content_hash);
    push_queue_entry(&runtime.outbound_queue, new_queue_entry(item));
    push_log(
        runtime,
        "DEBUG",
        &format!("queued outbound item {item_id} kind={kind} size_bytes={size_bytes}"),
    );
}

fn enqueue_inbound_item(
    runtime: &RuntimeInner,
    item: ClipboardItem,
    transfer_id: &str,
    peer: &str,
) {
    if update_latest_item(runtime, &item) {
        prune_stale_queue_entries(runtime);
    }
    let item_id = item.id.clone();
    let source = item.source_device_id.clone();
    let kind = item.payload.kind();
    let size_bytes = item.size_bytes;
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.peer = peer.to_string();
            entry.item_kind = kind.to_string();
            entry.item_label = payload_label(&item.payload);
            entry.item_summary = payload_summary(&item.payload);
            entry.item_id = item_id.clone();
            entry.transferred_bytes = size_bytes;
            entry.total_bytes = size_bytes;
            entry.percent = 100;
            entry.status = "queued".to_string();
            entry.updated_at_ms = now_ms();
            entry.error = None;
        }
    }
    push_queue_entry(&runtime.inbound_queue, new_queue_entry(item));
    push_log(
        runtime,
        "DEBUG",
        &format!("queued inbound item {item_id} kind={kind} size_bytes={size_bytes} from {source}"),
    );
}

fn set_error(runtime: &RuntimeInner, message: String) {
    if let Ok(mut guard) = runtime.last_error.lock() {
        *guard = Some(message.clone());
    }
    push_log(runtime, "ERROR", &message);
}

fn push_log(runtime: &RuntimeInner, level: &str, message: &str) {
    append_runtime_log_file(level, message);
    if let Ok(mut guard) = runtime.logs.lock() {
        guard.push(RuntimeLog {
            ts_ms: now_ms(),
            level: level.to_string(),
            message: message.to_string(),
        });
        if guard.len() > LOG_LIMIT {
            let drain_size = guard.len() - LOG_LIMIT;
            guard.drain(0..drain_size);
        }
    }
}

fn upsert_transfer(runtime: &RuntimeInner, transfer: TransferProgress) {
    prune_transfers(runtime);
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(existing) = guard.iter_mut().find(|entry| entry.id == transfer.id) {
            *existing = transfer;
        } else {
            guard.push(transfer);
        }
        guard.sort_by(|left, right| right.updated_at_ms.cmp(&left.updated_at_ms));
        if guard.len() > TRANSFER_HISTORY_LIMIT {
            guard.truncate(TRANSFER_HISTORY_LIMIT);
        }
    }
}

fn update_transfer_progress(
    runtime: &RuntimeInner,
    transfer_id: &str,
    transferred_bytes: u64,
    total_bytes: u64,
) {
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.transferred_bytes = transferred_bytes.min(total_bytes);
            entry.total_bytes = total_bytes;
            entry.percent = percent_for(entry.transferred_bytes, entry.total_bytes);
            entry.updated_at_ms = now_ms();
        }
    }
}

fn transfer_should_abort(runtime: &RuntimeInner, transfer_id: &str) -> bool {
    runtime
        .transfers
        .lock()
        .ok()
        .and_then(|guard| guard.iter().find(|entry| entry.id == transfer_id).cloned())
        .map(|entry| entry.status == "failed")
        .unwrap_or(false)
}

fn update_transfer_metadata(
    runtime: &RuntimeInner,
    transfer_id: &str,
    item_kind: &str,
    item_label: &str,
    item_summary: &str,
    item_id: &str,
    status: &str,
) {
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.item_kind = item_kind.to_string();
            entry.item_label = item_label.to_string();
            entry.item_summary = item_summary.to_string();
            entry.item_id = item_id.to_string();
            entry.status = status.to_string();
            entry.updated_at_ms = now_ms();
        }
    }
}

fn update_transfer_status(
    runtime: &RuntimeInner,
    transfer_id: &str,
    status: &str,
    error: Option<String>,
) {
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.status = status.to_string();
            entry.error = error;
            entry.updated_at_ms = now_ms();
        }
    }
}

fn mark_transfer_completed(runtime: &RuntimeInner, transfer_id: &str) {
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.transferred_bytes = entry.total_bytes;
            entry.percent = 100;
            entry.status = "completed".to_string();
            entry.error = None;
            entry.updated_at_ms = now_ms();
        }
    }
}

fn mark_transfer_failed(runtime: &RuntimeInner, transfer_id: &str, error: String) {
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.status = "failed".to_string();
            entry.error = Some(error);
            entry.updated_at_ms = now_ms();
        }
    }
}

fn prune_transfers(runtime: &RuntimeInner) {
    let threshold = now_ms().saturating_sub(TRANSFER_RETENTION_MS);
    if let Ok(mut guard) = runtime.transfers.lock() {
        guard.retain(|entry| {
            matches!(
                entry.status.as_str(),
                "sending" | "receiving" | "queued" | "applying" | "retrying"
            ) || entry.updated_at_ms >= threshold
        });
    }
}

fn has_active_transfers(runtime: &RuntimeInner) -> bool {
    runtime
        .transfers
        .lock()
        .map(|guard| {
            guard.iter().any(|entry| {
                matches!(
                    entry.status.as_str(),
                    "sending" | "receiving" | "queued" | "applying" | "retrying"
                )
            })
        })
        .unwrap_or(false)
}

fn find_receive_transfer_id(runtime: &RuntimeInner, item_id: &str) -> Option<String> {
    runtime.transfers.lock().ok().and_then(|guard| {
        guard
            .iter()
            .find(|entry| entry.direction == "receive" && entry.item_id == item_id)
            .map(|entry| entry.id.clone())
    })
}

fn canonical_receive_transfer_id(item: &ClipboardItem) -> String {
    format!("recv:{}:{}", item.source_device_id, item.id)
}

fn percent_for(transferred_bytes: u64, total_bytes: u64) -> u8 {
    if total_bytes == 0 {
        return 0;
    }
    ((transferred_bytes.saturating_mul(100) / total_bytes).min(100)) as u8
}

fn append_runtime_log_file(level: &str, message: &str) {
    let dir = std::env::temp_dir().join("lan-clipboard");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("runtime.log");
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(file) => file,
        Err(_) => return,
    };
    let _ = writeln!(
        file,
        "{} [{}] [pid={}] {}",
        now_ms(),
        level,
        std::process::id(),
        message
    );
}

fn build_service_info(config: &PresenceConfig) -> anyhow::Result<ServiceInfo> {
    let local_ip = resolve_local_ip_override(&config.local_ip)?.unwrap_or(pick_local_ip()?);
    let host_name = format!("lan-clipboard-{}.local.", config.device_id);
    let properties = [
        ("device_id", config.device_id.as_str()),
        ("shared_code", config.shared_code.as_str()),
        ("device_name", config.device_name.as_str()),
    ];
    Ok(ServiceInfo::new(
        SERVICE_TYPE,
        config.device_id.as_str(),
        &host_name,
        local_ip.to_string(),
        config.listen_port,
        &properties[..],
    )?)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_micros() as u64)
        .unwrap_or(0)
}

pub fn new_device_id() -> String {
    Uuid::new_v4().to_string()
}

pub fn discover_devices(
    device_id: &str,
    shared_code: &str,
    selected_local_ip: Option<&str>,
    timeout_ms: u64,
) -> anyhow::Result<Vec<DiscoveredDevice>> {
    let mdns = ServiceDaemon::new()?;
    let receiver = mdns.browse(SERVICE_TYPE)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut devices = Vec::new();
    let mut seen = HashSet::new();
    let selected_ipv4 = parse_selected_ipv4(selected_local_ip)?;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = match receiver.recv_timeout(remaining) {
            Ok(value) => value,
            Err(_) => break,
        };
        if let ServiceEvent::ServiceResolved(info) = event {
            if let Some(found) = info_to_device(&info, device_id, shared_code, selected_ipv4) {
                let dedupe_key = format!("{}:{}:{}", found.device_id, found.addr, found.port);
                if seen.insert(dedupe_key) {
                    devices.push(found);
                }
            }
        }
    }

    shutdown_discovery_daemon(&mdns, receiver);
    Ok(filter_reachable_discovered_devices(
        devices,
        DISCOVERY_REACHABILITY_TIMEOUT_MS,
    ))
}

fn filter_reachable_discovered_devices(
    devices: Vec<DiscoveredDevice>,
    timeout_ms: u64,
) -> Vec<DiscoveredDevice> {
    let handles = devices
        .into_iter()
        .map(|device| {
            std::thread::spawn(move || {
                if discovered_device_is_reachable(&device, timeout_ms) {
                    Some(device)
                } else {
                    None
                }
            })
        })
        .collect::<Vec<_>>();

    handles
        .into_iter()
        .filter_map(|handle| handle.join().ok().flatten())
        .collect()
}

fn discovered_device_is_reachable(device: &DiscoveredDevice, timeout_ms: u64) -> bool {
    let Ok(ip) = device.addr.parse::<IpAddr>() else {
        return false;
    };
    let socket_addr = SocketAddr::new(ip, device.port);
    TcpStream::connect_timeout(&socket_addr, Duration::from_millis(timeout_ms)).is_ok()
}

pub fn list_network_interfaces() -> Vec<NetworkInterfaceOption> {
    let Ok(all) = local_ip_address::list_afinet_netifas() else {
        return Vec::new();
    };

    let mut interfaces: Vec<NetworkInterfaceOption> = all
        .into_iter()
        .filter_map(|(name, ip)| {
            let IpAddr::V4(ipv4) = ip else {
                return None;
            };
            if !is_usable_ipv4(ipv4) {
                return None;
            }
            let label = if is_private_lan_ipv4(ipv4) {
                format!("{name} ({ipv4})")
            } else {
                format!("{name} ({ipv4}, 非局域网优先)")
            };
            Some(NetworkInterfaceOption {
                name,
                ip: ipv4.to_string(),
                label,
            })
        })
        .collect();

    interfaces.sort_by(|left, right| {
        let left_ip: std::net::Ipv4Addr =
            left.ip.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
        let right_ip: std::net::Ipv4Addr =
            right.ip.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
        is_private_lan_ipv4(right_ip)
            .cmp(&is_private_lan_ipv4(left_ip))
            .then_with(|| {
                is_likely_virtual_interface(&left.name)
                    .cmp(&is_likely_virtual_interface(&right.name))
            })
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.ip.cmp(&right.ip))
    });
    interfaces.dedup_by(|left, right| left.ip == right.ip);
    interfaces
}

fn shutdown_discovery_daemon(mdns: &ServiceDaemon, receiver: mdns_sd::Receiver<ServiceEvent>) {
    let _ = mdns.stop_browse(SERVICE_TYPE);
    while let Ok(event) = receiver.recv_timeout(Duration::from_millis(100)) {
        if matches!(event, ServiceEvent::SearchStopped(_)) {
            break;
        }
    }
    if let Ok(status_rx) = mdns.shutdown() {
        let _ = status_rx.recv_timeout(Duration::from_millis(300));
    }
}

fn info_to_device(
    info: &ServiceInfo,
    self_device_id: &str,
    shared_code: &str,
    selected_local_ip: Option<std::net::Ipv4Addr>,
) -> Option<DiscoveredDevice> {
    let device_id = info.get_fullname().split('.').next()?.to_string();
    if device_id == self_device_id {
        return None;
    }

    let found_shared_code = info
        .get_properties()
        .get_property_val_str("shared_code")
        .unwrap_or("");
    if found_shared_code != shared_code {
        return None;
    }

    let addr = pick_ipv4(info.get_addresses())?;
    if let Some(selected_ipv4) = selected_local_ip {
        let addr_ipv4: std::net::Ipv4Addr = addr.parse().ok()?;
        if !is_same_subnet(selected_ipv4, addr_ipv4) {
            return None;
        }
    }
    let device_name = info
        .get_properties()
        .get_property_val_str("device_name")
        .unwrap_or("局域网设备")
        .to_string();

    Some(DiscoveredDevice {
        device_id,
        device_name,
        addr,
        port: info.get_port(),
    })
}

fn pick_ipv4(addresses: &HashSet<IpAddr>) -> Option<String> {
    pick_best_ipv4(addresses.iter().filter_map(|address| match address {
        IpAddr::V4(ipv4) => Some(*ipv4),
        IpAddr::V6(_) => None,
    }))
    .map(|ip| ip.to_string())
}

fn pick_local_ip() -> anyhow::Result<IpAddr> {
    let all = local_ip_address::list_afinet_netifas()?;
    let candidates = all
        .into_iter()
        .filter_map(|(name, ip)| match ip {
            IpAddr::V4(v4) => Some((name, v4)),
            IpAddr::V6(_) => None,
        })
        .filter(|(name, ipv4)| is_usable_ipv4(*ipv4) && !is_likely_virtual_interface(name));
    pick_best_ipv4(candidates.map(|(_, ip)| ip))
        .map(IpAddr::V4)
        .ok_or_else(|| anyhow::anyhow!("no usable local ipv4 address found"))
}

fn is_likely_virtual_interface(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    #[cfg(target_os = "windows")]
    {
        return lower.contains("vethernet")
            || lower.contains("hyper-v")
            || lower.contains("virtual")
            || lower.contains("vmware")
            || lower.contains("virtualbox")
            || lower.contains("wsl")
            || lower.contains("tailscale")
            || lower.contains("hamachi")
            || lower.contains("tap")
            || lower.contains("tun")
            || lower.contains("loopback");
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = lower;
        false
    }
}

fn selected_or_active_local_ip(
    settings: &Settings,
    selected_local_ip: Option<&str>,
    active_local_ip: Option<String>,
) -> Option<String> {
    resolve_local_ip_override(selected_local_ip.unwrap_or(settings.sync.local_ip.as_str()))
        .ok()
        .flatten()
        .map(|ip| ip.to_string())
        .or(active_local_ip)
        .or_else(|| pick_local_ip().ok().map(|ip| ip.to_string()))
}

fn parse_selected_ipv4(
    selected_local_ip: Option<&str>,
) -> anyhow::Result<Option<std::net::Ipv4Addr>> {
    match resolve_local_ip_override(selected_local_ip.unwrap_or_default())? {
        Some(IpAddr::V4(ipv4)) => Ok(Some(ipv4)),
        Some(IpAddr::V6(_)) => Ok(None),
        None => Ok(None),
    }
}

fn resolve_local_ip_override(value: &str) -> anyhow::Result<Option<IpAddr>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let parsed: IpAddr = trimmed
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid selected local ip: {trimmed}"))?;
    match parsed {
        IpAddr::V4(ipv4) if is_usable_ipv4(ipv4) => {
            if is_ipv4_assigned_locally(ipv4) {
                Ok(Some(IpAddr::V4(ipv4)))
            } else {
                Ok(None)
            }
        }
        IpAddr::V4(_) => Err(anyhow::anyhow!(
            "selected local ip is not usable: {trimmed}"
        )),
        IpAddr::V6(_) => Err(anyhow::anyhow!("selected local ip must be ipv4: {trimmed}")),
    }
}

fn is_ipv4_assigned_locally(ipv4: std::net::Ipv4Addr) -> bool {
    let Ok(all) = local_ip_address::list_afinet_netifas() else {
        return true;
    };
    all.into_iter().any(|(_, ip)| match ip {
        IpAddr::V4(found) => found == ipv4,
        IpAddr::V6(_) => false,
    })
}

fn pick_best_ipv4<I>(candidates: I) -> Option<std::net::Ipv4Addr>
where
    I: IntoIterator<Item = std::net::Ipv4Addr>,
{
    let mut preferred = None;
    let mut fallback = None;

    for ipv4 in candidates {
        if !is_usable_ipv4(ipv4) {
            continue;
        }
        if preferred.is_none() && is_private_lan_ipv4(ipv4) {
            preferred = Some(ipv4);
        } else if fallback.is_none() {
            fallback = Some(ipv4);
        }
    }

    preferred.or(fallback)
}

fn remember_active_local_ip(runtime: &RuntimeInner, ip: IpAddr) {
    let IpAddr::V4(ipv4) = ip else {
        return;
    };
    if !is_usable_ipv4(ipv4) {
        return;
    }
    if let Ok(mut guard) = runtime.active_local_ip.lock() {
        let should_replace = match guard.as_deref().and_then(|value| value.parse().ok()) {
            Some(existing) => !is_private_lan_ipv4(existing) && is_private_lan_ipv4(ipv4),
            None => true,
        };
        if should_replace {
            *guard = Some(ipv4.to_string());
        }
    }
}

fn is_usable_ipv4(ipv4: std::net::Ipv4Addr) -> bool {
    !ipv4.is_loopback()
        && !ipv4.is_link_local()
        && !ipv4.is_unspecified()
        && !ipv4.is_broadcast()
        && !is_benchmark_ipv4(ipv4)
        && !is_multicast_ipv4(ipv4)
}

fn is_private_lan_ipv4(ipv4: std::net::Ipv4Addr) -> bool {
    let [a, b, _, _] = ipv4.octets();
    a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
}

fn is_benchmark_ipv4(ipv4: std::net::Ipv4Addr) -> bool {
    let [a, b, _, _] = ipv4.octets();
    a == 198 && (b == 18 || b == 19)
}

fn is_multicast_ipv4(ipv4: std::net::Ipv4Addr) -> bool {
    let [a, _, _, _] = ipv4.octets();
    (224..=239).contains(&a)
}

fn is_same_subnet(left: std::net::Ipv4Addr, right: std::net::Ipv4Addr) -> bool {
    let left = left.octets();
    let right = right.octets();
    left[0] == right[0] && left[1] == right[1] && left[2] == right[2]
}

fn filter_devices_for_local_ip(
    devices: Vec<DiscoveredDevice>,
    selected_local_ip: Option<&str>,
) -> Vec<DiscoveredDevice> {
    let Ok(Some(selected_ipv4)) = parse_selected_ipv4(selected_local_ip) else {
        return devices;
    };

    devices
        .into_iter()
        .filter(|device| {
            device
                .addr
                .parse::<std::net::Ipv4Addr>()
                .map(|ipv4| is_same_subnet(selected_ipv4, ipv4))
                .unwrap_or(false)
        })
        .collect()
}

fn udp_broadcast_targets(selected_local_ip: &str) -> Vec<SocketAddr> {
    let mut targets = HashSet::new();
    targets.insert(SocketAddr::from(([255, 255, 255, 255], UDP_DISCOVERY_PORT)));

    if let Ok(Some(IpAddr::V4(ipv4))) = resolve_local_ip_override(selected_local_ip) {
        let [a, b, c, _] = ipv4.octets();
        targets.insert(SocketAddr::from(([a, b, c, 255], UDP_DISCOVERY_PORT)));
        return targets.into_iter().collect();
    }

    if let Ok(all) = local_ip_address::list_afinet_netifas() {
        for (_, ip) in all {
            if let IpAddr::V4(ipv4) = ip {
                if !is_usable_ipv4(ipv4) {
                    continue;
                }
                let [a, b, c, _] = ipv4.octets();
                targets.insert(SocketAddr::from(([a, b, c, 255], UDP_DISCOVERY_PORT)));
            }
        }
    }

    targets.into_iter().collect()
}

fn local_device_name(device_id: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        for args in [
            &["--get", "ComputerName"][..],
            &["--get", "LocalHostName"][..],
        ] {
            if let Ok(output) = std::process::Command::new("scutil").args(args).output() {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !name.is_empty() {
                    return name;
                }
            }
        }
    }

    if let Ok(output) = std::process::Command::new("hostname").output() {
        if output.status.success() {
            let hostname = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !hostname.is_empty() {
                return hostname;
            }
        }
    }

    device_id.to_string()
}
