use super::discovery::{discover_devices, local_device_name, UDP_DISCOVERY_PORT};
use super::inbound::spawn_incoming_connection_worker;
use super::logs::{push_log, set_error};
use super::members::replace_discovered_devices;
use super::queue::QueueLane;
use super::socket::tune_stream_for_receive;
use super::transfers::has_active_transfers;
use super::udp::{receive_udp_announcements, send_udp_announcement};
use super::watch::{prune_clipboard_observation_caches, spawn_clipboard_watch_worker};
use super::wire::MAX_WIRE_FRAME_BYTES;
use super::workers::{
    join_worker, main_loop_sleep_duration, spawn_inbound_apply_worker, spawn_outbound_worker,
};
use super::{reconcile_member_state, RuntimeInner};
use crate::settings::Settings;
use std::net::{TcpListener, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DISCOVERY_REFRESH_MS: u64 = 3_000;
const DISCOVERY_TIMEOUT_MS: u64 = 900;
const UDP_ANNOUNCE_MS: u64 = 500;
pub(super) fn run_sync_loop(runtime: Arc<RuntimeInner>, settings: Settings, device_id: String) {
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

        prune_clipboard_observation_caches(&runtime);
        std::thread::sleep(main_loop_sleep_duration(&runtime));
    }

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
            replace_discovered_devices(runtime, selected_local_ip, devices);
            reconcile_member_state(runtime, settings);
        }
        Err(error) => set_error(runtime, format!("peer discovery failed: {error}")),
    }
}
