#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
use clipboard_master::{CallbackResult, ClipboardHandler, Master};

use super::dedupe::{
    prune_ignored_local_hashes, prune_recent_event_ids, register_ignored_local_hash,
    register_recent_event, remember_local_observation, should_ignore_local_observation,
};
use super::logs::{push_log, set_error};
use super::marker::update_latest_item;
use super::metrics::{elapsed_ms, now_ms};
use super::state::RuntimeInner;
use super::{build_item, enqueue_outbound_item};
use crate::clipboard;
use crate::clipboard::clipboard_change_token;
use crate::settings::{Settings, SizeLimits};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CLIPBOARD_WATCH_INTERVAL_MS: u64 = 50;
const CLIPBOARD_WATCH_MAX_INTERVAL_MS: u64 = 500;

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
struct ClipboardWatchHandler {
    runtime: Arc<RuntimeInner>,
    limits: SizeLimits,
    device_id: String,
    poll_interval: Duration,
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
impl ClipboardHandler for ClipboardWatchHandler {
    fn on_clipboard_change(&mut self) -> CallbackResult {
        let _ = process_local_clipboard_observation(&self.runtime, &self.limits, &self.device_id);
        CallbackResult::Next
    }

    fn sleep_interval(&self) -> Duration {
        self.poll_interval
    }
}

pub(super) fn spawn_clipboard_watch_worker(
    runtime: Arc<RuntimeInner>,
    settings: &Settings,
    device_id: &str,
) -> Option<std::thread::JoinHandle<()>> {
    let watcher_runtime = Arc::clone(&runtime);
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let watcher_stop_runtime = Arc::clone(&runtime);
    let watcher_limits = settings.limits.clone();
    let watcher_device_id = device_id.to_string();
    let watcher_poll_interval = Duration::from_millis(CLIPBOARD_WATCH_INTERVAL_MS);
    std::thread::Builder::new()
        .name("lan-clipboard-watch".to_string())
        .spawn(move || {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                run_clipboard_watch_poll_loop(
                    watcher_runtime,
                    watcher_limits,
                    watcher_device_id,
                    watcher_poll_interval,
                );
            }
            #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
            {
                let handler = ClipboardWatchHandler {
                    runtime: watcher_runtime,
                    limits: watcher_limits,
                    device_id: watcher_device_id,
                    poll_interval: watcher_poll_interval,
                };
                let Ok(mut master) = Master::new(handler) else {
                    return;
                };
                let shutdown = master.shutdown_channel();
                let _shutdown_guard = std::thread::Builder::new()
                    .name("lan-clipboard-watch-stop".to_string())
                    .spawn({
                        let runtime = watcher_stop_runtime;
                        move || {
                            while !runtime.stop_flag.load(Ordering::SeqCst) {
                                std::thread::sleep(Duration::from_millis(100));
                            }
                            shutdown.signal();
                        }
                    });
                let _ = master.run();
            }
        })
        .ok()
}

pub(super) fn prune_clipboard_observation_caches(runtime: &RuntimeInner) {
    prune_ignored_local_hashes(runtime);
    prune_recent_event_ids(runtime);
}

fn process_local_clipboard_observation(
    runtime: &RuntimeInner,
    limits: &SizeLimits,
    device_id: &str,
) -> bool {
    let observation_started = Instant::now();
    let snapshot_started = Instant::now();
    let payload = match clipboard::read_snapshot(limits) {
        Ok(payload) => payload,
        Err(clipboard::ClipboardError::Unsupported) => return false,
        Err(error) => {
            set_error(runtime, format!("clipboard watcher read failed: {error}"));
            return false;
        }
    };
    let snapshot_ms = elapsed_ms(snapshot_started);

    if clipboard::is_internal_file_payload(&payload) {
        if let Ok(content_hash) = clipboard::payload_content_hash(&payload) {
            remember_local_observation(runtime, &content_hash, now_ms());
            register_ignored_local_hash(runtime, &content_hash);
        }
        push_log(
            runtime,
            "DEBUG",
            "drop local observation from internal clipboard file payload",
        );
        return false;
    }

    let build_started = Instant::now();
    let item = match build_item(&payload, device_id) {
        Ok(Some(item)) => item,
        Ok(None) => return false,
        Err(error) => {
            set_error(
                runtime,
                format!("clipboard watcher build item failed: {error}"),
            );
            return false;
        }
    };
    let build_ms = elapsed_ms(build_started);

    if should_ignore_local_observation(runtime, &item, now_ms()) {
        return false;
    }

    push_log(
        runtime,
        "INFO",
        &format!(
            "detected local clipboard kind={} size_bytes={} item={}",
            item.payload.kind(),
            item.size_bytes,
            item.id
        ),
    );
    push_log(
        runtime,
        "DEBUG",
        &format!(
            "profile local_clipboard item={} kind={} size_bytes={} read_snapshot_ms={} build_item_ms={} total_ms={}",
            item.id,
            item.payload.kind(),
            item.size_bytes,
            snapshot_ms,
            build_ms,
            elapsed_ms(observation_started)
        ),
    );
    register_recent_event(runtime, &item.id);
    update_latest_item(runtime, &item);
    enqueue_outbound_item(runtime, item);
    true
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn seed_local_clipboard_baseline(runtime: &RuntimeInner, limits: &SizeLimits) {
    let Ok(payload) = clipboard::read_snapshot(limits) else {
        return;
    };
    let Ok(content_hash) = clipboard::payload_content_hash(&payload) else {
        return;
    };
    remember_local_observation(runtime, &content_hash, now_ms());
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_clipboard_watch_poll_loop(
    runtime: Arc<RuntimeInner>,
    limits: SizeLimits,
    device_id: String,
    poll_interval: Duration,
) {
    let mut last_change_token = clipboard_change_token();
    if last_change_token.is_none() {
        seed_local_clipboard_baseline(&runtime, &limits);
    }
    let mut current_interval = poll_interval;
    while !runtime.stop_flag.load(Ordering::SeqCst) {
        let current_change_token = clipboard_change_token();
        let should_read_snapshot = match (last_change_token, current_change_token) {
            (Some(previous), Some(current)) => current != previous,
            _ => true,
        };
        last_change_token = current_change_token;
        let changed = should_read_snapshot
            && process_local_clipboard_observation(&runtime, &limits, &device_id);
        if changed {
            current_interval = poll_interval;
        } else {
            let max_interval = Duration::from_millis(CLIPBOARD_WATCH_MAX_INTERVAL_MS);
            current_interval = (current_interval + current_interval).min(max_interval);
        }
        std::thread::sleep(current_interval);
    }
}
