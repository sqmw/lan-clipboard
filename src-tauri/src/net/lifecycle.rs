use super::discovery::{
    discover_devices, local_device_name, resolve_local_ip_override, UDP_DISCOVERY_PORT,
};
use super::inbound::{shutdown_incoming_workers, spawn_incoming_connection_worker};
use super::logs::{push_log, set_error};
use super::members::refresh_discovered_devices as merge_discovery_refresh;
use super::queue::QueueLane;
use super::transfers::has_active_transfers;
use super::udp::{receive_udp_announcements, send_udp_announcement};
use super::udp_socket::bind_discovery_socket;
use super::watch::{prune_clipboard_observation_caches, spawn_clipboard_watch_worker};
use super::workers::{
    join_worker, main_loop_sleep_duration, spawn_inbound_apply_worker, spawn_outbound_worker,
};
use super::RuntimeInner;
use crate::settings::Settings;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener};
use std::sync::atomic::Ordering;
use std::sync::{mpsc::SyncSender, Arc};
use std::time::{Duration, Instant};

const DISCOVERY_REFRESH_MS: u64 = 3_000;
const DISCOVERY_TIMEOUT_MS: u64 = 900;
const UDP_ANNOUNCE_MS: u64 = 500;
pub(super) fn run_sync_loop(
    runtime: Arc<RuntimeInner>,
    settings: Settings,
    device_id: String,
    ready: SyncSender<Result<(), String>>,
) {
    let bind_ip = match resolve_local_ip_override(&settings.sync.local_ip) {
        Ok(Some(ip)) => ip,
        Ok(None) if settings.sync.local_ip.trim().is_empty() => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        Ok(None) => {
            let message = format!(
                "selected local ip is not assigned on this machine: {}",
                settings.sync.local_ip.trim()
            );
            let _ = ready.send(Err(message.clone()));
            set_error(&runtime, message);
            runtime.running.store(false, Ordering::SeqCst);
            return;
        }
        Err(error) => {
            let message = format!("selected local ip is invalid: {error}");
            let _ = ready.send(Err(message.clone()));
            set_error(&runtime, message);
            runtime.running.store(false, Ordering::SeqCst);
            return;
        }
    };
    let addr = SocketAddr::new(bind_ip, settings.sync.listen_port);
    let listener = match TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(error) => {
            let message = format!("listener bind failed: {error}");
            let _ = ready.send(Err(message.clone()));
            set_error(&runtime, message);
            runtime.running.store(false, Ordering::SeqCst);
            return;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        let message = format!("listener nonblocking setup failed: {error}");
        let _ = ready.send(Err(message.clone()));
        set_error(&runtime, message);
        runtime.running.store(false, Ordering::SeqCst);
        return;
    }
    push_log(&runtime, "INFO", &format!("listener ready at {}", addr));

    let selected_udp_ip = match bind_ip {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    };
    let udp_socket = match bind_discovery_socket(selected_udp_ip, UDP_DISCOVERY_PORT) {
        Ok(socket) => {
            push_log(
                &runtime,
                "INFO",
                &format!(
                    "udp discovery ready at 0.0.0.0:{} interface={}",
                    UDP_DISCOVERY_PORT,
                    selected_udp_ip
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| "all".to_string())
                ),
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
    let watcher = spawn_clipboard_watch_worker(Arc::clone(&runtime), &settings, &device_id);
    let inbound_worker = spawn_inbound_apply_worker(Arc::clone(&runtime), settings.clone());
    let priority_outbound_worker = spawn_outbound_worker(
        "lan-clipboard-outbound-priority",
        Arc::clone(&runtime),
        settings.clone(),
        &[QueueLane::Realtime, QueueLane::Visual],
    );
    let bulk_outbound_worker = spawn_outbound_worker(
        "lan-clipboard-outbound-bulk",
        Arc::clone(&runtime),
        settings.clone(),
        &[QueueLane::Bulk],
    );

    if watcher.is_none()
        || inbound_worker.is_none()
        || priority_outbound_worker.is_none()
        || bulk_outbound_worker.is_none()
    {
        runtime.stop_flag.store(true, Ordering::SeqCst);
        let message = "failed to spawn one or more sync workers".to_string();
        let _ = ready.send(Err(message.clone()));
        set_error(&runtime, message);
        join_worker(watcher);
        join_worker(inbound_worker);
        join_worker(priority_outbound_worker);
        join_worker(bulk_outbound_worker);
        runtime.running.store(false, Ordering::SeqCst);
        return;
    }
    if ready.send(Ok(())).is_err() {
        runtime.stop_flag.store(true, Ordering::SeqCst);
    }

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
                if let Err(error) = stream.set_nonblocking(false) {
                    let _ = stream.shutdown(Shutdown::Both);
                    set_error(
                        &runtime,
                        format!("accepted stream blocking setup failed: {error}"),
                    );
                    continue;
                }
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

        prune_clipboard_observation_caches(&runtime);
        std::thread::sleep(main_loop_sleep_duration(&runtime));
    }

    shutdown_incoming_workers(&runtime);
    join_worker(watcher);
    join_worker(inbound_worker);
    join_worker(priority_outbound_worker);
    join_worker(bulk_outbound_worker);
    runtime.running.store(false, Ordering::SeqCst);
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
            merge_discovery_refresh(runtime, devices);
        }
        Err(error) => set_error(runtime, format!("peer discovery failed: {error}")),
    }
}
