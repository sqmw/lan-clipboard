use crate::protocol::ClipboardItem;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub(super) fn percent_for(transferred_bytes: u64, total_bytes: u64) -> u8 {
    if total_bytes == 0 {
        return 0;
    }
    ((transferred_bytes.saturating_mul(100) / total_bytes).min(100)) as u8
}

pub(super) fn elapsed_ms(started_at: Instant) -> u128 {
    started_at.elapsed().as_millis()
}

pub(super) fn item_age_ms(item: &ClipboardItem) -> u64 {
    now_us()
        .saturating_sub(item.created_at_us)
        .saturating_div(1_000)
}

pub(super) fn format_mib_per_second(bytes: u64, elapsed_ms: u128) -> String {
    if elapsed_ms == 0 {
        return "0.00".to_string();
    }
    let mib_per_second = bytes as f64 / 1024.0 / 1024.0 / (elapsed_ms as f64 / 1000.0);
    format!("{mib_per_second:.2}")
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_micros() as u64)
        .unwrap_or(0)
}
