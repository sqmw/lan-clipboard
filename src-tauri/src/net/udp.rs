use super::discovery::udp_broadcast_targets;
use super::logs::push_log;
use super::members::{mark_known_member, merge_discovered_devices};
use super::{DiscoveredDevice, RuntimeInner};
use crate::settings::Settings;
use serde::{Deserialize, Serialize};
use std::net::{SocketAddr, UdpSocket};

const DISCOVERY_APP: &str = "lan-clipboard";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiscoveryAnnouncement {
    pub v: u8,
    pub app: String,
    pub device_id: String,
    pub device_name: String,
    pub shared_code: String,
    pub tcp_port: u16,
}

pub(super) fn send_udp_announcement(
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

pub(super) fn receive_udp_announcements(
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
