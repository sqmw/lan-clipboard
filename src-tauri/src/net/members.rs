use super::discovery::{
    device_matches_selected_scope, parse_selected_ipv4, validate_discovered_device,
    DISCOVERED_DEVICE_LIMIT,
};
use super::{DiscoveredDevice, RuntimeInner};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const KNOWN_MEMBER_LIMIT: usize = 256;
const KNOWN_MEMBER_MAX_BYTES: usize = 160;
const DISCOVERY_MEMBER_TTL_MS: u64 = 30_000;

pub(super) fn mark_known_member(runtime: &RuntimeInner, kind: &str, member: &str) {
    let value = member.trim();
    if value.is_empty()
        || value.len() > KNOWN_MEMBER_MAX_BYTES
        || !matches!(kind, "device" | "addr")
    {
        return;
    }
    if let Ok(mut guard) = runtime.known_members.lock() {
        let key = format!("{kind}:{value}");
        if guard.contains(&key) {
            return;
        }
        if guard.len() >= KNOWN_MEMBER_LIMIT {
            if let Some(evicted) = guard.iter().min().cloned() {
                guard.remove(&evicted);
            }
        }
        guard.insert(key);
    }
}

pub(super) fn merge_discovered_devices(runtime: &RuntimeInner, devices: Vec<DiscoveredDevice>) {
    let devices = bounded_valid_devices(devices);
    if devices.is_empty() {
        return;
    }

    let now = Instant::now();
    let active_ids = if let Ok(mut seen_at) = runtime.discovered_seen_at.lock() {
        for device in &devices {
            insert_seen_at_bounded(&mut seen_at, device.device_id.clone(), now);
        }
        Some(seen_at.keys().cloned().collect::<HashSet<_>>())
    } else {
        None
    };

    if let Ok(mut guard) = runtime.discovered_devices.lock() {
        if let Some(active_ids) = active_ids.as_ref() {
            guard.retain(|device| active_ids.contains(&device.device_id));
        }
        for device in devices {
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
    let devices = bounded_valid_devices(devices);
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

    let active_ids = if let Ok(mut seen_at) = runtime.discovered_seen_at.lock() {
        if let Some(selected_ipv4) = selected_ipv4 {
            seen_at.retain(|device_id, _| {
                if new_ids.contains(device_id) {
                    return true;
                }
                existing_devices
                    .iter()
                    .find(|device| &device.device_id == device_id)
                    .map(|device| !device_matches_selected_scope(device, selected_ipv4))
                    .unwrap_or(false)
            });
        } else {
            seen_at.retain(|device_id, _| new_ids.contains(device_id));
        }
        for device in &devices {
            insert_seen_at_bounded(&mut seen_at, device.device_id.clone(), now);
        }
        Some(seen_at.keys().cloned().collect::<HashSet<_>>())
    } else {
        None
    };

    if let Ok(mut guard) = runtime.discovered_devices.lock() {
        if let Some(active_ids) = active_ids.as_ref() {
            guard.retain(|device| active_ids.contains(&device.device_id));
        } else if let Some(selected_ipv4) = selected_ipv4 {
            guard.retain(|device| {
                !device_matches_selected_scope(device, selected_ipv4)
                    || new_ids.contains(&device.device_id)
            });
        } else {
            guard.retain(|device| new_ids.contains(&device.device_id));
        }

        for device in devices {
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

fn bounded_valid_devices(devices: Vec<DiscoveredDevice>) -> Vec<DiscoveredDevice> {
    let mut ids = HashSet::new();
    devices
        .into_iter()
        .filter(validate_discovered_device)
        .filter(|device| ids.insert(device.device_id.clone()))
        .take(DISCOVERED_DEVICE_LIMIT)
        .collect()
}

fn insert_seen_at_bounded(seen_at: &mut HashMap<String, Instant>, device_id: String, now: Instant) {
    if let Some(last_seen) = seen_at.get_mut(&device_id) {
        *last_seen = now;
        return;
    }
    if seen_at.len() >= DISCOVERED_DEVICE_LIMIT {
        if let Some(oldest_id) = seen_at
            .iter()
            .min_by(|left, right| left.1.cmp(right.1).then_with(|| left.0.cmp(right.0)))
            .map(|(id, _)| id.clone())
        {
            seen_at.remove(&oldest_id);
        }
    }
    seen_at.insert(device_id, now);
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn device(index: usize) -> DiscoveredDevice {
        DiscoveredDevice {
            device_id: Uuid::from_u128(index as u128 + 1).hyphenated().to_string(),
            device_name: format!("device-{index:03}"),
            addr: format!("192.168.1.{}", index % 200 + 1),
            port: 32910,
        }
    }

    #[test]
    fn discovered_member_caches_remain_bounded() {
        let runtime = RuntimeInner::default();
        merge_discovered_devices(
            &runtime,
            (0..(DISCOVERED_DEVICE_LIMIT * 3)).map(device).collect(),
        );

        assert!(runtime.discovered_devices.lock().unwrap().len() <= DISCOVERED_DEVICE_LIMIT);
        assert!(runtime.discovered_seen_at.lock().unwrap().len() <= DISCOVERED_DEVICE_LIMIT);

        replace_discovered_devices(
            &runtime,
            None,
            (400..(400 + DISCOVERED_DEVICE_LIMIT * 3))
                .map(device)
                .collect(),
        );
        assert!(runtime.discovered_devices.lock().unwrap().len() <= DISCOVERED_DEVICE_LIMIT);
        assert!(runtime.discovered_seen_at.lock().unwrap().len() <= DISCOVERED_DEVICE_LIMIT);
    }

    #[test]
    fn known_member_cache_evicts_at_hard_limit() {
        let runtime = RuntimeInner::default();
        for index in 0..(KNOWN_MEMBER_LIMIT * 3) {
            mark_known_member(&runtime, "addr", &format!("192.168.1.1:{}", 1024 + index));
        }
        assert_eq!(
            runtime.known_members.lock().unwrap().len(),
            KNOWN_MEMBER_LIMIT
        );
    }
}
