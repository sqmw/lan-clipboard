use super::dedupe::{
    clear_content_inflight, mark_content_inflight, mark_shared_fingerprint, recent_event_seen,
    register_ignored_local_hash, register_recent_event, remember_local_observation,
    should_drop_duplicate_outbound,
};
use super::display::{payload_label, payload_summary};
use super::domain::prune_stale_queue_entries;
use super::logs::{push_log, set_error};
use super::marker::{is_stale_marker, item_marker, update_latest_item};
use super::metrics::{elapsed_ms, now_ms};
use super::queue::{
    new_queue_entry, pop_ready_outbound_entry, pop_ready_queue_entry, push_queue_entry,
    schedule_retry, QueueLane,
};
use super::sender::send_to_all_peers;
use super::transfers::{
    find_receive_transfer_id, mark_transfer_completed, mark_transfer_failed,
    update_transfer_metadata, update_transfer_status,
};
use super::RuntimeInner;
use crate::clipboard;
use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::Settings;
use std::sync::atomic::Ordering;
use std::time::Instant;

pub(super) fn process_inbound_queue(runtime: &RuntimeInner, settings: &Settings) -> bool {
    let mut did_work = false;
    loop {
        if runtime.stop_flag.load(Ordering::SeqCst) {
            break;
        }
        let Some(mut entry) = pop_ready_queue_entry(&runtime.inbound_queue) else {
            break;
        };
        did_work = true;
        if is_stale_marker(runtime, &item_marker(&entry.item)) {
            remove_internal_file_payload(runtime, &entry.item.payload, "drop stale inbound item");
            if let Some(transfer_id) = find_receive_transfer_id(runtime, &entry.item.id) {
                mark_transfer_failed(runtime, &transfer_id, "已被更新内容替代".to_string());
            }
            continue;
        }

        register_ignored_local_hash(runtime, &entry.item.content_hash);
        let transfer_id = find_receive_transfer_id(runtime, &entry.item.id);
        if let Some(transfer_id) = transfer_id.as_deref() {
            update_transfer_metadata(
                runtime,
                transfer_id,
                entry.item.payload.kind(),
                &payload_label(&entry.item.payload),
                &payload_summary(&entry.item.payload),
                &entry.item.id,
                "applying",
            );
        }
        let apply_started = Instant::now();
        match clipboard::write_item(&entry.item, &settings.limits) {
            Ok(applied) => {
                retire_internal_file_payload(runtime, &entry.item.payload);
                let apply_ms = elapsed_ms(apply_started);
                mark_shared_fingerprint(runtime, &entry.item.content_hash);
                if let Some(local_content_hash) = applied.content_hash.as_deref() {
                    register_ignored_local_hash(runtime, local_content_hash);
                    mark_shared_fingerprint(runtime, local_content_hash);
                }
                if let Some(transfer_id) = transfer_id.as_deref() {
                    mark_transfer_completed(runtime, transfer_id);
                }
                push_log(
                    runtime,
                    "INFO",
                    &format!(
                        "applied item {} kind={} size_bytes={} from {} after {} attempt(s)",
                        entry.item.id,
                        entry.item.payload.kind(),
                        entry.item.size_bytes,
                        entry.item.source_device_id,
                        entry.attempts + 1
                    ),
                );
                push_log(
                    runtime,
                    "DEBUG",
                    &format!(
                        "profile clipboard_apply item={} kind={} size_bytes={} apply_ms={} attempts={}",
                        entry.item.id,
                        entry.item.payload.kind(),
                        entry.item.size_bytes,
                        apply_ms,
                        entry.attempts + 1
                    ),
                )
            }
            Err(crate::clipboard::ClipboardError::Backend(error)) => {
                let apply_ms = elapsed_ms(apply_started);
                push_log(
                    runtime,
                    "DEBUG",
                    &format!(
                        "profile clipboard_apply item={} kind={} size_bytes={} apply_ms={} attempts={} success=false error={}",
                        entry.item.id,
                        entry.item.payload.kind(),
                        entry.item.size_bytes,
                        apply_ms,
                        entry.attempts + 1,
                        error
                    ),
                );
                if schedule_retry(&mut entry) {
                    if let Some(transfer_id) = transfer_id.as_deref() {
                        update_transfer_status(
                            runtime,
                            transfer_id,
                            "retrying",
                            Some(error.clone()),
                        );
                    }
                    push_log(
                        runtime,
                        "WARN",
                        &format!(
                            "apply retry queued for item {}: {} (attempt={})",
                            entry.item.id, error, entry.attempts
                        ),
                    );
                    push_queue_entry(&runtime.inbound_queue, entry);
                } else {
                    remove_internal_file_payload(
                        runtime,
                        &entry.item.payload,
                        "discard failed inbound item",
                    );
                    if let Some(transfer_id) = transfer_id.as_deref() {
                        mark_transfer_failed(runtime, transfer_id, error.clone());
                    }
                    set_error(
                        runtime,
                        format!(
                            "apply clipboard item failed after retries: item={} error={error}",
                            entry.item.id
                        ),
                    );
                }
            }
            Err(error) => {
                remove_internal_file_payload(
                    runtime,
                    &entry.item.payload,
                    "discard invalid inbound item",
                );
                let apply_ms = elapsed_ms(apply_started);
                push_log(
                    runtime,
                    "DEBUG",
                    &format!(
                        "profile clipboard_apply item={} kind={} size_bytes={} apply_ms={} attempts={} success=false error={}",
                        entry.item.id,
                        entry.item.payload.kind(),
                        entry.item.size_bytes,
                        apply_ms,
                        entry.attempts + 1,
                        error
                    ),
                );
                if let Some(transfer_id) = transfer_id.as_deref() {
                    mark_transfer_failed(runtime, transfer_id, error.to_string());
                }
                set_error(
                    runtime,
                    format!(
                        "apply clipboard item failed permanently: item={} error={error}",
                        entry.item.id
                    ),
                )
            }
        }
    }
    did_work
}

