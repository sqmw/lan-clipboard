use crate::clipboard;
use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::Settings;
use aes_gcm_siv::aead::{Aead, KeyInit};
use aes_gcm_siv::{Aes256GcmSiv, Nonce};
use clipboard_master::{CallbackResult, ClipboardHandler, Master};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SERVICE_TYPE: &str = "_lan-clipboard._tcp.local.";
const LOG_LIMIT: usize = 800;
const DISCOVERY_REFRESH_MS: u64 = 1_000;
const DISCOVERY_TIMEOUT_MS: u64 = 900;
const STATUS_DISCOVERY_TIMEOUT_MS: u64 = 350;
const APPLY_MUTE_MS: u64 = 1_200;
const APPLY_RICH_TEXT_MUTE_MS: u64 = 2_500;
const APPLY_FILE_MUTE_MS: u64 = 5_000;
const DISCOVERY_MEMBER_TTL_MS: u64 = 30_000;
const UDP_DISCOVERY_PORT: u16 = 32911;
const UDP_ANNOUNCE_MS: u64 = 500;
const DISCOVERY_APP: &str = "lan-clipboard";
const LOCAL_EVENT_DEBOUNCE_MS: u64 = 250;
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
const WIRE_VERSION: u8 = 2;
const MAX_WIRE_FRAME_BYTES: usize = 512 * 1024 * 1024;
const TRANSFER_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const TRANSFER_HISTORY_LIMIT: usize = 24;
const TRANSFER_RETENTION_MS: u64 = 15_000;

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

#[derive(Debug, Clone, Serialize)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileStreamStart {
    item_id: String,
    content_hash: String,
    created_at_ms: u64,
    source_device_id: String,
    size_bytes: u64,
    top_level_names: Vec<String>,
}

