use super::dedupe::clear_content_inflight;
use super::discovery::{
    is_private_lan_ipv4, is_same_lan_scope, is_usable_ipv4, parse_selected_ipv4,
    selected_or_active_local_ip,
};
use super::logs::push_log;
use super::marker::{compare_markers, item_marker};
use super::members::prune_stale_discovered_devices;
use super::socket::is_self_socket_addr;
use super::{RuntimeInner, Settings};
use crate::protocol::ClipboardPayload;
use std::net::{IpAddr, SocketAddr};

pub(super) fn collect_peer_targets(runtime: &RuntimeInner, settings: &Settings) -> Vec<String> {
    prune_stale_discovered_devices(runtime);
    let active_local_ip = runtime
        .active_local_ip
        .lock()
        .ok()
        .and_then(|guard| (*guard).clone());
    let effective_local_ip = selected_or_active_local_ip(settings, None, active_local_ip);
    let selected_ipv4 = parse_selected_ipv4(effective_local_ip.as_deref())
        .ok()
        .flatten();
    let self_device_id = settings.sync_device_id();
    let mut peer_by_ip = std::collections::HashMap::new();
    if let Ok(guard) = runtime.discovered_devices.lock() {
        for device in guard.iter() {
            if device.device_id == self_device_id {
                continue;
            }
            if let Some(selected_ipv4) = selected_ipv4 {
                if let Ok(peer_ipv4) = device.addr.parse::<std::net::Ipv4Addr>() {
                    if !is_same_lan_scope(selected_ipv4, peer_ipv4) {
                        continue;
                    }
                }
            }
            let peer = format!("{}:{}", device.addr, device.port);
            if let Ok(socket_addr) = peer.parse::<SocketAddr>() {
                if is_self_socket_addr(&socket_addr, effective_local_ip.as_deref()) {
                    continue;
                }
                peer_by_ip.insert(socket_addr.ip().to_string(), peer);
            }
        }
    }
    let mut peers = peer_by_ip.into_values().collect::<Vec<_>>();
    peers.sort();
    peers
}

pub(super) fn clear_member_cache(runtime: &RuntimeInner) {
    if let Ok(mut guard) = runtime.discovered_devices.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.discovered_seen_at.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.known_members.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.outbound_queue.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.inbound_queue.lock() {
        for entry in guard.iter() {
            remove_internal_file_payload(runtime, &entry.item.payload, "clear inbound queue");
        }
        guard.clear();
    }
    if let Ok(mut guard) = runtime.latest_item.lock() {
        guard.take();
    }
    if let Ok(mut guard) = runtime.shared_content_fingerprint.lock() {
        guard.take();
    }
    if let Ok(mut guard) = runtime.inflight_content_fingerprints.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.last_local_observed.lock() {
        guard.take();
    }
    if let Ok(mut guard) = runtime.ignored_local_hashes.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = runtime.recent_event_ids.lock() {
        guard.clear();
    }
}

pub(super) fn prune_stale_queue_entries(runtime: &RuntimeInner) {
    let latest = runtime
        .latest_item
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let Some(latest) = latest else {
        return;
    };

    let mut dropped_outbound = 0usize;
    if let Ok(mut guard) = runtime.outbound_queue.lock() {
        guard.retain(|entry| {
            let keep = !compare_markers(&item_marker(&entry.item), &latest).is_lt();
            if !keep {
                dropped_outbound += 1;
                clear_content_inflight(runtime, &entry.item.content_hash);
            }
            keep
        });
    }

    let mut dropped_inbound = 0usize;
    if let Ok(mut guard) = runtime.inbound_queue.lock() {
        guard.retain(|entry| {
            let keep = !compare_markers(&item_marker(&entry.item), &latest).is_lt();
            if !keep {
                dropped_inbound += 1;
                remove_internal_file_payload(
                    runtime,
                    &entry.item.payload,
                    "prune stale inbound queue item",
                );
            }
            keep
        });
    }

    if dropped_outbound > 0 || dropped_inbound > 0 {
        push_log(
            runtime,
            "DEBUG",
            &format!(
                "pruned stale queue entries outbound={} inbound={}",
                dropped_outbound, dropped_inbound
            ),
        );
    }
}

fn remove_internal_file_payload(runtime: &RuntimeInner, payload: &ClipboardPayload, context: &str) {
    if let Err(error) = crate::clipboard::remove_internal_file_payload(payload) {
        push_log(runtime, "WARN", &format!("{context}: {error}"));
    }
}

pub(super) fn remember_active_local_ip(runtime: &RuntimeInner, ip: IpAddr) {
    let IpAddr::V4(ipv4) = ip else {
        return;
    };
    if !is_usable_ipv4(ipv4) {
        return;
    }
    if let Ok(mut guard) = runtime.active_local_ip.lock() {
        let should_replace = match guard.as_deref().and_then(|value| value.parse().ok()) {
            Some(existing) => !is_private_lan_ipv4(existing) && is_private_lan_ipv4(ipv4),
            None => true,
        };
        if should_replace {
            *guard = Some(ipv4.to_string());
        }
    }
}
