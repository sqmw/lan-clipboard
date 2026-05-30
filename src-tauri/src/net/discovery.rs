use super::presence::PresenceConfig;
use super::{DiscoveredDevice, NetworkInterfaceOption};
use crate::settings::Settings;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub(super) const SERVICE_TYPE: &str = "_lan-clipboard._tcp.local.";
pub(super) const UDP_DISCOVERY_PORT: u16 = 32911;
const DISCOVERY_REACHABILITY_TIMEOUT_MS: u64 = 220;

pub(super) fn build_service_info(config: &PresenceConfig) -> anyhow::Result<ServiceInfo> {
    let local_ip = resolve_local_ip_override(&config.local_ip)?.unwrap_or(pick_local_ip()?);
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

pub fn discover_devices(
    device_id: &str,
    shared_code: &str,
    selected_local_ip: Option<&str>,
    timeout_ms: u64,
) -> anyhow::Result<Vec<DiscoveredDevice>> {
    let mdns = ServiceDaemon::new()?;
    let receiver = mdns.browse(SERVICE_TYPE)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut devices = Vec::new();
    let mut seen = HashSet::new();
    let selected_ipv4 = parse_selected_ipv4(selected_local_ip)?;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = match receiver.recv_timeout(remaining) {
            Ok(value) => value,
            Err(_) => break,
        };
        if let ServiceEvent::ServiceResolved(info) = event {
            if let Some(found) = info_to_device(&info, device_id, shared_code, selected_ipv4) {
                let dedupe_key = format!("{}:{}:{}", found.device_id, found.addr, found.port);
                if seen.insert(dedupe_key) {
                    devices.push(found);
                }
            }
        }
    }

    shutdown_discovery_daemon(&mdns, receiver);
    Ok(filter_reachable_discovered_devices(
        devices,
        DISCOVERY_REACHABILITY_TIMEOUT_MS,
    ))
}

pub fn list_network_interfaces() -> Vec<NetworkInterfaceOption> {
    let Ok(all) = local_ip_address::list_afinet_netifas() else {
        return Vec::new();
    };

    let mut interfaces: Vec<NetworkInterfaceOption> = all
        .into_iter()
        .filter_map(|(name, ip)| {
            let IpAddr::V4(ipv4) = ip else {
                return None;
            };
            if !is_usable_ipv4(ipv4) {
                return None;
            }
            let label = if is_private_lan_ipv4(ipv4) {
                format!("{name} ({ipv4})")
            } else {
                format!("{name} ({ipv4}, 非局域网优先)")
            };
            Some(NetworkInterfaceOption {
                name,
                ip: ipv4.to_string(),
                label,
            })
        })
        .collect();

    interfaces.sort_by(|left, right| {
        let left_ip: std::net::Ipv4Addr =
            left.ip.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
        let right_ip: std::net::Ipv4Addr =
            right.ip.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
        is_private_lan_ipv4(right_ip)
            .cmp(&is_private_lan_ipv4(left_ip))
            .then_with(|| {
                is_likely_virtual_interface(&left.name)
                    .cmp(&is_likely_virtual_interface(&right.name))
            })
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.ip.cmp(&right.ip))
    });
    interfaces.dedup_by(|left, right| left.ip == right.ip);
    interfaces
}

pub(super) fn selected_or_active_local_ip(
    settings: &Settings,
    selected_local_ip: Option<&str>,
    active_local_ip: Option<String>,
) -> Option<String> {
    resolve_local_ip_override(selected_local_ip.unwrap_or(settings.sync.local_ip.as_str()))
        .ok()
        .flatten()
        .map(|ip| ip.to_string())
        .or(active_local_ip)
        .or_else(|| pick_local_ip().ok().map(|ip| ip.to_string()))
}

