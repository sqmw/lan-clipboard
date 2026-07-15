use super::RuntimeInner;
use crate::protocol::ClipboardItem;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub(super) const APPLIED_HASH_TTL: Duration = Duration::from_secs(10);
pub(super) const APPLIED_HASH_LIMIT: usize = 1_024;
pub(super) const RECENT_EVENT_TTL: Duration = Duration::from_secs(120);
pub(super) const RECENT_EVENT_ID_LIMIT: usize = 4_096;

#[derive(Debug)]
pub(super) struct BoundedRecentSet {
    entries: HashMap<String, Instant>,
    insertion_order: VecDeque<(String, Instant)>,
    ttl: Duration,
    capacity: usize,
}

impl BoundedRecentSet {
    pub(super) fn new(ttl: Duration, capacity: usize) -> Self {
        debug_assert!(capacity > 0);
        Self {
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
            ttl,
            capacity,
        }
    }

    fn contains_fresh(&mut self, event_id: &str, now: Instant) -> bool {
        self.prune(now);
        self.entries.contains_key(event_id)
    }

    fn insert(&mut self, event_id: &str, now: Instant) {
        self.prune(now);
        if self.entries.contains_key(event_id) {
            return;
        }
        let event_id = event_id.to_string();
        self.entries.insert(event_id.clone(), now);
        self.insertion_order.push_back((event_id, now));
        self.enforce_capacity();
    }

    fn prune(&mut self, now: Instant) {
        while self
            .insertion_order
            .front()
            .is_some_and(|(_, seen_at)| now.saturating_duration_since(*seen_at) >= self.ttl)
        {
            self.remove_oldest_if_current();
        }
    }

    fn enforce_capacity(&mut self) {
        while self.entries.len() > self.capacity {
            if !self.remove_oldest_if_current() && self.insertion_order.is_empty() {
                break;
            }
        }
    }

    fn remove_oldest_if_current(&mut self) -> bool {
        let Some((event_id, inserted_at)) = self.insertion_order.pop_front() else {
            return false;
        };
        if self.entries.get(&event_id) == Some(&inserted_at) {
            self.entries.remove(&event_id);
            return true;
        }
        false
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.insertion_order.clear();
    }
}

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
        guard.insert(content_hash, Instant::now());
    }
}

pub(super) fn prune_ignored_local_hashes(runtime: &RuntimeInner) {
    if let Ok(mut guard) = runtime.ignored_local_hashes.lock() {
        guard.prune(Instant::now());
    }
}

pub(super) fn recent_event_seen(runtime: &RuntimeInner, event_id: &str) -> bool {
    runtime
        .recent_event_ids
        .lock()
        .ok()
        .map(|mut guard| guard.contains_fresh(event_id, Instant::now()))
        .unwrap_or(false)
}

pub(super) fn register_recent_event(runtime: &RuntimeInner, event_id: &str) {
    if let Ok(mut guard) = runtime.recent_event_ids.lock() {
        guard.insert(event_id, Instant::now());
    }
}

pub(super) fn prune_recent_event_ids(runtime: &RuntimeInner) {
    if let Ok(mut guard) = runtime.recent_event_ids.lock() {
        guard.prune(Instant::now());
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
        .map(|mut guard| guard.contains_fresh(content_hash, Instant::now()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_event_ids_have_a_hard_capacity_and_keep_newest_entries() {
        let runtime = RuntimeInner::default();
        for index in 0..(RECENT_EVENT_ID_LIMIT + 32) {
            register_recent_event(&runtime, &format!("event-{index:05}"));
        }

        let guard = runtime.recent_event_ids.lock().expect("recent event ids");
        assert_eq!(guard.entries.len(), RECENT_EVENT_ID_LIMIT);
        assert_eq!(guard.insertion_order.len(), RECENT_EVENT_ID_LIMIT);
        assert!(!guard.entries.contains_key("event-00000"));
        assert!(guard
            .entries
            .contains_key(&format!("event-{:05}", RECENT_EVENT_ID_LIMIT + 31)));
    }

    #[test]
    fn applied_hashes_have_a_hard_capacity_and_keep_newest_entries() {
        let runtime = RuntimeInner::default();
        for index in 0..(APPLIED_HASH_LIMIT + 32) {
            register_ignored_local_hash(&runtime, &format!("hash-{index:05}"));
        }

        let guard = runtime
            .ignored_local_hashes
            .lock()
            .expect("ignored local hashes");
        assert_eq!(guard.entries.len(), APPLIED_HASH_LIMIT);
        assert_eq!(guard.insertion_order.len(), APPLIED_HASH_LIMIT);
        assert!(!guard.entries.contains_key("hash-00000"));
        assert!(guard
            .entries
            .contains_key(&format!("hash-{:05}", APPLIED_HASH_LIMIT + 31)));
    }

    #[test]
    fn repeated_keys_do_not_grow_the_fifo_sidecar() {
        let runtime = RuntimeInner::default();
        for _ in 0..(RECENT_EVENT_ID_LIMIT * 2) {
            register_recent_event(&runtime, "same-event");
            register_ignored_local_hash(&runtime, "same-hash");
        }

        let event_ids = runtime.recent_event_ids.lock().expect("recent event ids");
        assert_eq!(event_ids.entries.len(), 1);
        assert_eq!(event_ids.insertion_order.len(), 1);
        drop(event_ids);

        let hashes = runtime
            .ignored_local_hashes
            .lock()
            .expect("ignored local hashes");
        assert_eq!(hashes.entries.len(), 1);
        assert_eq!(hashes.insertion_order.len(), 1);
    }
}