struct IncomingFileStream {
    meta: FileStreamStart,
    archive_bytes: Vec<u8>,
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
    suppress_until_ms: AtomicU64,
    outbound_queue: Mutex<VecDeque<QueueEntry>>,
    inbound_queue: Mutex<VecDeque<QueueEntry>>,
    latest_item: Mutex<Option<ItemMarker>>,
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
            suppress_until_ms: AtomicU64::new(0),
            outbound_queue: Mutex::new(VecDeque::new()),
            inbound_queue: Mutex::new(VecDeque::new()),
            latest_item: Mutex::new(None),
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
    created_at_ms: u64,
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

struct ClipboardWatchHandler {
    runtime: Arc<RuntimeInner>,
    limits: crate::settings::SizeLimits,
    device_id: String,
    poll_interval: Duration,
}

impl SyncEngine {
    pub fn status(&self, settings: &Settings) -> RuntimeStatus {
        if self.inner.running.load(Ordering::SeqCst) && cached_member_signal_count(&self.inner) == 0
        {
            refresh_discovered_devices(
                &self.inner,
                settings,
                &settings.sync_device_id(),
                STATUS_DISCOVERY_TIMEOUT_MS,
            );
        }
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
        let peer_count = current_member_count(&self.inner, settings);
        RuntimeStatus {
            running: self.inner.running.load(Ordering::SeqCst),
            device_id: settings.sync_device_id(),
            device_name: local_device_name(),
            local_ip: selected_or_active_local_ip(settings, active_local_ip),
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

    pub fn devices(&self) -> Vec<DiscoveredDevice> {
        prune_stale_discovered_devices(&self.inner);
        self.inner
            .discovered_devices
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn merge_discovered_devices(&self, devices: Vec<DiscoveredDevice>) {
        merge_discovered_devices(&self.inner, devices);
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
                "sync starting shared_code={} port={} static_peers={}",
                settings.sync.shared_code,
                settings.sync.listen_port,
                settings.sync.peers.len()
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
            device_id,
            device_name: local_device_name(),
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

        if self
            .inner
            .signature
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .as_deref()
            == Some(signature.as_str())
        {
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

impl ClipboardHandler for ClipboardWatchHandler {
    fn on_clipboard_change(&mut self) -> CallbackResult {
        if is_clipboard_suppressed(&self.runtime) {
            return CallbackResult::Next;
        }

        let payload = match clipboard::read_snapshot(&self.limits) {
            Ok(payload) => payload,
            Err(clipboard::ClipboardError::Unsupported) => return CallbackResult::Next,
            Err(error) => {
                set_error(
                    &self.runtime,
                    format!("clipboard watcher read failed: {error}"),
                );
                return CallbackResult::Next;
            }
        };

        let Some(item) = build_item(&payload, &self.device_id) else {
            return CallbackResult::Next;
        };

        if should_ignore_local_observation(&self.runtime, &item.content_hash) {
            return CallbackResult::Next;
        }

        push_log(
            &self.runtime,
            "INFO",
            &format!(
                "detected local clipboard kind={} size_bytes={} item={}",
                item.payload.kind(),
                item.size_bytes,
                item.id
            ),
        );
        register_recent_event(&self.runtime, &item.id);
        update_latest_item(&self.runtime, &item);
        enqueue_outbound_item(&self.runtime, item);
        CallbackResult::Next
    }

    fn sleep_interval(&self) -> Duration {
        self.poll_interval
    }
}

fn run_presence_loop(runtime: Arc<PresenceInner>, config: PresenceConfig) {
    let mdns = match ServiceDaemon::new() {
        Ok(value) => value,
        Err(_) => return,
    };
    let service = match build_service_info(&config) {
        Ok(value) => value,
        Err(_) => {
            let _ = mdns.shutdown();
            return;
        }
    };

    if mdns.register(service).is_err() {
        let _ = mdns.shutdown();
        return;
    }

    while !runtime.stop_flag.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(400));
    }

    if let Ok(status_rx) = mdns.shutdown() {
        let _ = status_rx.recv_timeout(Duration::from_millis(300));
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
    let device_name = local_device_name();
    let watcher_runtime = Arc::clone(&runtime);
    let watcher_stop_runtime = Arc::clone(&runtime);
    let watcher_limits = settings.limits.clone();
    let watcher_device_id = device_id.clone();
    let watcher_poll_interval = Duration::from_millis(CLIPBOARD_WATCH_INTERVAL_MS);
    let watcher = std::thread::Builder::new()
        .name("lan-clipboard-watch".to_string())
        .spawn(move || {
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
                refresh_discovered_devices(&runtime, &settings, &device_id, DISCOVERY_TIMEOUT_MS);
            }
            last_discovery = Instant::now();
        }

        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_nodelay(true);
                let _ = stream
                    .set_read_timeout(Some(write_timeout_for_payload(MAX_WIRE_FRAME_BYTES as u64)));
                if let Err(error) = handle_incoming(&runtime, &settings, stream, &device_id) {
                    set_error(&runtime, format!("incoming handler failed: {error}"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => set_error(&runtime, format!("listener accept failed: {error}")),
        }

        process_inbound_queue(&runtime, &settings);
        process_outbound_queue(&runtime, &settings);
        prune_ignored_local_hashes(&runtime);
        prune_recent_event_ids(&runtime);

        std::thread::sleep(Duration::from_millis(10));
    }

    if let Some(handle) = watcher {
        let _ = handle.join();
    }
    runtime.running.store(false, Ordering::SeqCst);
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
    timeout_ms: u64,
) {
    match discover_devices(device_id, &settings.sync.shared_code, timeout_ms) {
        Ok(devices) => {
            merge_discovered_devices(runtime, devices);
            prune_stale_discovered_devices(runtime);
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
    while let Some((transfer_id, frame_bytes)) = read_wire_frame(
        runtime,
        &mut stream,
        remote_addr.as_deref().unwrap_or("未知来源"),
    )? {
        let frame = bincode::deserialize::<WireMessage>(&frame_bytes)?;
        let body = decode_wire_body(&frame, settings)?;
        match body {
            WireBody::ClipboardItem(item) => {
                handle_incoming_item(
                    runtime,
                    &transfer_id,
                    remote_addr.as_deref().unwrap_or("未知来源"),
                    item,
                    device_id,
                );
            }
            WireBody::FileStreamStart(meta) => {
                let canonical_transfer_id =
                    format!("recv:{}:{}", meta.source_device_id, meta.item_id);
                adopt_receive_transfer(
                    runtime,
                    &transfer_id,
                    &canonical_transfer_id,
                    remote_addr.as_deref().unwrap_or("未知来源"),
                );
                update_transfer_metadata(
                    runtime,
                    &canonical_transfer_id,
                    "file_bundle",
                    &file_stream_label(&meta.top_level_names),
                    &file_stream_summary(&meta.top_level_names),
                    &meta.item_id,
                    "receiving",
                );
                incoming_files.insert(
                    meta.item_id.clone(),
                    IncomingFileStream {
                        meta,
                        archive_bytes: Vec::new(),
                        peer: remote_addr.as_deref().unwrap_or("未知来源").to_string(),
                    },
                );
            }
            WireBody::FileStreamChunk { item_id, bytes } => {
                if let Some(file_stream) = incoming_files.get_mut(&item_id) {
                    let canonical_transfer_id =
                        format!("recv:{}:{}", file_stream.meta.source_device_id, item_id);
                    adopt_receive_transfer(
                        runtime,
                        &transfer_id,
                        &canonical_transfer_id,
                        remote_addr.as_deref().unwrap_or("未知来源"),
                    );
                    file_stream.archive_bytes.extend_from_slice(&bytes);
                    update_transfer_progress(
                        runtime,
                        &canonical_transfer_id,
                        file_stream.archive_bytes.len() as u64,
                        file_stream.meta.size_bytes,
                    );
                }
            }
            WireBody::FileStreamEnd { item_id } => {
                if let Some(file_stream) = incoming_files.remove(&item_id) {
                    let canonical_transfer_id =
                        format!("recv:{}:{}", file_stream.meta.source_device_id, item_id);
                    adopt_receive_transfer(
                        runtime,
                        &transfer_id,
                        &canonical_transfer_id,
                        remote_addr.as_deref().unwrap_or("未知来源"),
                    );
                    let item = ClipboardItem {
                        id: file_stream.meta.item_id,
                        content_hash: file_stream.meta.content_hash,
                        created_at_ms: file_stream.meta.created_at_ms,
                        source_device_id: file_stream.meta.source_device_id,
                        size_bytes: file_stream.archive_bytes.len() as u64,
                        payload: ClipboardPayload::FileBundle {
                            archive_bytes: file_stream.archive_bytes,
                            top_level_names: file_stream.meta.top_level_names,
                        },
                    };
                    let canonical_transfer_id = canonical_receive_transfer_id(&item);
                    update_transfer_metadata(
                        runtime,
                        &canonical_transfer_id,
                        item.payload.kind(),
                        &payload_label(&item.payload),
                        &payload_summary(&item.payload),
                        &item.id,
                        "received",
                    );
                    handle_incoming_item(
                        runtime,
                        &canonical_transfer_id,
                        &file_stream.peer,
                        item,
                        device_id,
                    );
                }
            }
        }
    }
    Ok(())
}

fn handle_incoming_item(
    runtime: &RuntimeInner,
    transfer_id: &str,
    peer: &str,
    item: ClipboardItem,
    device_id: &str,
) {
    let canonical_transfer_id = canonical_receive_transfer_id(&item);
    if transfer_id != canonical_transfer_id {
        adopt_receive_transfer(runtime, transfer_id, &canonical_transfer_id, peer);
    }
    update_transfer_metadata(
        runtime,
        &canonical_transfer_id,
        item.payload.kind(),
        &payload_label(&item.payload),
        &payload_summary(&item.payload),
        &item.id,
        "received",
    );

    if item.source_device_id == device_id {
        return;
    }
    mark_known_member(runtime, "device", &item.source_device_id);
    if should_skip_remote_item(runtime, &item) {
        return;
    }
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

fn read_wire_frame(
    runtime: &RuntimeInner,
    stream: &mut TcpStream,
    peer: &str,
) -> anyhow::Result<Option<(String, Vec<u8>)>> {
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

    let transfer_id = format!("recv:{}:{}", peer, now_ms());
    let total_bytes = frame_len as u64 + 4;
    upsert_transfer(
        runtime,
        TransferProgress {
            id: transfer_id.clone(),
            direction: "receive".to_string(),
            peer: peer.to_string(),
            item_kind: "识别中".to_string(),
            item_label: "识别中".to_string(),
            item_summary: "正在解析内容".to_string(),
            item_id: String::new(),
            transferred_bytes: 4,
            total_bytes,
            percent: percent_for(4, total_bytes),
            status: "receiving".to_string(),
            updated_at_ms: now_ms(),
            error: None,
        },
    );

    let mut frame = vec![0u8; frame_len];
    read_exact_with_progress(runtime, stream, &mut frame, &transfer_id, total_bytes, 4)?;
    Ok(Some((transfer_id, frame)))
}

fn send_to_all_peers(runtime: &RuntimeInner, settings: &Settings, item: &ClipboardItem) -> usize {
    if matches!(item.payload, ClipboardPayload::FileList { .. }) {
        return send_file_list_to_all_peers(runtime, settings, item);
    }

    let payload = match encode_wire_message(item, settings) {
        Ok(payload) => payload,
        Err(error) => {
            set_error(runtime, format!("encode payload failed: {error}"));
            return 0;
        }
    };

    let mut delivered = 0usize;
    for peer in collect_peer_targets(runtime, settings) {
        let connect_timeout = Duration::from_millis(CONNECT_TIMEOUT_MS);
        let write_timeout = write_timeout_for_payload(payload.len() as u64);
        let stream = TcpStream::connect_timeout(
            &match peer.parse() {
                Ok(socket_addr) => socket_addr,
                Err(_) => {
                    push_log(runtime, "WARN", &format!("skip bad peer addr: {}", peer));
                    continue;
                }
            },
            connect_timeout,
        );

        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                push_log(
                    runtime,
                    "DEBUG",
                    &format!("connect peer failed peer={peer} error={error}"),
                );
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        if let Ok(local_addr) = stream.local_addr() {
            remember_active_local_ip(runtime, local_addr.ip());
        }
        let _ = stream.set_write_timeout(Some(write_timeout));
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
        if write_all_with_progress(runtime, &mut stream, &payload, &transfer_id).is_ok() {
            mark_transfer_completed(runtime, &transfer_id);
            mark_known_member(runtime, "addr", &peer);
            delivered += 1;
        } else {
            mark_transfer_failed(runtime, &transfer_id, "发送失败".to_string());
            push_log(
                runtime,
                "DEBUG",
                &format!(
                    "write peer failed peer={peer} payload_bytes={} timeout_ms={}",
                    payload.len(),
                    write_timeout.as_millis()
                ),
            );
        }
    }

    if delivered > 0 {
        push_log(
            runtime,
            "DEBUG",
            &format!("broadcast delivered={}", delivered),
        );
    }
    delivered
}

fn send_file_list_to_all_peers(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
) -> usize {
    let mut delivered = 0usize;
    for peer in collect_peer_targets(runtime, settings) {
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
                total_bytes: item.size_bytes,
                percent: 0,
                status: "sending".to_string(),
                updated_at_ms: now_ms(),
                error: None,
            },
        );

        match send_file_list_to_peer(runtime, settings, item, &peer, &transfer_id) {
            Ok(()) => {
                mark_transfer_completed(runtime, &transfer_id);
                mark_known_member(runtime, "addr", &peer);
                delivered += 1;
            }
            Err(error) => {
                mark_transfer_failed(runtime, &transfer_id, error.to_string());
                push_log(
                    runtime,
                    "DEBUG",
                    &format!("stream file peer failed peer={peer} error={error}"),
                );
            }
        }
    }
    delivered
}

fn send_file_list_to_peer(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
    peer: &str,
    transfer_id: &str,
) -> anyhow::Result<()> {
    let ClipboardPayload::FileList { paths, .. } = &item.payload else {
        return Err(anyhow::anyhow!("expected file list payload"));
    };
    let socket_addr: SocketAddr = peer.parse()?;
    let mut stream =
        TcpStream::connect_timeout(&socket_addr, Duration::from_millis(CONNECT_TIMEOUT_MS))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(write_timeout_for_payload(item.size_bytes)));
    if let Ok(local_addr) = stream.local_addr() {
        remember_active_local_ip(runtime, local_addr.ip());
    }

    let start = WireBody::FileStreamStart(FileStreamStart {
        item_id: item.id.clone(),
        content_hash: item.content_hash.clone(),
        created_at_ms: item.created_at_ms,
        source_device_id: item.source_device_id.clone(),
        size_bytes: item.size_bytes,
        top_level_names: match &item.payload {
            ClipboardPayload::FileList {
                top_level_names, ..
            } => top_level_names.clone(),
            _ => Vec::new(),
        },
    });
    write_wire_body_to_stream(&mut stream, settings, &start)?;

    {
        let mut writer = FileStreamWriter {
            runtime,
            settings,
            stream: &mut stream,
            item_id: item.id.clone(),
            transfer_id: transfer_id.to_string(),
            buffer: Vec::with_capacity(TRANSFER_CHUNK_BYTES),
            sent_archive_bytes: 0,
            total_archive_bytes: item.size_bytes,
        };
        clipboard::stream_file_bundle_archive(paths, &mut writer)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        writer.finish()?;
    }

    write_wire_body_to_stream(
        &mut stream,
        settings,
        &WireBody::FileStreamEnd {
            item_id: item.id.clone(),
        },
    )?;
    Ok(())
}

struct FileStreamWriter<'a> {
    runtime: &'a RuntimeInner,
    settings: &'a Settings,
    stream: &'a mut TcpStream,
    item_id: String,
    transfer_id: String,
    buffer: Vec<u8>,
    sent_archive_bytes: u64,
    total_archive_bytes: u64,
}

impl FileStreamWriter<'_> {
    fn finish(&mut self) -> anyhow::Result<()> {
        self.flush_chunk()
    }

    fn flush_chunk(&mut self) -> anyhow::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.buffer);
        let archive_bytes = bytes.len() as u64;
        write_wire_body_to_stream(
            self.stream,
            self.settings,
            &WireBody::FileStreamChunk {
                item_id: self.item_id.clone(),
                bytes,
            },
        )?;
        self.sent_archive_bytes = self.sent_archive_bytes.saturating_add(archive_bytes);
        update_transfer_progress(
            self.runtime,
            &self.transfer_id,
            self.sent_archive_bytes,
            self.total_archive_bytes,
        );
        Ok(())
    }
}

impl Write for FileStreamWriter<'_> {
    fn write(&mut self, mut input: &[u8]) -> std::io::Result<usize> {
        let original_len = input.len();
        while !input.is_empty() {
            let remaining = TRANSFER_CHUNK_BYTES.saturating_sub(self.buffer.len());
            let take = remaining.min(input.len());
            self.buffer.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.buffer.len() >= TRANSFER_CHUNK_BYTES {
                self.flush_chunk()
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))?;
            }
        }
        Ok(original_len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_chunk()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))
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

fn read_exact_with_progress(
    runtime: &RuntimeInner,
    stream: &mut TcpStream,
    buffer: &mut [u8],
    transfer_id: &str,
    total_bytes: u64,
    initial_bytes: u64,
) -> anyhow::Result<()> {
    let mut offset = 0usize;
    while offset < buffer.len() {
        let end = (offset + TRANSFER_CHUNK_BYTES).min(buffer.len());
        if let Err(error) = stream.read_exact(&mut buffer[offset..end]) {
            mark_transfer_failed(runtime, transfer_id, error.to_string());
            return Err(error.into());
        }
        offset = end;
        let transferred_bytes = initial_bytes + offset as u64;
        update_transfer_progress(runtime, transfer_id, transferred_bytes, total_bytes);
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

fn process_inbound_queue(runtime: &RuntimeInner, settings: &Settings) {
    loop {
        let Some(mut entry) = pop_ready_queue_entry(&runtime.inbound_queue) else {
            break;
        };

        register_ignored_local_hash(runtime, &entry.item.content_hash);
        set_clipboard_suppressed(runtime, apply_mute_duration_ms(&entry.item));
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
            Ok(()) => {
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
}

fn process_outbound_queue(runtime: &RuntimeInner, settings: &Settings) {
    if is_clipboard_suppressed(runtime) {
        return;
    }

    loop {
        let Some(mut entry) = pop_ready_queue_entry(&runtime.outbound_queue) else {
            break;
        };

        let attempted = collect_peer_targets(runtime, settings).len();
        let delivered = send_to_all_peers(runtime, settings, &entry.item);
        if attempted == 0 || delivered < attempted {
            if schedule_retry(&mut entry) {
                push_log(
                    runtime,
                    "DEBUG",
                    &format!(
                        "outbound item {} pending peers delivered={delivered} attempted={attempted} retry={}",
                        entry.item.id, entry.attempts
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
                    entry.item.id
                ),
            );
            continue;
        }

        push_log(
            runtime,
            "DEBUG",
            &format!(
                "outbound item {} completed delivered={delivered} attempted={attempted}",
                entry.item.id
            ),
        );
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

fn push_queue_entry(queue: &Mutex<VecDeque<QueueEntry>>, entry: QueueEntry) {
    if let Ok(mut guard) = queue.lock() {
        guard.push_back(entry);
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
    let mut peer_by_ip = HashMap::new();
    if let Ok(guard) = runtime.discovered_devices.lock() {
        for device in guard.iter() {
            let peer = format!("{}:{}", device.addr, device.port);
            if let Ok(socket_addr) = peer.parse::<SocketAddr>() {
                peer_by_ip.insert(socket_addr.ip().to_string(), peer);
            }
        }
    }
    for peer in &settings.sync.peers {
        let addr = normalize_peer(peer, settings.sync.listen_port);
        if !addr.is_empty() {
            if let Ok(socket_addr) = addr.parse::<SocketAddr>() {
                peer_by_ip
                    .entry(socket_addr.ip().to_string())
                    .or_insert(addr);
            }
        }
    }
    if let Ok(guard) = runtime.known_members.lock() {
        for member in guard.iter() {
            if let Some(addr) = member.strip_prefix("addr:") {
                let normalized = normalize_peer(addr, settings.sync.listen_port);
                if is_expected_listen_addr(&normalized, settings.sync.listen_port) {
                    if let Ok(socket_addr) = normalized.parse::<SocketAddr>() {
                        peer_by_ip
                            .entry(socket_addr.ip().to_string())
                            .or_insert(normalized);
                    }
                }
            }
        }
    }
    peer_by_ip.into_values().collect()
}

fn is_expected_listen_addr(addr: &str, listen_port: u16) -> bool {
    addr.parse::<SocketAddr>()
        .map(|socket_addr| socket_addr.port() == listen_port)
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

fn cached_member_signal_count(runtime: &RuntimeInner) -> usize {
    prune_stale_discovered_devices(runtime);
    let discovered = runtime
        .discovered_devices
        .lock()
        .map(|guard| guard.len())
        .unwrap_or(0);
    let known = runtime
        .known_members
        .lock()
        .map(|guard| guard.len())
        .unwrap_or(0);
    discovered + known
}

fn current_member_count(runtime: &RuntimeInner, settings: &Settings) -> usize {
    prune_stale_discovered_devices(runtime);
    let self_device_id = settings.sync_device_id();
    if self_device_id.trim().is_empty() {
        return 0;
    }

    let configured_addrs = settings
        .sync
        .peers
        .iter()
        .map(|peer| normalize_peer(peer, settings.sync.listen_port))
        .filter(|peer| !peer.is_empty())
        .collect::<HashSet<_>>();

    let mut discovered_ids = HashSet::new();
    if let Ok(guard) = runtime.discovered_devices.lock() {
        for device in guard.iter() {
            discovered_ids.insert(device.device_id.clone());
        }
    }

    let mut known_device_ids = HashSet::new();
    let mut known_addrs = HashSet::new();
    if let Ok(guard) = runtime.known_members.lock() {
        for member in guard.iter() {
            if let Some(value) = member.strip_prefix("device:") {
                known_device_ids.insert(value.to_string());
            } else if let Some(value) = member.strip_prefix("addr:") {
                known_addrs.insert(value.to_string());
            }
        }
    }

    let remote_count = discovered_ids
        .len()
        .max(known_device_ids.len())
        .max(known_addrs.len())
        .max(configured_addrs.len());

    remote_count + 1
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
    if let Ok(mut guard) = runtime.last_local_observed.lock() {
        guard.take();
    }
    if let Ok(mut guard) = runtime.ignored_local_hashes.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.recent_event_ids.lock() {
        guard.clear();
    }
    runtime.suppress_until_ms.store(0, Ordering::SeqCst);
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

fn normalize_peer(peer: &str, fallback_port: u16) -> String {
    let value = peer.trim();
    if value.is_empty() {
        return String::new();
    }
    if value.contains(':') {
        value.to_string()
    } else {
        format!("{}:{}", value, fallback_port)
    }
}

pub fn build_item(payload: &ClipboardPayload, device_id: &str) -> Option<ClipboardItem> {
    let payload_bytes = match payload {
        ClipboardPayload::Text { text } => text.as_bytes().to_vec(),
        ClipboardPayload::ImagePng { png_bytes } => png_bytes.clone(),
        ClipboardPayload::FileBundle { archive_bytes, .. } => archive_bytes.clone(),
        ClipboardPayload::FileList {
            paths,
            top_level_names,
            estimated_archive_bytes,
        } => {
            let marker = format!("{paths:?}:{top_level_names:?}:{estimated_archive_bytes}");
            marker.into_bytes()
        }
        ClipboardPayload::Html { html } => html.as_bytes().to_vec(),
        ClipboardPayload::Rtf { rtf } => rtf.as_bytes().to_vec(),
    };

    let size_bytes = match payload {
        ClipboardPayload::FileList {
            estimated_archive_bytes,
            ..
        } => *estimated_archive_bytes,
        _ => payload_bytes.len() as u64,
    };
    if size_bytes == 0 {
        return None;
    }

    let created_at_ms = now_ms();
    let content_hash = payload_hash(&payload_bytes);

    Some(ClipboardItem {
        id: Uuid::new_v4().to_string(),
        content_hash,
        created_at_ms,
        source_device_id: device_id.to_string(),
        size_bytes,
        payload: payload.clone(),
    })
}

fn encode_wire_message(item: &ClipboardItem, settings: &Settings) -> anyhow::Result<Vec<u8>> {
    encode_wire_body(&WireBody::ClipboardItem(item.clone()), settings)
}

fn encode_wire_body(body: &WireBody, settings: &Settings) -> anyhow::Result<Vec<u8>> {
    let plain = bincode::serialize(body)?;
    let source_device_id = wire_body_source_device_id(body);
    let frame = if settings.security.encryption_enabled {
        let secret = effective_secret(settings);
        let (nonce, body) = encrypt_bytes(&plain, &derive_key(&secret))?;
        WireMessage {
            v: WIRE_VERSION,
            encrypted: true,
            source_device_id: source_device_id.clone(),
            nonce: Some(nonce),
            body,
        }
    } else {
        WireMessage {
            v: WIRE_VERSION,
            encrypted: false,
            source_device_id,
            nonce: None,
            body: plain,
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
        WireBody::FileStreamStart(meta) => meta.source_device_id.clone(),
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

    Ok(bincode::deserialize::<WireBody>(&bytes)?)
}

fn effective_secret(settings: &Settings) -> String {
    let pairing_code = settings.security.pairing_code.trim();
    if !pairing_code.is_empty() {
        return pairing_code.to_string();
    }
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
    let nonce = Nonce::from_slice(&nonce_bytes);
    let encrypted = cipher
        .encrypt(nonce, plain)
        .map_err(|_| anyhow::anyhow!("encrypt failed"))?;
    Ok((nonce_bytes, encrypted))
}

fn decrypt_bytes(nonce_bytes: [u8; 12], body: &[u8], key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256GcmSiv::new_from_slice(key)?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plain = cipher
        .decrypt(nonce, body)
        .map_err(|_| anyhow::anyhow!("decrypt failed (shared code or pairing code mismatch?)"))?;
    Ok(plain)
}

fn payload_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn is_clipboard_suppressed(runtime: &RuntimeInner) -> bool {
    runtime.suppress_until_ms.load(Ordering::SeqCst) > now_ms()
}

fn set_clipboard_suppressed(runtime: &RuntimeInner, duration_ms: u64) {
    runtime
        .suppress_until_ms
        .store(now_ms().saturating_add(duration_ms), Ordering::SeqCst);
}

fn should_ignore_local_observation(runtime: &RuntimeInner, content_hash: &str) -> bool {
    if recent_applied_hash_seen(runtime, content_hash) {
        return true;
    }

    let observed_at_ms = now_ms();
    if let Ok(mut guard) = runtime.last_local_observed.lock() {
        if let Some(previous) = guard.as_ref() {
            if previous.content_hash == content_hash
                && observed_at_ms.saturating_sub(previous.observed_at_ms) <= LOCAL_EVENT_DEBOUNCE_MS
            {
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

fn apply_mute_duration_ms(item: &ClipboardItem) -> u64 {
    match &item.payload {
        ClipboardPayload::FileBundle { .. } | ClipboardPayload::FileList { .. } => {
            APPLY_FILE_MUTE_MS
        }
        ClipboardPayload::Html { .. } | ClipboardPayload::Rtf { .. } => APPLY_RICH_TEXT_MUTE_MS,
        _ => APPLY_MUTE_MS,
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
        created_at_ms: item.created_at_ms,
        source_device_id: item.source_device_id.clone(),
    }
}

fn compare_markers(left: &ItemMarker, right: &ItemMarker) -> std::cmp::Ordering {
    left.created_at_ms
        .cmp(&right.created_at_ms)
        .then_with(|| left.source_device_id.cmp(&right.source_device_id))
        .then_with(|| left.id.cmp(&right.id))
}

fn update_latest_item(runtime: &RuntimeInner, item: &ClipboardItem) {
    if let Ok(mut guard) = runtime.latest_item.lock() {
        let marker = item_marker(item);
        let replace = guard
            .as_ref()
            .map(|current| compare_markers(&marker, current).is_gt())
            .unwrap_or(true);
        if replace {
            *guard = Some(marker);
        }
    }
}

fn should_skip_remote_item(runtime: &RuntimeInner, item: &ClipboardItem) -> bool {
    if recent_event_seen(runtime, &item.id) {
        return true;
    }

    let marker = item_marker(item);
    let should_skip = runtime
        .latest_item
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .map(|current| compare_markers(&marker, &current).is_lt())
        .unwrap_or(false);

    if should_skip {
        return true;
    }

    register_recent_event(runtime, &item.id);
    update_latest_item(runtime, item);
    false
}

fn enqueue_outbound_item(runtime: &RuntimeInner, item: ClipboardItem) {
    let item_id = item.id.clone();
    let kind = item.payload.kind();
    let size_bytes = item.size_bytes;
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

fn adopt_receive_transfer(
    runtime: &RuntimeInner,
    transient_id: &str,
    canonical_id: &str,
    peer: &str,
) {
    if let Ok(mut guard) = runtime.transfers.lock() {
        let transient_index = guard.iter().position(|entry| entry.id == transient_id);
        let canonical_index = guard.iter().position(|entry| entry.id == canonical_id);

        match (transient_index, canonical_index) {
            (Some(transient_index), Some(canonical_index))
                if transient_index != canonical_index =>
            {
                let transient = guard[transient_index].clone();
                if let Some(existing) = guard.get_mut(canonical_index) {
                    existing.peer = peer.to_string();
                    existing.transferred_bytes =
                        existing.transferred_bytes.max(transient.transferred_bytes);
                    existing.total_bytes = existing.total_bytes.max(transient.total_bytes);
                    existing.percent = existing.percent.max(transient.percent);
                    existing.updated_at_ms = now_ms();
                    if existing.item_summary.is_empty() {
                        existing.item_summary = transient.item_summary;
                    }
                }
                guard.remove(transient_index);
            }
            (Some(transient_index), None) => {
                if let Some(entry) = guard.get_mut(transient_index) {
                    entry.id = canonical_id.to_string();
                    entry.peer = peer.to_string();
                    entry.updated_at_ms = now_ms();
                }
            }
            _ => {}
        }
    }
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

pub fn new_device_id() -> String {
    Uuid::new_v4().to_string()
}

pub fn discover_devices(
    device_id: &str,
    shared_code: &str,
    timeout_ms: u64,
) -> anyhow::Result<Vec<DiscoveredDevice>> {
    let mdns = ServiceDaemon::new()?;
    let receiver = mdns.browse(SERVICE_TYPE)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut devices = Vec::new();
    let mut seen = HashSet::new();

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = match receiver.recv_timeout(remaining) {
            Ok(value) => value,
            Err(_) => break,
        };
        if let ServiceEvent::ServiceResolved(info) = event {
            if let Some(found) = info_to_device(&info, device_id, shared_code) {
                let dedupe_key = format!("{}:{}:{}", found.device_id, found.addr, found.port);
                if seen.insert(dedupe_key) {
                    devices.push(found);
                }
            }
        }
    }

    shutdown_discovery_daemon(&mdns, receiver);
    Ok(devices)
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
    let candidates = all.into_iter().filter_map(|(_, ip)| match ip {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(_) => None,
    });
    pick_best_ipv4(candidates)
        .map(IpAddr::V4)
        .ok_or_else(|| anyhow::anyhow!("no usable local ipv4 address found"))
}

fn selected_or_active_local_ip(
    settings: &Settings,
    active_local_ip: Option<String>,
) -> Option<String> {
    resolve_local_ip_override(&settings.sync.local_ip)
        .ok()
        .flatten()
        .map(|ip| ip.to_string())
        .or(active_local_ip)
        .or_else(|| pick_local_ip().ok().map(|ip| ip.to_string()))
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
        IpAddr::V4(ipv4) if is_usable_ipv4(ipv4) => Ok(Some(IpAddr::V4(ipv4))),
        IpAddr::V4(_) => Err(anyhow::anyhow!(
            "selected local ip is not usable: {trimmed}"
        )),
        IpAddr::V6(_) => Err(anyhow::anyhow!("selected local ip must be ipv4: {trimmed}")),
    }
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

fn local_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "当前设备".to_string())
}
