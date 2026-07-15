use super::crypto::discovery_domain_id;
use super::discovery::{
    is_usable_ipv4, is_valid_device_id, is_valid_discovery_domain_id, normalize_device_name,
    udp_broadcast_targets, validate_discovered_device,
};
use super::logs::push_log;
use super::members::merge_discovered_devices;
use super::{DiscoveredDevice, RuntimeInner};
use crate::settings::Settings;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const DISCOVERY_APP: &str = "lan-clipboard";
const UDP_RECEIVE_PACKET_LIMIT: usize = 64;
const UDP_RECEIVE_TIME_BUDGET_MS: u64 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoveryAnnouncement {
    pub v: u8,
    pub app: String,
    pub device_id: String,
    pub device_name: String,
    pub domain_id: String,
    pub tcp_port: u16,
}

pub(super) fn send_udp_announcement(
    socket: &UdpSocket,
    settings: &Settings,
    device_id: &str,
    device_name: &str,
) {
    let domain_id = discovery_domain_id(settings);
    let Some(device_name) = normalize_device_name(device_name) else {
        return;
    };
    if !is_valid_device_id(device_id)
        || !is_valid_discovery_domain_id(&domain_id)
        || settings.sync.listen_port < 1024
    {
        return;
    }
    let announcement = DiscoveryAnnouncement {
        v: 2,
        app: DISCOVERY_APP.to_string(),
        device_id: device_id.to_string(),
        device_name,
        domain_id,
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
) -> usize {
    let mut buffer = [0u8; 2048];
    let started_at = Instant::now();
    let domain_id = discovery_domain_id(settings);
    let mut received_packets = 0usize;
    while received_packets < UDP_RECEIVE_PACKET_LIMIT
        && started_at.elapsed() < Duration::from_millis(UDP_RECEIVE_TIME_BUDGET_MS)
        && !runtime.stop_flag.load(Ordering::SeqCst)
    {
        match socket.recv_from(&mut buffer) {
            Ok((size, source)) => {
                received_packets += 1;
                let announcement =
                    match serde_json::from_slice::<DiscoveryAnnouncement>(&buffer[..size]) {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
                if let Some(device) =
                    announcement_to_device(&announcement, source, device_id, &domain_id)
                {
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
    received_packets
}

fn announcement_to_device(
    announcement: &DiscoveryAnnouncement,
    source: SocketAddr,
    self_device_id: &str,
    domain_id: &str,
) -> Option<DiscoveredDevice> {
    if announcement.v != 2
        || announcement.app != DISCOVERY_APP
        || announcement.device_id == self_device_id
        || !is_valid_device_id(&announcement.device_id)
        || announcement.domain_id != domain_id
        || !is_valid_discovery_domain_id(&announcement.domain_id)
        || !is_valid_discovery_domain_id(domain_id)
        || announcement.tcp_port < 1024
    {
        return None;
    }
    let source_ip = match source.ip() {
        IpAddr::V4(ipv4) if is_usable_ipv4(ipv4) => IpAddr::V4(ipv4),
        IpAddr::V4(_) | IpAddr::V6(_) => return None,
    };
    let device_name = normalize_device_name(&announcement.device_name)?;
    let device = DiscoveredDevice {
        device_id: announcement.device_id.clone(),
        device_name,
        addr: source_ip.to_string(),
        port: announcement.tcp_port,
    };
    if !validate_discovered_device(&device) {
        return None;
    }
    Some(device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn announcement() -> DiscoveryAnnouncement {
        DiscoveryAnnouncement {
            v: 2,
            app: DISCOVERY_APP.to_string(),
            device_id: Uuid::from_u128(2).hyphenated().to_string(),
            device_name: "peer laptop".to_string(),
            domain_id: "0123456789abcdef0123456789abcdef".to_string(),
            tcp_port: 32910,
        }
    }

    #[test]
    fn accepts_only_well_formed_udp_announcements() {
        let expected_domain = "0123456789abcdef0123456789abcdef";
        let source = SocketAddr::from(([192, 168, 1, 10], 41234));
        assert!(announcement_to_device(
            &announcement(),
            source,
            &Uuid::from_u128(1).hyphenated().to_string(),
            expected_domain,
        )
        .is_some());

        let mut invalid = announcement();
        invalid.tcp_port = 0;
        assert!(announcement_to_device(
            &invalid,
            source,
            &Uuid::from_u128(1).hyphenated().to_string(),
            expected_domain,
        )
        .is_none());
        invalid = announcement();
        invalid.device_name = "bad\nname".to_string();
        assert!(announcement_to_device(
            &invalid,
            source,
            &Uuid::from_u128(1).hyphenated().to_string(),
            expected_domain,
        )
        .is_none());
        invalid = announcement();
        invalid.device_id = "not-a-uuid".to_string();
        assert!(announcement_to_device(
            &invalid,
            source,
            &Uuid::from_u128(1).hyphenated().to_string(),
            expected_domain,
        )
        .is_none());
    }

    #[test]
    fn udp_poll_stops_at_packet_budget() {
        let receiver = UdpSocket::bind(("127.0.0.1", 0)).expect("bind receiver");
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set readiness timeout");
        let sender = UdpSocket::bind(("127.0.0.1", 0)).expect("bind sender");
        let target = receiver.local_addr().expect("receiver address");
        let settings = Settings::default();
        let mut valid_announcement = announcement();
        valid_announcement.domain_id = discovery_domain_id(&settings);
        let bytes = serde_json::to_vec(&valid_announcement).expect("serialize announcement");
        for _ in 0..(UDP_RECEIVE_PACKET_LIMIT + 20) {
            sender.send_to(&bytes, target).expect("send announcement");
        }
        let mut readiness_probe = [0u8; 2048];
        receiver
            .peek_from(&mut readiness_probe)
            .expect("wait for first datagram");
        receiver.set_nonblocking(true).expect("set nonblocking");

        let runtime = RuntimeInner::default();
        let received = receive_udp_announcements(
            &runtime,
            &settings,
            &Uuid::from_u128(1).hyphenated().to_string(),
            &receiver,
        );

        assert_eq!(received, UDP_RECEIVE_PACKET_LIMIT);
    }
}