fn retire_internal_file_payload(runtime: &RuntimeInner, payload: &ClipboardPayload) {
    if let Err(error) = clipboard::retire_internal_file_payload(payload) {
        push_log(
            runtime,
            "WARN",
            &format!("failed to retire applied file payload: {error}"),
        );
    }
}

fn remove_internal_file_payload(runtime: &RuntimeInner, payload: &ClipboardPayload, context: &str) {
    if let Err(error) = clipboard::remove_internal_file_payload(payload) {
        push_log(runtime, "WARN", &format!("{context}: {error}"));
    }
}

pub(super) fn process_outbound_queue(
    runtime: &RuntimeInner,
    settings: &Settings,
    allowed_lanes: &[QueueLane],
) -> bool {
    let mut did_work = false;
    loop {
        if runtime.stop_flag.load(Ordering::SeqCst) {
            break;
        }
        let Some(mut entry) = pop_ready_outbound_entry(&runtime.outbound_queue, allowed_lanes)
        else {
            break;
        };
        did_work = true;
        if is_stale_marker(runtime, &item_marker(&entry.item)) {
            clear_content_inflight(runtime, &entry.item.content_hash);
            push_log(
                runtime,
                "DEBUG",
                &format!("drop stale outbound item {}", entry.item.id),
            );
            continue;
        }

        let report = send_to_all_peers(
            runtime,
            settings,
            &entry.item,
            entry.pending_peers.as_deref(),
        );
        if report.attempted == 0 {
            clear_content_inflight(runtime, &entry.item.content_hash);
            push_log(
                runtime,
                "DEBUG",
                &format!(
                    "drop outbound item {} because shared domain only contains self",
                    entry.item.id
                ),
            );
            continue;
        }
        if report.deferred {
            push_log(
                runtime,
                "DEBUG",
                &format!("outbound item {} deferred to file sender", entry.item.id),
            );
            continue;
        }
        if !report.failed_peers.is_empty() {
            entry.pending_peers = Some(report.failed_peers);
            if schedule_retry(&mut entry) {
                push_log(
                    runtime,
                    "DEBUG",
                    &format!(
                        "outbound item {} pending peers delivered={delivered} attempted={attempted} remaining={} retry={}",
                        entry.item.id,
                        entry.pending_peers.as_ref().map_or(0, Vec::len),
                        entry.attempts,
                        delivered = report.delivered,
                        attempted = report.attempted
                    ),
                );
                push_queue_entry(&runtime.outbound_queue, entry);
                continue;
            }
            push_log(
                runtime,
                "WARN",
                &format!(
                    "drop outbound item {} after retries delivered={delivered} attempted={attempted}",
                    entry.item.id,
                    delivered = report.delivered,
                    attempted = report.attempted
                ),
            );
            clear_content_inflight(runtime, &entry.item.content_hash);
            continue;
        }

        mark_shared_fingerprint(runtime, &entry.item.content_hash);
        clear_content_inflight(runtime, &entry.item.content_hash);
        push_log(
            runtime,
            "DEBUG",
            &format!(
                "outbound item {} completed delivered={delivered} attempted={attempted}",
                entry.item.id,
                delivered = report.delivered,
                attempted = report.attempted
            ),
        );
    }
    did_work
}

