use super::discovery::{device_matches_selected_subnet, parse_selected_ipv4};
use super::{DiscoveredDevice, RuntimeInner};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const DISCOVERED_DEVICE_LIMIT: usize = 100;
const DISCOVERY_MEMBER_TTL_MS: u64 = 30_000;

pub(super) fn mark_known_member(runtime: &RuntimeInner, kind: &str, member: &str) {
    let value = member.trim();
    if value.is_empty() {
        return;
    }
    if let Ok(mut guard) = runtime.known_members.lock() {
        guard.insert(format!("{kind}:{value}"));
    }
}

pub(super) fn merge_discovered_devices(runtime: &RuntimeInner, devices: Vec<DiscoveredDevice>) {
    if devices.is_empty() {
        return;
    }

    let now = Instant::now();
    if let Ok(mut seen_at) = runtime.discovered_seen_at.lock() {
        for device in &devices {
            if !device.device_id.trim().is_empty() {
                seen_at.insert(device.device_id.clone(), now);
            }
        }
    }

    if let Ok(mut guard) = runtime.discovered_devices.lock() {
        for device in devices {
            if device.device_id.trim().is_empty() {
                continue;
            }
            if let Some(existing) = guard
                .iter_mut()
                .find(|existing| existing.device_id == device.device_id)
            {
                *existing = device;
            } else {
                guard.push(device);
            }
        }
        guard.sort_by(|left, right| left.device_name.cmp(&right.device_name));
        guard.dedup_by(|left, right| left.device_id == right.device_id);
        if guard.len() > DISCOVERED_DEVICE_LIMIT {
            guard.truncate(DISCOVERED_DEVICE_LIMIT);
        }
    }
}

pub(super) fn replace_discovered_devices(
    runtime: &RuntimeInner,
    selected_local_ip: Option<&str>,
    devices: Vec<DiscoveredDevice>,
) {
    let now = Instant::now();
    let selected_ipv4 = parse_selected_ipv4(selected_local_ip).ok().flatten();
    let existing_devices = runtime
        .discovered_devices
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    let new_ids = devices
        .iter()
        .filter(|device| !device.device_id.trim().is_empty())
        .map(|device| device.device_id.clone())
        .collect::<HashSet<_>>();

    if let Ok(mut seen_at) = runtime.discovered_seen_at.lock() {
        if let Some(selected_ipv4) = selected_ipv4 {
            seen_at.retain(|device_id, _| {
                if new_ids.contains(device_id) {
                    return true;
                }
                existing_devices
                    .iter()
                    .find(|device| &device.device_id == device_id)
                    .map(|device| !device_matches_selected_subnet(device, selected_ipv4))
                    .unwrap_or(false)
            });
        } else {
            seen_at.retain(|device_id, _| new_ids.contains(device_id));
        }
        for device in &devices {
            if !device.device_id.trim().is_empty() {
                seen_at.insert(device.device_id.clone(), now);
            }
        }
    }

    if let Ok(mut guard) = runtime.discovered_devices.lock() {
        if let Some(selected_ipv4) = selected_ipv4 {
            guard.retain(|device| {
                !device_matches_selected_subnet(device, selected_ipv4)
                    || new_ids.contains(&device.device_id)
            });
        } else {
            guard.retain(|device| new_ids.contains(&device.device_id));
        }

        for device in devices {
            if device.device_id.trim().is_empty() {
                continue;
            }
            if let Some(existing) = guard
                .iter_mut()
                .find(|existing| existing.device_id == device.device_id)
            {
                *existing = device;
            } else {
                guard.push(device);
            }
        }
        guard.sort_by(|left, right| left.device_name.cmp(&right.device_name));
    }
}

pub(super) fn prune_stale_discovered_devices(runtime: &RuntimeInner) {
    let active_ids = match runtime.discovered_seen_at.lock() {
        Ok(mut seen_at) => {
            seen_at.retain(|_, last_seen| {
                last_seen.elapsed() < Duration::from_millis(DISCOVERY_MEMBER_TTL_MS)
            });
            seen_at.keys().cloned().collect::<HashSet<_>>()
        }
        Err(_) => return,
    };

    if let Ok(mut guard) = runtime.discovered_devices.lock() {
        guard.retain(|device| active_ids.contains(&device.device_id));
    }
}
