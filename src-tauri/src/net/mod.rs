use crate::settings::Settings;
mod crypto;
mod dedupe;
mod discovery;
mod display;
mod domain;
mod file_stream;
mod flow;
mod handshake;
mod inbound;
mod item;
mod logs;
mod marker;
mod members;
mod metrics;
mod presence;
mod queue;
mod sender;
mod socket;
mod state;
mod transfers;
mod udp;
mod udp_socket;
mod watch;
mod wire;
mod workers;
pub use discovery::{discover_devices, list_network_interfaces};
use discovery::{filter_devices_for_local_ip, local_device_name, selected_or_active_local_ip};
use domain::clear_member_cache;
use domain::{collect_peer_targets, remember_active_local_ip};
use flow::{enqueue_inbound_item, enqueue_outbound_item, should_skip_remote_item};
pub use item::{build_item, new_device_id};
mod lifecycle;
use crypto::discovery_domain_id;
use lifecycle::run_sync_loop;
pub use logs::RuntimeLog;
use logs::{clear_runtime_log_file, push_log, LOG_LIMIT};
use members::{
    prune_stale_discovered_devices, refresh_discovered_devices, replace_discovered_devices,
};
use presence::run_presence_loop;
use sender::shutdown_outbound_connections;
pub use state::{DiscoveredDevice, NetworkInterfaceOption, RuntimeStatus};
use state::{PresenceInner, RuntimeInner};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::Duration;
use transfers::prune_transfers;
pub use transfers::TransferProgress;

const PRESENCE_READY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
pub struct SyncEngine {
    inner: Arc<RuntimeInner>,
}

#[derive(Debug, Default)]
pub struct PresenceService {
    inner: Arc<PresenceInner>,
}

struct SyncStartGuard<'a> {
    runtime: &'a RuntimeInner,
    worker: Option<std::thread::JoinHandle<()>>,
    committed: bool,
}

impl<'a> SyncStartGuard<'a> {
    fn new(runtime: &'a RuntimeInner) -> Self {
        Self {
            runtime,
            worker: None,
            committed: false,
        }
    }
}

impl Drop for SyncStartGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.runtime.stop_flag.store(true, Ordering::SeqCst);
        self.runtime.running.store(false, Ordering::SeqCst);
        shutdown_outbound_connections(self.runtime);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
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
            last_error: error,
            settings_notice: None,
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
        clear_runtime_log_file();
    }

    pub fn transfers(&self) -> Vec<TransferProgress> {
        prune_transfers(&self.inner);
        self.inner
            .transfers
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn has_active_transfers(&self) -> bool {
        transfers::has_active_transfers(&self.inner)
    }

    pub fn record_error(&self, message: String) {
        logs::set_error(&self.inner, message);
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
    ) {
        replace_discovered_devices(&self.inner, selected_local_ip, devices);
    }

    /// Applies a routine discovery observation without treating omitted peers
    /// as an authoritative membership removal. Explicit settings/domain resets
    /// must continue to use [`Self::replace_discovered_devices`].
    pub fn refresh_discovered_devices(&self, devices: Vec<DiscoveredDevice>) {
        refresh_discovered_devices(&self.inner, devices);
    }

    pub fn start(&self, settings: Settings, device_id: String) -> anyhow::Result<()> {
        if !settings.security.encryption_enabled {
            return Err(anyhow::anyhow!(
                "protocol v4 requires encrypted authenticated sessions"
            ));
        }
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
                "sync starting protocol=v4 port={} encryption={}",
                settings.sync.listen_port, settings.security.encryption_enabled
            ),
        );

        let runtime = Arc::clone(&self.inner);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let mut startup = SyncStartGuard::new(&self.inner);
        let worker = std::thread::Builder::new()
            .name("lan-clipboard-sync".to_string())
            .spawn(move || run_sync_loop(runtime, settings, device_id, ready_tx))?;
        startup.worker = Some(worker);

        {
            let mut guard = self
                .inner
                .worker
                .lock()
                .map_err(|_| anyhow::anyhow!("sync worker lock poisoned"))?;
            *guard = startup.worker.take();
        }
        startup.committed = true;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                let _ = self.stop();
                Err(anyhow::anyhow!(error))
            }
            Err(error) => {
                let _ = self.stop();
                Err(anyhow::anyhow!("sync startup readiness failed: {error}"))
            }
        }
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        self.inner.stop_flag.store(true, Ordering::SeqCst);
        self.inner.running.store(false, Ordering::SeqCst);
        shutdown_outbound_connections(&self.inner);
        let worker = self
            .inner
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = worker {
            let _ = handle.join();
        }
        self.inner
            .outbound_sockets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        clear_member_cache(&self.inner);
        self.log("INFO", "sync stopped");
        Ok(())
    }

    fn log(&self, level: &str, message: &str) {
        push_log(&self.inner, level, message);
    }
}

