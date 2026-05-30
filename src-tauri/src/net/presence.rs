use super::discovery::build_service_info;
use super::state::PresenceInner;
use mdns_sd::ServiceDaemon;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const PRESENCE_RETRY_MS: u64 = 1_000;

#[derive(Debug, Clone)]
pub(super) struct PresenceConfig {
    pub(super) device_id: String,
    pub(super) device_name: String,
    pub(super) shared_code: String,
    pub(super) local_ip: String,
    pub(super) listen_port: u16,
}

pub(super) fn run_presence_loop(runtime: Arc<PresenceInner>, config: PresenceConfig) {
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
