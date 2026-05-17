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
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SERVICE_TYPE: &str = "_lan-clipboard._tcp.local.";
const LOG_LIMIT: usize = 800;

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeStatus {
    pub running: bool,
    pub device_id: String,
    pub last_error: Option<String>,
    pub recent_log_count: usize,
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
    pub device_code: String,
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

#[derive(Debug)]
struct RuntimeInner {
    running: AtomicBool,
    stop_flag: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
    last_error: Mutex<Option<String>>,
    muted_until: Mutex<Instant>,
    last_hash: Mutex<Option<String>>,
    logs: Mutex<Vec<RuntimeLog>>,
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
        }
    }
}

#[derive(Debug, Default)]
pub struct SyncEngine {
    inner: Arc<RuntimeInner>,
}

impl SyncEngine {
    pub fn status(&self, device_id: &str) -> RuntimeStatus {
        let error = self
            .inner
            .last_error
            .lock()
            .ok()
            .and_then(|guard| (*guard).clone());
        let recent_log_count = self.inner.logs.lock().map(|g| g.len()).unwrap_or(0);
        RuntimeStatus {
            running: self.inner.running.load(Ordering::SeqCst),
            device_id: device_id.to_string(),
            last_error: error,
            recent_log_count,
        }
    }

    pub fn logs(&self, limit: usize) -> Vec<RuntimeLog> {
        let target = if limit == 0 { 200 } else { limit.min(LOG_LIMIT) };
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

    pub fn start(&self, settings: Settings, device_id: String) -> anyhow::Result<()> {
        if self.inner.running.swap(true, Ordering::SeqCst) {
            self.log("INFO", "sync loop already running");
            return Ok(());
        }

        self.inner.stop_flag.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = self.inner.last_error.lock() {
            *guard = None;
        }
        self.log(
            "INFO",
            &format!(
                "sync starting on port={} peers={}",
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

    while !runtime.stop_flag.load(Ordering::SeqCst) {
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
    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        let frame = serde_json::from_str::<WireMessage>(line.trim())?;
        line.clear();
        let item = decode_wire_message(&frame, settings)?;

        if item.source_device_id == device_id {
            continue;
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
            *guard = Instant::now() + Duration::from_millis(1200);
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
    for peer in &settings.sync.peers {
        let addr = normalize_peer(peer, settings.sync.listen_port);
        let timeout = Duration::from_millis(900);
        let stream = TcpStream::connect_timeout(
            &match addr.parse() {
                Ok(socket_addr) => socket_addr,
                Err(_) => {
                    push_log(runtime, "WARN", &format!("skip bad peer addr: {}", addr));
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
            delivered += 1;
        }
    }

    if delivered > 0 {
        push_log(runtime, "DEBUG", &format!("broadcast delivered={}", delivered));
    }
}

fn normalize_peer(peer: &str, fallback_port: u16) -> String {
    if peer.contains(':') {
        peer.trim().to_string()
    } else {
        format!("{}:{}", peer.trim(), fallback_port)
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
        let code = settings.security.pairing_code.trim();
        if settings.security.require_pairing_code && code.is_empty() {
            return Err(anyhow::anyhow!("pairing code required but empty"));
        }
        let key = derive_key(code);
        let (nonce_base64, body_base64) = encrypt_bytes(&plain, &key)?;
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
        let code = settings.security.pairing_code.trim();
        if settings.security.require_pairing_code && code.is_empty() {
            return Err(anyhow::anyhow!("local pairing code empty"));
        }
        let key = derive_key(code);
        let nonce_base64 = frame
            .nonce_base64
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing nonce"))?;
        decrypt_bytes(&frame.body_base64, nonce_base64, &key)?
    } else {
        if settings.security.encryption_enabled {
            return Err(anyhow::anyhow!("received plain frame but encryption enabled"));
        }
        base64::engine::general_purpose::STANDARD.decode(frame.body_base64.as_bytes())?
    };

    Ok(serde_json::from_slice::<ClipboardItem>(&bytes)?)
}

fn derive_key(pairing_code: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(pairing_code.as_bytes());
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
        .map_err(|_| anyhow::anyhow!("decrypt failed (pairing code mismatch?)"))?;
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
    device_code: &str,
    listen_port: u16,
    timeout_ms: u64,
) -> anyhow::Result<Vec<DiscoveredDevice>> {
    let mdns = ServiceDaemon::new()?;
    let local_ip = pick_local_ip()?;
    let host_name = format!("lan-clipboard-{}.local.", device_id);
    let properties = [("device_id", device_id), ("code", device_code)];
    let service = ServiceInfo::new(
        SERVICE_TYPE,
        device_id,
        &host_name,
        local_ip.to_string(),
        listen_port,
        &properties[..],
    )?;
    mdns.register(service)?;

    let receiver = mdns.browse(SERVICE_TYPE)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut devices = Vec::new();
    let mut seen = HashSet::new();

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = receiver.recv_timeout(remaining);
        let event = match event {
            Ok(value) => value,
            Err(_) => break,
        };
        if let ServiceEvent::ServiceResolved(info) = event {
            if let Some(found) = info_to_device(&info, device_id) {
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

fn shutdown_discovery_daemon(
    mdns: &ServiceDaemon,
    receiver: mdns_sd::Receiver<ServiceEvent>,
) {
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

fn info_to_device(info: &ServiceInfo, self_device_id: &str) -> Option<DiscoveredDevice> {
    let device_id = info.get_fullname().split('.').next()?.to_string();
    if device_id == self_device_id {
        return None;
    }
    let addr = pick_ipv4(info.get_addresses())?;
    let device_code = info
        .get_properties()
        .get_property_val_str("code")
        .unwrap_or("")
        .to_string();
    Some(DiscoveredDevice {
        device_id,
        device_code,
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