impl PresenceService {
    pub fn ensure(&self, settings: Settings, device_id: String) -> anyhow::Result<()> {
        let config = presence::PresenceConfig {
            device_id: device_id.clone(),
            device_name: local_device_name(&settings.sync_device_id()),
            domain_id: discovery_domain_id(&settings),
            local_ip: settings.sync.local_ip.trim().to_string(),
            listen_port: settings.sync.listen_port,
        };
        let signature = format!(
            "{}:{}:{}:{}:{}",
            config.device_id,
            config.device_name,
            config.domain_id,
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
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("lan-clipboard-presence".to_string())
            .spawn(move || run_presence_loop(runtime, config, ready_tx))?;

        match self.inner.worker.lock() {
            Ok(mut guard) => *guard = Some(worker),
            Err(_) => {
                self.inner.stop_flag.store(true, Ordering::SeqCst);
                let _ = worker.join();
                return Err(anyhow::anyhow!("presence worker lock poisoned"));
            }
        }
        match ready_rx.recv_timeout(PRESENCE_READY_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.stop();
                return Err(anyhow::anyhow!(error));
            }
            Err(error) => {
                self.stop();
                return Err(anyhow::anyhow!(
                    "presence startup readiness failed: {error}"
                ));
            }
        }
        match self.inner.signature.lock() {
            Ok(mut guard) => *guard = Some(signature),
            Err(_) => {
                self.stop();
                return Err(anyhow::anyhow!("presence signature lock poisoned"));
            }
        }
        Ok(())
    }

    fn stop(&self) {
        self.inner.stop_flag.store(true, Ordering::SeqCst);
        let worker = self
            .inner
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = worker {
            let _ = handle.join();
        }
        if let Ok(mut signature) = self.inner.signature.lock() {
            signature.take();
        }
    }
}

impl Drop for PresenceService {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClipboardItem, ClipboardPayload};
    use uuid::Uuid;

    fn device(index: u128, address: &str) -> DiscoveredDevice {
        DiscoveredDevice {
            device_id: Uuid::from_u128(index).hyphenated().to_string(),
            device_name: format!("peer-{index}"),
            addr: address.to_string(),
            port: 32910,
        }
    }

    fn text_item() -> ClipboardItem {
        ClipboardItem {
            id: "item-1".to_string(),
            content_hash: "hash-1".to_string(),
            created_at_us: 1,
            source_device_id: "source-device".to_string(),
            size_bytes: 4,
            payload: ClipboardPayload::Text {
                text: "text".to_string(),
            },
        }
    }

    fn transfer(id: &str, direction: &str, status: &str) -> TransferProgress {
        TransferProgress {
            id: id.to_string(),
            direction: direction.to_string(),
            peer: "192.168.1.2:32910".to_string(),
            item_kind: "text".to_string(),
            item_label: "text".to_string(),
            item_summary: "text".to_string(),
            item_id: "item-1".to_string(),
            transferred_bytes: 0,
            total_bytes: 4,
            percent: 0,
            status: status.to_string(),
            updated_at_ms: metrics::now_ms(),
            error: None,
        }
    }

    #[test]
    fn routine_discovery_refresh_preserves_members_and_active_transfer_state() {
        let engine = SyncEngine::default();
        let first_peer = device(1, "192.168.1.2");
        let partial_scan_peer = device(2, "192.168.1.3");

        engine.refresh_discovered_devices(vec![first_peer.clone()]);
        engine
            .inner
            .outbound_queue
            .lock()
            .unwrap()
            .push_back(queue::new_queue_entry(text_item()));
        engine.inner.transfers.lock().unwrap().extend([
            transfer("send:peer:item-1", "send", "sending"),
            transfer("recv:peer:item-1", "receive", "receiving"),
        ]);

        engine.refresh_discovered_devices(Vec::new());
        engine.refresh_discovered_devices(vec![partial_scan_peer.clone()]);

        let devices = engine.devices(None);
        assert!(devices
            .iter()
            .any(|device| device.device_id == first_peer.device_id));
        assert!(devices
            .iter()
            .any(|device| device.device_id == partial_scan_peer.device_id));
        assert_eq!(engine.inner.outbound_queue.lock().unwrap().len(), 1);

        let transfers = engine.transfers();
        assert_eq!(
            transfers
                .iter()
                .find(|entry| entry.id == "send:peer:item-1")
                .map(|entry| entry.status.as_str()),
            Some("sending")
        );
        assert_eq!(
            transfers
                .iter()
                .find(|entry| entry.id == "recv:peer:item-1")
                .map(|entry| entry.status.as_str()),
            Some("receiving")
        );
    }

    #[test]
    fn explicit_member_replacement_remains_authoritative_for_domain_resets() {
        let engine = SyncEngine::default();
        let peer = device(3, "192.168.1.4");

        engine.refresh_discovered_devices(vec![peer]);
        engine.replace_discovered_devices(None, Vec::new());

        assert!(engine.devices(None).is_empty());
    }
}
