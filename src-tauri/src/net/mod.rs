use crate::clipboard;
use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::Settings;
use aes_gcm_siv::aead::{Aead, KeyInit};
use aes_gcm_siv::{Aes256GcmSiv, Nonce};
use base64::Engine;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SERVICE_TYPE: &str = "_lan-clipboard._tcp.local.";
const LOG_LIMIT: usize = 800;
const DISCOVERY_REFRESH_MS: u64 = 2_500;
const DISCOVERY_TIMEOUT_MS: u64 = 900;
const STATUS_DISCOVERY_TIMEOUT_MS: u64 = 350;
const APPLY_MUTE_MS: u64 = 1_200;
const DISCOVERY_MEMBER_TTL_MS: u64 = 30_000;
const UDP_DISCOVERY_PORT: u16 = 32911;
const UDP_ANNOUNCE_MS: u64 = 1_000;
const DISCOVERY_APP: &str = "lan-clipboard";

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeStatus {
    pub running: bool,
    pub device_id: String,
    pub shared_code: String,
    pub last_error: Option<String>,
    pub recent_log_count: usize,
    pub peer_count: usize,
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
    pub nonce_base64: Option<String>,
    pub body_base64: String,
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
    muted_until: Mutex<Instant>,
    last_hash: Mutex<Option<String>>,
    logs: Mutex<Vec<RuntimeLog>>,
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
            muted_until: Mutex::new(Instant::now()),
            last_hash: Mutex::new(None),
            logs: Mutex::new(Vec::new()),
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
    listen_port: u16,
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
        let recent_log_count = self.inner.logs.lock().map(|guard| guard.len()).unwrap_or(0);
        let peer_count = current_member_count(&self.inner, settings);
        RuntimeStatus {
            running: self.inner.running.load(Ordering::SeqCst),
            device_id: settings.sync_device_id(),
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
            listen_port: settings.sync.listen_port,
        };
        let signature = format!(
            "{}:{}:{}:{}",
            config.device_id, config.device_name, config.shared_code, config.listen_port
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

    while !runtime.stop_flag.load(Ordering::SeqCst) {
        if let Some(socket) = udp_socket.as_ref() {
            receive_udp_announcements(&runtime, &settings, &device_id, socket);
            if last_udp_announce.elapsed() >= Duration::from_millis(UDP_ANNOUNCE_MS) {
                send_udp_announcement(socket, &settings, &device_id, &device_name);
                last_udp_announce = Instant::now();
            }
        }

        if last_discovery.elapsed() >= Duration::from_millis(DISCOVERY_REFRESH_MS) {
            refresh_discovered_devices(&runtime, &settings, &device_id, DISCOVERY_TIMEOUT_MS);
            last_discovery = Instant::now();
        }

        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = handle_incoming(&runtime, &settings, stream, &device_id) {
                    set_error(&runtime, format!("incoming handler failed: {error}"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => set_error(&runtime, format!("listener accept failed: {error}")),
        }

        if should_send(&runtime) {
            match clipboard::read_snapshot(&settings.limits) {
                Ok(payload) => {
                    if let Some(item) = build_item(&payload, &device_id) {
                        if is_new_hash(&runtime, &item.id) {
                            send_to_all_peers(&runtime, &settings, &item);
                        }
                    }
                }
                Err(clipboard::ClipboardError::Unsupported) => {}
                Err(error) => set_error(&runtime, format!("clipboard read failed: {error}")),
            }
        }

        std::thread::sleep(Duration::from_millis(settings.sync.poll_interval_ms));
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
    for target in udp_broadcast_targets() {
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

fn should_send(runtime: &RuntimeInner) -> bool {
    runtime
        .muted_until
        .lock()
        .map(|guard| Instant::now() >= *guard)
        .unwrap_or(true)
}

fn handle_incoming(
    runtime: &RuntimeInner,
    settings: &Settings,
    stream: TcpStream,
    device_id: &str,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream);
    let remote_addr = reader
        .get_ref()
        .peer_addr()
        .ok()
        .map(|addr| addr.to_string());
    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        let frame = serde_json::from_str::<WireMessage>(line.trim())?;
        line.clear();
        let item = decode_wire_message(&frame, settings)?;

        if item.source_device_id == device_id {
            continue;
        }
        mark_known_member(runtime, "device", &item.source_device_id);
        if let Some(addr) = &remote_addr {
            mark_known_member(runtime, "addr", addr);
        }
        if !is_new_hash(runtime, &item.id) {
            continue;
        }

        clipboard::write_item(&item, &settings.limits)?;
        push_log(
            runtime,
            "INFO",
            &format!("applied item from {}", item.source_device_id),
        );
        if let Ok(mut guard) = runtime.muted_until.lock() {
            *guard = Instant::now() + Duration::from_millis(APPLY_MUTE_MS);
        }
    }
    Ok(())
}

fn send_to_all_peers(runtime: &RuntimeInner, settings: &Settings, item: &ClipboardItem) {
    let payload = match encode_wire_message(item, settings) {
        Ok(mut payload) => {
            payload.push('\n');
            payload
        }
        Err(error) => {
            set_error(runtime, format!("encode payload failed: {error}"));
            return;
        }
    };

    let mut delivered = 0usize;
    for peer in collect_peer_targets(runtime, settings) {
        let timeout = Duration::from_millis(900);
        let stream = TcpStream::connect_timeout(
            &match peer.parse() {
                Ok(socket_addr) => socket_addr,
                Err(_) => {
                    push_log(runtime, "WARN", &format!("skip bad peer addr: {}", peer));
                    continue;
                }
            },
            timeout,
        );

        let mut stream = match stream {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        let _ = stream.set_write_timeout(Some(timeout));
        if stream.write_all(payload.as_bytes()).is_ok() {
            mark_known_member(runtime, "addr", &peer);
            delivered += 1;
        }
    }

    if delivered > 0 {
        push_log(
            runtime,
            "DEBUG",
            &format!("broadcast delivered={}", delivered),
        );
    }
}

fn collect_peer_targets(runtime: &RuntimeInner, settings: &Settings) -> Vec<String> {
    prune_stale_discovered_devices(runtime);
    let mut peers = HashSet::new();
    for peer in &settings.sync.peers {
        let addr = normalize_peer(peer, settings.sync.listen_port);
        if !addr.is_empty() {
            peers.insert(addr);
        }
    }
    if let Ok(guard) = runtime.discovered_devices.lock() {
        for device in guard.iter() {
            peers.insert(format!("{}:{}", device.addr, device.port));
        }
    }
    peers.into_iter().collect()
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
        ClipboardPayload::ImagePng { png_base64 } => png_base64.as_bytes().to_vec(),
        ClipboardPayload::Html { html } => html.as_bytes().to_vec(),
        ClipboardPayload::Rtf { rtf } => rtf.as_bytes().to_vec(),
    };

    let size_bytes = payload_bytes.len() as u64;
    if size_bytes == 0 {
        return None;
    }

    let created_at_ms = now_ms();

    Some(ClipboardItem {
        id: payload_hash(&payload_bytes),
        created_at_ms,
        source_device_id: device_id.to_string(),
        size_bytes,
        payload: payload.clone(),
    })
}

fn encode_wire_message(item: &ClipboardItem, settings: &Settings) -> anyhow::Result<String> {
    let plain = serde_json::to_vec(item)?;
    if settings.security.encryption_enabled {
        let secret = effective_secret(settings);
        let (nonce_base64, body_base64) = encrypt_bytes(&plain, &derive_key(&secret))?;
        let frame = WireMessage {
            v: 1,
            encrypted: true,
            source_device_id: item.source_device_id.clone(),
            nonce_base64: Some(nonce_base64),
            body_base64,
        };
        return Ok(serde_json::to_string(&frame)?);
    }

    let frame = WireMessage {
        v: 1,
        encrypted: false,
        source_device_id: item.source_device_id.clone(),
        nonce_base64: None,
        body_base64: base64::engine::general_purpose::STANDARD.encode(plain),
    };
    Ok(serde_json::to_string(&frame)?)
}

fn decode_wire_message(frame: &WireMessage, settings: &Settings) -> anyhow::Result<ClipboardItem> {
    let bytes = if frame.encrypted {
        decrypt_bytes(
            &frame.body_base64,
            frame
                .nonce_base64
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing nonce"))?,
            &derive_key(&effective_secret(settings)),
        )?
    } else {
        if settings.security.encryption_enabled {
            return Err(anyhow::anyhow!(
                "received plain frame but encryption enabled"
            ));
        }
        base64::engine::general_purpose::STANDARD.decode(frame.body_base64.as_bytes())?
    };

    Ok(serde_json::from_slice::<ClipboardItem>(&bytes)?)
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

fn encrypt_bytes(plain: &[u8], key: &[u8; 32]) -> anyhow::Result<(String, String)> {
    let cipher = Aes256GcmSiv::new_from_slice(key)?;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let encrypted = cipher
        .encrypt(nonce, plain)
        .map_err(|_| anyhow::anyhow!("encrypt failed"))?;
    Ok((
        base64::engine::general_purpose::STANDARD.encode(nonce_bytes),
        base64::engine::general_purpose::STANDARD.encode(encrypted),
    ))
}

fn decrypt_bytes(body_base64: &str, nonce_base64: &str, key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256GcmSiv::new_from_slice(key)?;
    let nonce_bytes = base64::engine::general_purpose::STANDARD.decode(nonce_base64.as_bytes())?;
    if nonce_bytes.len() != 12 {
        return Err(anyhow::anyhow!("invalid nonce length"));
    }
    let body = base64::engine::general_purpose::STANDARD.decode(body_base64.as_bytes())?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plain = cipher
        .decrypt(nonce, body.as_ref())
        .map_err(|_| anyhow::anyhow!("decrypt failed (shared code or pairing code mismatch?)"))?;
    Ok(plain)
}

fn payload_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn is_new_hash(runtime: &RuntimeInner, hash: &str) -> bool {
    if let Ok(mut guard) = runtime.last_hash.lock() {
        if guard.as_deref() == Some(hash) {
            return false;
        }
        *guard = Some(hash.to_string());
        return true;
    }
    true
}

fn set_error(runtime: &RuntimeInner, message: String) {
    if let Ok(mut guard) = runtime.last_error.lock() {
        *guard = Some(message.clone());
    }
    push_log(runtime, "ERROR", &message);
}

fn push_log(runtime: &RuntimeInner, level: &str, message: &str) {
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

fn build_service_info(config: &PresenceConfig) -> anyhow::Result<ServiceInfo> {
    let local_ip = pick_local_ip()?;
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
    for address in addresses {
        if let IpAddr::V4(ipv4) = address {
            if !ipv4.is_loopback() {
                return Some(ipv4.to_string());
            }
        }
    }
    None
}

fn pick_local_ip() -> anyhow::Result<IpAddr> {
    let all = local_ip_address::list_afinet_netifas()?;
    for (_, ip) in all {
        if let IpAddr::V4(v4) = ip {
            if !v4.is_loopback() && !v4.is_link_local() {
                return Ok(IpAddr::V4(v4));
            }
        }
    }
    Err(anyhow::anyhow!("no usable local ipv4 address found"))
}

fn udp_broadcast_targets() -> Vec<SocketAddr> {
    let mut targets = HashSet::new();
    targets.insert(SocketAddr::from(([255, 255, 255, 255], UDP_DISCOVERY_PORT)));

    if let Ok(all) = local_ip_address::list_afinet_netifas() {
        for (_, ip) in all {
            if let IpAddr::V4(ipv4) = ip {
                if ipv4.is_loopback() || ipv4.is_link_local() {
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