pub(super) fn resolve_local_ip_override(value: &str) -> anyhow::Result<Option<IpAddr>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let parsed: IpAddr = trimmed
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid selected local ip: {trimmed}"))?;
    match parsed {
        IpAddr::V4(ipv4) if is_usable_ipv4(ipv4) => {
            if is_ipv4_assigned_locally(ipv4) {
                Ok(Some(IpAddr::V4(ipv4)))
            } else {
                Ok(None)
            }
        }
        IpAddr::V4(_) => Err(anyhow::anyhow!(
            "selected local ip is not usable: {trimmed}"
        )),
        IpAddr::V6(_) => Err(anyhow::anyhow!("selected local ip must be ipv4: {trimmed}")),
    }
}

pub(super) fn filter_devices_for_local_ip(
    devices: Vec<DiscoveredDevice>,
    selected_local_ip: Option<&str>,
) -> Vec<DiscoveredDevice> {
    let Ok(Some(selected_ipv4)) = parse_selected_ipv4(selected_local_ip) else {
        return devices;
    };

    devices
        .into_iter()
        .filter(|device| device_matches_selected_subnet(device, selected_ipv4))
        .collect()
}

pub(super) fn udp_broadcast_targets(selected_local_ip: &str) -> Vec<SocketAddr> {
    let mut targets = HashSet::new();
    targets.insert(SocketAddr::from(([255, 255, 255, 255], UDP_DISCOVERY_PORT)));

    if let Ok(Some(IpAddr::V4(ipv4))) = resolve_local_ip_override(selected_local_ip) {
        let [a, b, c, _] = ipv4.octets();
        targets.insert(SocketAddr::from(([a, b, c, 255], UDP_DISCOVERY_PORT)));
        return targets.into_iter().collect();
    }

    if let Ok(all) = local_ip_address::list_afinet_netifas() {
        for (_, ip) in all {
            if let IpAddr::V4(ipv4) = ip {
                if !is_usable_ipv4(ipv4) {
                    continue;
                }
                let [a, b, c, _] = ipv4.octets();
                targets.insert(SocketAddr::from(([a, b, c, 255], UDP_DISCOVERY_PORT)));
            }
        }
    }

    targets.into_iter().collect()
}

pub(super) fn local_device_name(device_id: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        for args in [
            &["--get", "ComputerName"][..],
            &["--get", "LocalHostName"][..],
        ] {
            if let Ok(output) = std::process::Command::new("scutil").args(args).output() {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !name.is_empty() {
                    return name;
                }
            }
        }
    }

    if let Ok(output) = std::process::Command::new("hostname").output() {
        if output.status.success() {
            let hostname = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !hostname.is_empty() {
                return hostname;
            }
        }
    }

    device_id.to_string()
}

pub(super) fn parse_selected_ipv4(
    selected_local_ip: Option<&str>,
) -> anyhow::Result<Option<std::net::Ipv4Addr>> {
    match resolve_local_ip_override(selected_local_ip.unwrap_or_default())? {
        Some(IpAddr::V4(ipv4)) => Ok(Some(ipv4)),
        Some(IpAddr::V6(_)) => Ok(None),
        None => Ok(None),
    }
}

pub(super) fn is_same_subnet(left: std::net::Ipv4Addr, right: std::net::Ipv4Addr) -> bool {
    let left = left.octets();
    let right = right.octets();
    left[0] == right[0] && left[1] == right[1] && left[2] == right[2]
}

pub(super) fn is_usable_ipv4(ipv4: std::net::Ipv4Addr) -> bool {
    !ipv4.is_loopback()
        && !ipv4.is_link_local()
        && !ipv4.is_unspecified()
        && !ipv4.is_broadcast()
        && !is_benchmark_ipv4(ipv4)
        && !is_multicast_ipv4(ipv4)
}

pub(super) fn is_private_lan_ipv4(ipv4: std::net::Ipv4Addr) -> bool {
    let [a, b, _, _] = ipv4.octets();
    a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
}

pub(super) fn device_matches_selected_subnet(
    device: &DiscoveredDevice,
    selected_ipv4: std::net::Ipv4Addr,
) -> bool {
    device
        .addr
        .parse::<std::net::Ipv4Addr>()
        .map(|peer_ipv4| is_same_subnet(selected_ipv4, peer_ipv4))
        .unwrap_or(false)
}

