use super::metrics::{now_ms, percent_for};
use super::RuntimeInner;
use crate::protocol::ClipboardItem;
use serde::Serialize;

const TRANSFER_HISTORY_LIMIT: usize = 24;
const TRANSFER_RETENTION_MS: u64 = 15_000;

#[derive(Debug, Clone, Serialize)]
pub struct TransferProgress {
    pub id: String,
    pub direction: String,
    pub peer: String,
    pub item_kind: String,
    pub item_label: String,
    pub item_summary: String,
    pub item_id: String,
    pub transferred_bytes: u64,
    pub total_bytes: u64,
    pub percent: u8,
    pub status: String,
    pub updated_at_ms: u64,
    pub error: Option<String>,
}

pub(super) fn upsert_transfer(runtime: &RuntimeInner, transfer: TransferProgress) {
    prune_transfers(runtime);
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(existing) = guard.iter_mut().find(|entry| entry.id == transfer.id) {
            *existing = transfer;
        } else {
            guard.push(transfer);
        }
        guard.sort_by(|left, right| right.updated_at_ms.cmp(&left.updated_at_ms));
        if guard.len() > TRANSFER_HISTORY_LIMIT {
            guard.truncate(TRANSFER_HISTORY_LIMIT);
        }
    }
}

pub(super) fn update_transfer_progress(
    runtime: &RuntimeInner,
    transfer_id: &str,
    transferred_bytes: u64,
    total_bytes: u64,
) {
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.transferred_bytes = transferred_bytes.min(total_bytes);
            entry.total_bytes = total_bytes;
            entry.percent = percent_for(entry.transferred_bytes, entry.total_bytes);
            entry.updated_at_ms = now_ms();
        }
    }
}

pub(super) fn transfer_should_abort(runtime: &RuntimeInner, transfer_id: &str) -> bool {
    runtime
        .transfers
        .lock()
        .ok()
        .and_then(|guard| guard.iter().find(|entry| entry.id == transfer_id).cloned())
        .map(|entry| entry.status == "failed")
        .unwrap_or(false)
}

pub(super) fn update_transfer_metadata(
    runtime: &RuntimeInner,
    transfer_id: &str,
    item_kind: &str,
    item_label: &str,
    item_summary: &str,
    item_id: &str,
    status: &str,
) {
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.item_kind = item_kind.to_string();
            entry.item_label = item_label.to_string();
            entry.item_summary = item_summary.to_string();
            entry.item_id = item_id.to_string();
            entry.status = status.to_string();
            entry.updated_at_ms = now_ms();
        }
    }
}

pub(super) fn update_transfer_status(
    runtime: &RuntimeInner,
    transfer_id: &str,
    status: &str,
    error: Option<String>,
) {
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.status = status.to_string();
            entry.error = error;
            entry.updated_at_ms = now_ms();
        }
    }
}

pub(super) fn mark_transfer_completed(runtime: &RuntimeInner, transfer_id: &str) {
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.transferred_bytes = entry.total_bytes;
            entry.percent = 100;
            entry.status = "completed".to_string();
            entry.error = None;
            entry.updated_at_ms = now_ms();
        }
    }
}

pub(super) fn mark_transfer_failed(runtime: &RuntimeInner, transfer_id: &str, error: String) {
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.status = "failed".to_string();
            entry.error = Some(error);
            entry.updated_at_ms = now_ms();
        }
    }
}

pub(super) fn has_active_transfers(runtime: &RuntimeInner) -> bool {
    runtime
        .transfers
        .lock()
        .map(|guard| {
            guard.iter().any(|entry| {
                matches!(
                    entry.status.as_str(),
                    "sending" | "receiving" | "queued" | "applying" | "retrying"
                )
            })
        })
        .unwrap_or(false)
}

pub(super) fn find_receive_transfer_id(runtime: &RuntimeInner, item_id: &str) -> Option<String> {
    runtime.transfers.lock().ok().and_then(|guard| {
        guard
            .iter()
            .find(|entry| entry.direction == "receive" && entry.item_id == item_id)
            .map(|entry| entry.id.clone())
    })
}

pub(super) fn canonical_receive_transfer_id(item: &ClipboardItem) -> String {
    format!("recv:{}:{}", item.source_device_id, item.id)
}

pub(super) fn prune_transfers(runtime: &RuntimeInner) {
    let threshold = now_ms().saturating_sub(TRANSFER_RETENTION_MS);
    if let Ok(mut guard) = runtime.transfers.lock() {
        guard.retain(|entry| {
            matches!(
                entry.status.as_str(),
                "sending" | "receiving" | "queued" | "applying" | "retrying"
            ) || entry.updated_at_ms >= threshold
        });
    }
}
