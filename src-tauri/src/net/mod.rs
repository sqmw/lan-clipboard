use crate::settings::Settings;
mod crypto;
mod dedupe;
mod discovery;
mod display;
mod domain;
mod file_stream;
mod flow;
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
mod watch;
mod wire;
mod workers;
pub use discovery::{discover_devices, list_network_interfaces};
use discovery::{filter_devices_for_local_ip, local_device_name, selected_or_active_local_ip};
use domain::{clear_member_cache, reconcile_member_state};
use domain::{
    collect_peer_targets, prune_stale_queue_entries, remember_active_local_ip,
    sanitize_file_component,
};
use flow::{enqueue_inbound_item, enqueue_outbound_item, should_skip_remote_item};
pub use item::{build_item, new_device_id};
mod lifecycle;
use lifecycle::run_sync_loop;
pub use logs::RuntimeLog;
use logs::{push_log, LOG_LIMIT};
use members::{prune_stale_discovered_devices, replace_discovered_devices};
use presence::run_presence_loop;
pub use state::{DiscoveredDevice, NetworkInterfaceOption, RuntimeStatus};
use state::{PresenceInner, RuntimeInner};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use transfers::prune_transfers;
pub use transfers::TransferProgress;

#[derive(Debug, Default)]
pub struct SyncEngine {
    inner: Arc<RuntimeInner>,
}

#[derive(Debug, Default)]
pub struct PresenceService {
    inner: Arc<PresenceInner>,
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
        let config = presence::PresenceConfig {
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