fn filter_reachable_discovered_devices(
    devices: Vec<DiscoveredDevice>,
    timeout_ms: u64,
) -> Vec<DiscoveredDevice> {
    let handles = devices
        .into_iter()
        .map(|device| {
            std::thread::spawn(move || {
                if discovered_device_is_reachable(&device, timeout_ms) {
                    Some(device)
                } else {
                    None
                }
            })
        })
        .collect::<Vec<_>>();

    handles
        .into_iter()
        .filter_map(|handle| handle.join().ok().flatten())
        .collect()
}

fn discovered_device_is_reachable(device: &DiscoveredDevice, timeout_ms: u64) -> bool {
    let Ok(ip) = device.addr.parse::<IpAddr>() else {
        return false;
    };
    let socket_addr = SocketAddr::new(ip, device.port);
    TcpStream::connect_timeout(&socket_addr, Duration::from_millis(timeout_ms)).is_ok()
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
    selected_local_ip: Option<std::net::Ipv4Addr>,
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
    if let Some(selected_ipv4) = selected_local_ip {
        let addr_ipv4: std::net::Ipv4Addr = addr.parse().ok()?;
        if !is_same_subnet(selected_ipv4, addr_ipv4) {
            return None;
        }
    }
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
    pick_best_ipv4(addresses.iter().filter_map(|address| match address {
        IpAddr::V4(ipv4) => Some(*ipv4),
        IpAddr::V6(_) => None,
    }))
    .map(|ip| ip.to_string())
}

fn pick_local_ip() -> anyhow::Result<IpAddr> {
    let all = local_ip_address::list_afinet_netifas()?;
    let candidates = all
        .into_iter()
        .filter_map(|(name, ip)| match ip {
            IpAddr::V4(v4) => Some((name, v4)),
            IpAddr::V6(_) => None,
        })
        .filter(|(name, ipv4)| is_usable_ipv4(*ipv4) && !is_likely_virtual_interface(name));
    pick_best_ipv4(candidates.map(|(_, ip)| ip))
        .map(IpAddr::V4)
        .ok_or_else(|| anyhow::anyhow!("no usable local ipv4 address found"))
}

fn is_likely_virtual_interface(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    #[cfg(target_os = "windows")]
    {
        return lower.contains("vethernet")
            || lower.contains("hyper-v")
            || lower.contains("virtual")
            || lower.contains("vmware")
            || lower.contains("virtualbox")
            || lower.contains("wsl")
            || lower.contains("tailscale")
            || lower.contains("hamachi")
            || lower.contains("tap")
            || lower.contains("tun")
            || lower.contains("loopback");
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = lower;
        false
    }
}

fn is_ipv4_assigned_locally(ipv4: std::net::Ipv4Addr) -> bool {
    let Ok(all) = local_ip_address::list_afinet_netifas() else {
        return true;
    };
    all.into_iter().any(|(_, ip)| match ip {
        IpAddr::V4(found) => found == ipv4,
        IpAddr::V6(_) => false,
    })
}

fn pick_best_ipv4<I>(candidates: I) -> Option<std::net::Ipv4Addr>
where
    I: IntoIterator<Item = std::net::Ipv4Addr>,
{
    let mut preferred = None;
    let mut fallback = None;

    for ipv4 in candidates {
        if !is_usable_ipv4(ipv4) {
            continue;
        }
        if preferred.is_none() && is_private_lan_ipv4(ipv4) {
            preferred = Some(ipv4);
        } else if fallback.is_none() {
            fallback = Some(ipv4);
        }
    }

    preferred.or(fallback)
}

fn is_benchmark_ipv4(ipv4: std::net::Ipv4Addr) -> bool {
    let [a, b, _, _] = ipv4.octets();
    a == 198 && (b == 18 || b == 19)
}

fn is_multicast_ipv4(ipv4: std::net::Ipv4Addr) -> bool {
    let [a, _, _, _] = ipv4.octets();
    (224..=239).contains(&a)
}
