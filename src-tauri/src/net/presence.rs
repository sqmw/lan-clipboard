use super::discovery::build_service_info;
use super::state::PresenceInner;
use mdns_sd::ServiceDaemon;
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub(super) struct PresenceConfig {
    pub(super) device_id: String,
    pub(super) device_name: String,
    pub(super) domain_id: String,
    pub(super) local_ip: String,
    pub(super) listen_port: u16,
}

impl super::PresenceService {
    /// Stops mDNS advertisement when synchronization is disabled.
    pub fn disable(&self) {
        self.stop();
    }
}

pub(super) fn run_presence_loop(
    runtime: Arc<PresenceInner>,
    config: PresenceConfig,
    ready: SyncSender<Result<(), String>>,
) {
    if runtime.stop_flag.load(Ordering::SeqCst) {
        let _ = ready.send(Err("presence startup cancelled".to_string()));
        return;
    }
    let mdns = match ServiceDaemon::new() {
        Ok(value) => value,
        Err(error) => {
            let _ = ready.send(Err(format!("mDNS daemon startup failed: {error}")));
            return;
        }
    };
    let service = match build_service_info(&config) {
        Ok(value) => value,
        Err(error) => {
            let _ = mdns.shutdown();
            let _ = ready.send(Err(format!("mDNS service configuration failed: {error}")));
            return;
        }
    };

    if let Err(error) = mdns.register(service) {
        let _ = mdns.shutdown();
        let _ = ready.send(Err(format!("mDNS service registration failed: {error}")));
        return;
    }
    if ready.send(Ok(())).is_err() {
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