pub(super) fn should_skip_remote_item(runtime: &RuntimeInner, item: &ClipboardItem) -> bool {
    if recent_event_seen(runtime, &item.id) {
        return true;
    }

    let marker = item_marker(item);
    let should_skip = is_stale_marker(runtime, &marker);

    if should_skip {
        return true;
    }

    register_recent_event(runtime, &item.id);
    if update_latest_item(runtime, item) {
        prune_stale_queue_entries(runtime);
    }
    false
}

pub(super) fn enqueue_outbound_item(runtime: &RuntimeInner, item: ClipboardItem) {
    if should_drop_duplicate_outbound(runtime, &item) {
        remember_local_observation(runtime, &item.content_hash, now_ms());
        push_log(
            runtime,
            "DEBUG",
            &format!(
                "drop duplicate outbound item {} kind={} fingerprint={}",
                item.id,
                item.payload.kind(),
                item.content_hash
            ),
        );
        return;
    }

    if update_latest_item(runtime, &item) {
        prune_stale_queue_entries(runtime);
    }
    let item_id = item.id.clone();
    let kind = item.payload.kind();
    let size_bytes = item.size_bytes;
    mark_content_inflight(runtime, &item.content_hash);
    push_queue_entry(&runtime.outbound_queue, new_queue_entry(item));
    push_log(
        runtime,
        "DEBUG",
        &format!("queued outbound item {item_id} kind={kind} size_bytes={size_bytes}"),
    );
}

pub(super) fn enqueue_inbound_item(
    runtime: &RuntimeInner,
    item: ClipboardItem,
    transfer_id: &str,
    peer: &str,
) {
    if update_latest_item(runtime, &item) {
        prune_stale_queue_entries(runtime);
    }
    let item_id = item.id.clone();
    let source = item.source_device_id.clone();
    let kind = item.payload.kind();
    let size_bytes = item.size_bytes;
    if let Ok(mut guard) = runtime.transfers.lock() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.id == transfer_id) {
            entry.peer = peer.to_string();
            entry.item_kind = kind.to_string();
            entry.item_label = payload_label(&item.payload);
            entry.item_summary = payload_summary(&item.payload);
            entry.item_id = item_id.clone();
            entry.transferred_bytes = size_bytes;
            entry.total_bytes = size_bytes;
            entry.percent = 100;
            entry.status = "queued".to_string();
            entry.updated_at_ms = now_ms();
            entry.error = None;
        }
    }
    push_queue_entry(&runtime.inbound_queue, new_queue_entry(item));
    push_log(
        runtime,
        "DEBUG",
        &format!("queued inbound item {item_id} kind={kind} size_bytes={size_bytes} from {source}"),
    );
}
