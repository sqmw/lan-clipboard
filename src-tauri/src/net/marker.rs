use super::wire::{FileStreamStart, ImageStreamStart};
use super::RuntimeInner;
use crate::protocol::ClipboardItem;

#[derive(Debug, Clone)]
pub(super) struct ItemMarker {
    pub(super) id: String,
    pub(super) created_at_us: u64,
    pub(super) source_device_id: String,
}

pub(super) fn item_marker(item: &ClipboardItem) -> ItemMarker {
    ItemMarker {
        id: item.id.clone(),
        created_at_us: item.created_at_us,
        source_device_id: item.source_device_id.clone(),
    }
}

pub(super) fn file_stream_marker(meta: &FileStreamStart) -> ItemMarker {
    ItemMarker {
        id: meta.item_id.clone(),
        created_at_us: meta.created_at_us,
        source_device_id: meta.source_device_id.clone(),
    }
}

pub(super) fn image_stream_marker(meta: &ImageStreamStart) -> ItemMarker {
    ItemMarker {
        id: meta.item_id.clone(),
        created_at_us: meta.created_at_us,
        source_device_id: meta.source_device_id.clone(),
    }
}

pub(super) fn compare_markers(left: &ItemMarker, right: &ItemMarker) -> std::cmp::Ordering {
    left.created_at_us
        .cmp(&right.created_at_us)
        .then_with(|| left.source_device_id.cmp(&right.source_device_id))
        .then_with(|| left.id.cmp(&right.id))
}

pub(super) fn update_latest_marker(runtime: &RuntimeInner, marker: ItemMarker) -> bool {
    if let Ok(mut guard) = runtime.latest_item.lock() {
        let replace = guard
            .as_ref()
            .map(|current| compare_markers(&marker, current).is_gt())
            .unwrap_or(true);
        if replace {
            *guard = Some(marker);
        }
        return replace;
    }
    false
}

pub(super) fn update_latest_item(runtime: &RuntimeInner, item: &ClipboardItem) -> bool {
    update_latest_marker(runtime, item_marker(item))
}

pub(super) fn is_stale_marker(runtime: &RuntimeInner, marker: &ItemMarker) -> bool {
    runtime
        .latest_item
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .map(|current| compare_markers(marker, &current).is_lt())
        .unwrap_or(false)
}
