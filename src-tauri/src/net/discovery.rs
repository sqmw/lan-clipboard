use super::crypto::discovery_domain_id_from_secret;
use super::presence::PresenceConfig;
use super::socket::connect_tcp_from;
use super::{DiscoveredDevice, NetworkInterfaceOption};
use crate::settings::Settings;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub(super) const SERVICE_TYPE: &str = "_lan-clipboard._tcp.local.";
pub(super) const UDP_DISCOVERY_PORT: u16 = 32911;
pub(super) const DISCOVERED_DEVICE_LIMIT: usize = 100;
pub(super) const DEVICE_NAME_MAX_BYTES: usize = 128;
const MDNS_RESOLVED_EVENT_LIMIT: usize = 256;
const DISCOVERY_REACHABILITY_WORKER_LIMIT: usize = 8;
const DISCOVERY_REACHABILITY_TIMEOUT_MS: u64 = 220;
const DISCOVERY_REACHABILITY_MAX_BUDGET_MS: u64 = 400;
const DISCOVERY_SHUTDOWN_EVENT_LIMIT: usize = 64;
const DISCOVERY_SHUTDOWN_BUDGET_MS: u64 = 100;

pub(super) fn build_service_info(config: &PresenceConfig) -> anyhow::Result<ServiceInfo> {
    if !is_valid_device_id(&config.device_id)
        || !is_valid_discovery_domain_id(&config.domain_id)
        || config.listen_port < 1024
    {
        return Err(anyhow::anyhow!("invalid local discovery identity"));
    }
    let device_name = normalize_device_name(&config.device_name)
        .ok_or_else(|| anyhow::anyhow!("invalid local discovery device name"))?;
    let local_ip = resolve_local_ip_override(&config.local_ip)?.unwrap_or(pick_local_ip()?);
    let host_name = format!("lan-clipboard-{}.local.", config.device_id);
    let properties = [
        ("device_id", config.device_id.as_str()),
        ("domain_id", config.domain_id.as_str()),
        ("device_name", device_name.as_str()),
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
    shared_secret: &str,
    selected_local_ip: Option<&str>,
    timeout_ms: u64,
) -> anyhow::Result<Vec<DiscoveredDevice>> {
    let started_at = Instant::now();
    let total_budget = Duration::from_millis(timeout_ms);
    let deadline = started_at + total_budget;
    let reachability_budget =
        Duration::from_millis((timeout_ms / 3).min(DISCOVERY_REACHABILITY_MAX_BUDGET_MS));
    let shutdown_budget = Duration::from_millis(
        timeout_ms
            .saturating_sub(reachability_budget.as_millis() as u64)
            .min(DISCOVERY_SHUTDOWN_BUDGET_MS),
    );
    let browse_budget = total_budget
        .saturating_sub(reachability_budget)
        .saturating_sub(shutdown_budget);
    let browse_deadline = started_at + browse_budget;
    let shutdown_deadline = browse_deadline + shutdown_budget;
    let mdns = ServiceDaemon::new()?;
    let receiver = mdns.browse(SERVICE_TYPE)?;
    let mut devices = Vec::new();
    let mut seen = HashSet::new();
    let mut resolved_events = 0usize;
    let selected_ipv4 = parse_selected_ipv4(selected_local_ip)?;
    let domain_id = discovery_domain_id_from_secret(shared_secret);

    while Instant::now() < browse_deadline {
        let remaining = browse_deadline.saturating_duration_since(Instant::now());
        let event = match receiver.recv_timeout(remaining) {
            Ok(value) => value,
            Err(_) => break,
        };
        if let ServiceEvent::ServiceResolved(info) = event {
            resolved_events += 1;
            if resolved_events > MDNS_RESOLVED_EVENT_LIMIT {
                break;
            }
            if let Some(found) = info_to_device(&info, device_id, &domain_id, selected_ipv4) {
                let dedupe_key = format!("{}:{}:{}", found.device_id, found.addr, found.port);
                if seen.insert(dedupe_key) {
                    devices.push(found);
                    if devices.len() >= DISCOVERED_DEVICE_LIMIT {
                        break;
                    }
                }
            }
        }
    }

    shutdown_discovery_daemon(&mdns, receiver, shutdown_deadline);
    Ok(filter_reachable_discovered_devices(
        devices,
        selected_ipv4,
        deadline,
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
        .filter(|device| device_matches_selected_scope(device, selected_ipv4))
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

pub(super) fn is_same_lan_scope(left: std::net::Ipv4Addr, right: std::net::Ipv4Addr) -> bool {
    match (private_lan_scope(left), private_lan_scope(right)) {
        (Some(left_scope), Some(right_scope)) => left_scope == right_scope,
        (Some(_), None) | (None, Some(_)) => false,
        // Without an OS-provided netmask, non-RFC1918 addresses are not
        // prefiltered. Source binding and the reachability probe are decisive.
        (None, None) => true,
    }
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
    private_lan_scope(ipv4).is_some()
}

pub(super) fn device_matches_selected_scope(
    device: &DiscoveredDevice,
    selected_ipv4: std::net::Ipv4Addr,
) -> bool {
    device
        .addr
        .parse::<std::net::Ipv4Addr>()
        .map(|peer_ipv4| is_same_lan_scope(selected_ipv4, peer_ipv4))
        .unwrap_or(false)
}

fn private_lan_scope(ipv4: std::net::Ipv4Addr) -> Option<u8> {
    let [a, b, _, _] = ipv4.octets();
    if a == 10 {
        Some(10)
    } else if a == 172 && (16..=31).contains(&b) {
        Some(172)
    } else if a == 192 && b == 168 {
        Some(192)
    } else {
        None
    }
}

fn filter_reachable_discovered_devices(
    devices: Vec<DiscoveredDevice>,
    selected_local_ip: Option<std::net::Ipv4Addr>,
    deadline: Instant,
) -> Vec<DiscoveredDevice> {
    let selected_local_ip = selected_local_ip.map(|ip| ip.to_string());
    filter_reachable_with(devices, deadline, move |device, remaining| {
        discovered_device_is_reachable(
            device,
            selected_local_ip.as_deref(),
            remaining.min(Duration::from_millis(DISCOVERY_REACHABILITY_TIMEOUT_MS)),
        )
    })
}

fn filter_reachable_with<F>(
    devices: Vec<DiscoveredDevice>,
    deadline: Instant,
    is_reachable: F,
) -> Vec<DiscoveredDevice>
where
    F: Fn(&DiscoveredDevice, Duration) -> bool + Send + Sync + 'static,
{
    let worker_count = devices.len().min(DISCOVERY_REACHABILITY_WORKER_LIMIT);
    if worker_count == 0 {
        return Vec::new();
    }

    let queue = Arc::new(Mutex::new(VecDeque::from(devices)));
    let reachable = Arc::new(Mutex::new(Vec::new()));
    let is_reachable = Arc::new(is_reachable);
    let handles = (0..worker_count)
        .map(|_| {
            let queue = Arc::clone(&queue);
            let reachable = Arc::clone(&reachable);
            let is_reachable = Arc::clone(&is_reachable);
            std::thread::spawn(move || loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return;
                }
                let device = match queue.lock() {
                    Ok(mut queue) => queue.pop_front(),
                    Err(_) => return,
                };
                let Some(device) = device else {
                    return;
                };
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return;
                }
                if is_reachable(&device, remaining) {
                    if let Ok(mut reachable) = reachable.lock() {
                        reachable.push(device);
                    } else {
                        return;
                    }
                }
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        let _ = handle.join();
    }
    let mut devices = reachable
        .lock()
        .map(|devices| devices.clone())
        .unwrap_or_default();
    devices.sort_by(|left, right| {
        left.device_name
            .cmp(&right.device_name)
            .then_with(|| left.device_id.cmp(&right.device_id))
    });
    devices
}

fn discovered_device_is_reachable(
    device: &DiscoveredDevice,
    selected_local_ip: Option<&str>,
    timeout: Duration,
) -> bool {
    let Ok(ip) = device.addr.parse::<IpAddr>() else {
        return false;
    };
    let socket_addr = SocketAddr::new(ip, device.port);
    connect_tcp_from(&socket_addr, selected_local_ip, timeout).is_ok()
}

fn shutdown_discovery_daemon(
    mdns: &ServiceDaemon,
    receiver: mdns_sd::Receiver<ServiceEvent>,
    deadline: Instant,
) {
    let _ = mdns.stop_browse(SERVICE_TYPE);
    for _ in 0..DISCOVERY_SHUTDOWN_EVENT_LIMIT {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(event) = receiver.recv_timeout(remaining.min(Duration::from_millis(100))) else {
            break;
        };
        if matches!(event, ServiceEvent::SearchStopped(_)) {
            break;
        }
    }
    if let Ok(status_rx) = mdns.shutdown() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            let _ = status_rx.recv_timeout(remaining);
        }
    }
}

fn info_to_device(
    info: &ServiceInfo,
    self_device_id: &str,
    domain_id: &str,
    selected_local_ip: Option<std::net::Ipv4Addr>,
) -> Option<DiscoveredDevice> {
    let device_id = info.get_fullname().split('.').next()?.to_string();
    let property_device_id = info.get_properties().get_property_val_str("device_id")?;
    if device_id != property_device_id
        || device_id == self_device_id
        || !is_valid_device_id(&device_id)
        || info.get_port() < 1024
    {
        return None;
    }

    let found_domain_id = info
        .get_properties()
        .get_property_val_str("domain_id")
        .unwrap_or("");
    if found_domain_id != domain_id
        || !is_valid_discovery_domain_id(found_domain_id)
        || !is_valid_discovery_domain_id(domain_id)
    {
        return None;
    }

    let addr = pick_ipv4(info.get_addresses())?;
    if let Some(selected_ipv4) = selected_local_ip {
        let addr_ipv4: std::net::Ipv4Addr = addr.parse().ok()?;
        if !is_same_lan_scope(selected_ipv4, addr_ipv4) {
            return None;
        }
    }
    let device_name = normalize_device_name(
        info.get_properties()
            .get_property_val_str("device_name")
            .unwrap_or_default(),
    )?;

    let device = DiscoveredDevice {
        device_id,
        device_name,
        addr,
        port: info.get_port(),
    };
    validate_discovered_device(&device).then_some(device)
}

pub(super) fn is_valid_device_id(value: &str) -> bool {
    value.len() == 36
        && Uuid::parse_str(value)
            .map(|uuid| !uuid.is_nil() && uuid.hyphenated().to_string() == value)
            .unwrap_or(false)
}

pub(super) fn is_valid_discovery_domain_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn normalize_device_name(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()
        && trimmed.len() <= DEVICE_NAME_MAX_BYTES
        && !trimmed.chars().any(char::is_control))
    .then(|| trimmed.to_string())
}

pub(super) fn validate_discovered_device(device: &DiscoveredDevice) -> bool {
    if !is_valid_device_id(&device.device_id)
        || normalize_device_name(&device.device_name).as_deref()
            != Some(device.device_name.as_str())
        || device.port < 1024
    {
        return false;
    }
    device
        .addr
        .parse::<IpAddr>()
        .map(|address| match address {
            IpAddr::V4(ipv4) => is_usable_ipv4(ipv4),
            IpAddr::V6(_) => false,
        })
        .unwrap_or(false)
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
        lower.contains("vethernet")
            || lower.contains("hyper-v")
            || lower.contains("virtual")
            || lower.contains("vmware")
            || lower.contains("virtualbox")
            || lower.contains("wsl")
            || lower.contains("tailscale")
            || lower.contains("hamachi")
            || lower.contains("tap")
            || lower.contains("tun")
            || lower.contains("loopback")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_device(index: usize) -> DiscoveredDevice {
        DiscoveredDevice {
            device_id: Uuid::from_u128(index as u128 + 1).hyphenated().to_string(),
            device_name: format!("device-{index}"),
            addr: "192.168.1.20".to_string(),
            port: 32910,
        }
    }

    #[test]
    fn validates_discovery_identity_fields() {
        assert!(is_valid_device_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!is_valid_device_id("550E8400-E29B-41D4-A716-446655440000"));
        assert!(!is_valid_device_id("not-a-uuid"));
        assert!(is_valid_discovery_domain_id(
            "0123456789abcdef0123456789abcdef"
        ));
        assert!(!is_valid_discovery_domain_id(
            "0123456789ABCDEF0123456789ABCDEF"
        ));
        assert_eq!(normalize_device_name(" laptop ").as_deref(), Some("laptop"));
        assert!(normalize_device_name("bad\nname").is_none());
        assert!(normalize_device_name(&"x".repeat(DEVICE_NAME_MAX_BYTES + 1)).is_none());
    }

    #[test]
    fn validates_complete_discovered_device() {
        assert!(validate_discovered_device(&test_device(1)));
        let mut invalid = test_device(2);
        invalid.port = 0;
        assert!(!validate_discovered_device(&invalid));
        invalid = test_device(3);
        invalid.addr = "127.0.0.1".to_string();
        assert!(!validate_discovered_device(&invalid));
    }

    #[test]
    fn private_scope_prefilter_does_not_assume_a_slash_24_netmask() {
        assert!(is_same_lan_scope(
            "10.20.1.4".parse().expect("left /16 address"),
            "10.20.200.9".parse().expect("right /16 address"),
        ));
        assert!(is_same_lan_scope(
            "192.168.0.4".parse().expect("left /23 address"),
            "192.168.1.9".parse().expect("right /23 address"),
        ));
        assert!(!is_same_lan_scope(
            "10.20.1.4".parse().expect("10/8 address"),
            "192.168.1.9".parse().expect("192.168/16 address"),
        ));
        assert!(is_same_lan_scope(
            "100.64.0.4".parse().expect("non-RFC1918 left address"),
            "100.64.8.9".parse().expect("non-RFC1918 right address"),
        ));
    }

    #[test]
    fn reachability_probe_uses_bounded_worker_pool() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let active_for_probe = Arc::clone(&active);
        let peak_for_probe = Arc::clone(&peak);
        let devices = (0..32).map(test_device).collect::<Vec<_>>();

        let found = filter_reachable_with(
            devices,
            Instant::now() + Duration::from_secs(1),
            move |_, _| {
                let current = active_for_probe.fetch_add(1, Ordering::SeqCst) + 1;
                peak_for_probe.fetch_max(current, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(2));
                active_for_probe.fetch_sub(1, Ordering::SeqCst);
                true
            },
        );

        assert_eq!(found.len(), 32);
        assert!(peak.load(Ordering::SeqCst) <= DISCOVERY_REACHABILITY_WORKER_LIMIT);
    }

    #[test]
    fn reachability_probe_stops_dequeuing_when_total_budget_expires() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_probe = Arc::clone(&attempts);
        let devices = (0..DISCOVERED_DEVICE_LIMIT)
            .map(test_device)
            .collect::<Vec<_>>();
        let started_at = Instant::now();

        let found = filter_reachable_with(
            devices,
            started_at + Duration::from_millis(30),
            move |_, remaining| {
                attempts_for_probe.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(remaining.min(Duration::from_millis(30)));
                false
            },
        );

        assert!(found.is_empty());
        assert!(attempts.load(Ordering::SeqCst) <= DISCOVERY_REACHABILITY_WORKER_LIMIT);
        assert!(started_at.elapsed() < Duration::from_millis(500));
    }
}
