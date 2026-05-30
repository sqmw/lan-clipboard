use super::RuntimeInner;
use crate::protocol::ClipboardItem;
use std::time::{Duration, Instant};

const APPLIED_HASH_TTL_MS: u64 = 10_000;
const RECENT_EVENT_TTL_MS: u64 = 120_000;

#[derive(Debug, Clone)]
pub(super) struct ObservedClipboard {
    pub(super) content_hash: String,
    pub(super) observed_at_ms: u64,
}

pub(super) fn should_ignore_local_observation(
    runtime: &RuntimeInner,
    item: &ClipboardItem,
    observed_at_ms: u64,
) -> bool {
    let content_hash = item.content_hash.as_str();
    if shared_fingerprint_seen(runtime, content_hash)
        || inflight_fingerprint_seen(runtime, content_hash)
    {
        remember_local_observation(runtime, content_hash, observed_at_ms);
        return true;
    }

    if recent_applied_hash_seen(runtime, content_hash) {
        remember_local_observation(runtime, content_hash, observed_at_ms);
        return true;
    }

    if let Ok(mut guard) = runtime.last_local_observed.lock() {
        if let Some(previous) = guard.as_mut() {
            if previous.content_hash == content_hash {
                previous.observed_at_ms = observed_at_ms;
                return true;
            }
        }
        *guard = Some(ObservedClipboard {
            content_hash: content_hash.to_string(),
            observed_at_ms,
        });
    }
    false
}

pub(super) fn remember_local_observation(
    runtime: &RuntimeInner,
    content_hash: &str,
    observed_at_ms: u64,
) {
    if let Ok(mut guard) = runtime.last_local_observed.lock() {
        *guard = Some(ObservedClipboard {
            content_hash: content_hash.to_string(),
            observed_at_ms,
        });
    }
}

pub(super) fn should_drop_duplicate_outbound(runtime: &RuntimeInner, item: &ClipboardItem) -> bool {
    shared_fingerprint_seen(runtime, &item.content_hash)
        || inflight_fingerprint_seen(runtime, &item.content_hash)
        || recent_applied_hash_seen(runtime, &item.content_hash)
}

pub(super) fn mark_content_inflight(runtime: &RuntimeInner, content_hash: &str) {
    if let Ok(mut guard) = runtime.inflight_content_fingerprints.lock() {
        guard.insert(content_hash.to_string());
    }
}

pub(super) fn clear_content_inflight(runtime: &RuntimeInner, content_hash: &str) {
    if let Ok(mut guard) = runtime.inflight_content_fingerprints.lock() {
        guard.remove(content_hash);
    }
}

pub(super) fn mark_shared_fingerprint(runtime: &RuntimeInner, content_hash: &str) {
    if let Ok(mut guard) = runtime.shared_content_fingerprint.lock() {
        *guard = Some(content_hash.to_string());
    }
}

pub(super) fn register_ignored_local_hash(runtime: &RuntimeInner, content_hash: &str) {
    if let Ok(mut guard) = runtime.ignored_local_hashes.lock() {
        guard.insert(content_hash.to_string(), Instant::now());
    }
}

pub(super) fn prune_ignored_local_hashes(runtime: &RuntimeInner) {
    if let Ok(mut guard) = runtime.ignored_local_hashes.lock() {
        guard.retain(|_, seen_at| seen_at.elapsed() < Duration::from_millis(APPLIED_HASH_TTL_MS));
    }
}

pub(super) fn recent_event_seen(runtime: &RuntimeInner, event_id: &str) -> bool {
    runtime
        .recent_event_ids
        .lock()
        .ok()
        .and_then(|guard| guard.get(event_id).copied())
        .map(|seen_at| seen_at.elapsed() < Duration::from_millis(RECENT_EVENT_TTL_MS))
        .unwrap_or(false)
}

pub(super) fn register_recent_event(runtime: &RuntimeInner, event_id: &str) {
    if let Ok(mut guard) = runtime.recent_event_ids.lock() {
        guard.insert(event_id.to_string(), Instant::now());
    }
}

pub(super) fn prune_recent_event_ids(runtime: &RuntimeInner) {
    if let Ok(mut guard) = runtime.recent_event_ids.lock() {
        guard.retain(|_, seen_at| seen_at.elapsed() < Duration::from_millis(RECENT_EVENT_TTL_MS));
    }
}

fn shared_fingerprint_seen(runtime: &RuntimeInner, content_hash: &str) -> bool {
    runtime
        .shared_content_fingerprint
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .map(|fingerprint| fingerprint == content_hash)
        .unwrap_or(false)
}

fn inflight_fingerprint_seen(runtime: &RuntimeInner, content_hash: &str) -> bool {
    runtime
        .inflight_content_fingerprints
        .lock()
        .map(|guard| guard.contains(content_hash))
        .unwrap_or(false)
}

fn recent_applied_hash_seen(runtime: &RuntimeInner, content_hash: &str) -> bool {
    runtime
        .ignored_local_hashes
        .lock()
        .ok()
        .and_then(|guard| guard.get(content_hash).copied())
        .map(|seen_at| seen_at.elapsed() < Duration::from_millis(APPLIED_HASH_TTL_MS))
        .unwrap_or(false)
}
