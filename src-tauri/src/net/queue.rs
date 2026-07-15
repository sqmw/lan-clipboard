use super::metrics::now_ms;
use super::RuntimeInner;
use crate::protocol::{ClipboardItem, ClipboardPayload};
use std::collections::VecDeque;
use std::sync::Mutex;

const QUEUE_RETRY_BASE_MS: u64 = 30;
const QUEUE_RETRY_MAX_MS: u64 = 500;
const QUEUE_MAX_RETRIES: u32 = 24;
const QUEUE_MAX_AGE_MS: u64 = 30_000;

#[derive(Debug, Clone)]
pub(super) struct QueueEntry {
    pub(super) item: ClipboardItem,
    pub(super) attempts: u32,
    pub(super) queued_at_ms: u64,
    pub(super) pending_peers: Option<Vec<String>>,
    next_attempt_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum QueueLane {
    Realtime,
    Visual,
    Bulk,
}

pub(super) fn pop_ready_queue_entry(queue: &Mutex<VecDeque<QueueEntry>>) -> Option<QueueEntry> {
    let now = now_ms();
    queue.lock().ok().and_then(|mut guard| {
        let position = guard
            .iter()
            .position(|entry| entry.next_attempt_at_ms <= now)?;
        guard.remove(position)
    })
}

pub(super) fn pop_ready_outbound_entry(
    queue: &Mutex<VecDeque<QueueEntry>>,
    allowed_lanes: &[QueueLane],
) -> Option<QueueEntry> {
    let now = now_ms();
    queue.lock().ok().and_then(|mut guard| {
        let position = guard
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.next_attempt_at_ms <= now)
            .filter(|(_, entry)| allowed_lanes.contains(&outbound_lane(entry)))
            .max_by(|(_, left), (_, right)| compare_outbound_entries(left, right))?
            .0;
        guard.remove(position)
    })
}

pub(super) fn has_ready_outbound_lane(runtime: &RuntimeInner, lanes: &[QueueLane]) -> bool {
    let now = now_ms();
    runtime
        .outbound_queue
        .lock()
        .map(|guard| {
            guard.iter().any(|entry| {
                entry.next_attempt_at_ms <= now && lanes.contains(&outbound_lane(entry))
            })
        })
        .unwrap_or(false)
}

pub(super) fn push_queue_entry(queue: &Mutex<VecDeque<QueueEntry>>, entry: QueueEntry) {
    if let Ok(mut guard) = queue.lock() {
        guard.push_back(entry);
    }
}

fn compare_outbound_entries(left: &QueueEntry, right: &QueueEntry) -> std::cmp::Ordering {
    outbound_phase_rank(left)
        .cmp(&outbound_phase_rank(right))
        .reverse()
        .then_with(|| outbound_lane(left).cmp(&outbound_lane(right)).reverse())
        .then_with(|| left.item.created_at_us.cmp(&right.item.created_at_us))
        .then_with(|| right.attempts.cmp(&left.attempts))
        .then_with(|| left.queued_at_ms.cmp(&right.queued_at_ms))
}

fn outbound_phase_rank(entry: &QueueEntry) -> u8 {
    if entry.attempts == 0 {
        1
    } else {
        0
    }
}

fn outbound_lane(entry: &QueueEntry) -> QueueLane {
    match &entry.item.payload {
        ClipboardPayload::Text { .. }
        | ClipboardPayload::Html { .. }
        | ClipboardPayload::Rtf { .. } => QueueLane::Realtime,
        ClipboardPayload::ImagePng { .. } => QueueLane::Visual,
        ClipboardPayload::FileBundleDir { .. } | ClipboardPayload::FileList { .. } => {
            QueueLane::Bulk
        }
    }
}

pub(super) fn schedule_retry(entry: &mut QueueEntry) -> bool {
    if entry.attempts >= QUEUE_MAX_RETRIES {
        return false;
    }
    if now_ms().saturating_sub(entry.queued_at_ms) >= QUEUE_MAX_AGE_MS {
        return false;
    }

    entry.attempts += 1;
    let retry_delay_ms =
        (QUEUE_RETRY_BASE_MS.saturating_mul(entry.attempts as u64)).min(QUEUE_RETRY_MAX_MS);
    entry.next_attempt_at_ms = now_ms().saturating_add(retry_delay_ms);
    true
}

pub(super) fn new_queue_entry(item: ClipboardItem) -> QueueEntry {
    let queued_at_ms = now_ms();
    QueueEntry {
        item,
        attempts: 0,
        queued_at_ms,
        pending_peers: None,
        next_attempt_at_ms: queued_at_ms,
    }
}
