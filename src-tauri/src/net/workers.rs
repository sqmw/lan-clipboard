use super::flow::{process_inbound_queue, process_outbound_queue};
use super::queue::QueueLane;
use super::state::RuntimeInner;
use super::transfers::has_active_transfers;
use crate::settings::Settings;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const MAIN_LOOP_ACTIVE_SLEEP_MS: u64 = 15;
const MAIN_LOOP_IDLE_SLEEP_MS: u64 = 80;
const QUEUE_LOOP_ACTIVE_SLEEP_MS: u64 = 5;
const QUEUE_LOOP_IDLE_SLEEP_MS: u64 = 40;

pub(super) fn spawn_inbound_apply_worker(
    runtime: Arc<RuntimeInner>,
    settings: Settings,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("lan-clipboard-inbound-apply".to_string())
        .spawn(move || run_inbound_apply_loop(runtime, settings))
        .ok()
}

pub(super) fn spawn_outbound_worker(
    name: &str,
    runtime: Arc<RuntimeInner>,
    settings: Settings,
    allowed_lanes: &'static [QueueLane],
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || run_outbound_dispatch_loop(runtime, settings, allowed_lanes))
        .ok()
}

pub(super) fn main_loop_sleep_duration(runtime: &RuntimeInner) -> Duration {
    let queue_busy = runtime
        .outbound_queue
        .lock()
        .map(|guard| !guard.is_empty())
        .unwrap_or(true)
        || runtime
            .inbound_queue
            .lock()
            .map(|guard| !guard.is_empty())
            .unwrap_or(true);
    let sleep_ms = if has_active_transfers(runtime) || queue_busy {
        MAIN_LOOP_ACTIVE_SLEEP_MS
    } else {
        MAIN_LOOP_IDLE_SLEEP_MS
    };
    Duration::from_millis(sleep_ms)
}

pub(super) fn join_worker(worker: Option<std::thread::JoinHandle<()>>) {
    if let Some(handle) = worker {
        let _ = handle.join();
    }
}

fn run_inbound_apply_loop(runtime: Arc<RuntimeInner>, settings: Settings) {
    while !runtime.stop_flag.load(Ordering::SeqCst) {
        let did_work = process_inbound_queue(&runtime, &settings);
        std::thread::sleep(queue_loop_sleep_duration(did_work));
    }
}

fn run_outbound_dispatch_loop(
    runtime: Arc<RuntimeInner>,
    settings: Settings,
    allowed_lanes: &'static [QueueLane],
) {
    while !runtime.stop_flag.load(Ordering::SeqCst) {
        let did_work = process_outbound_queue(&runtime, &settings, allowed_lanes);
        std::thread::sleep(queue_loop_sleep_duration(did_work));
    }
}

fn queue_loop_sleep_duration(did_work: bool) -> Duration {
    let sleep_ms = if did_work {
        QUEUE_LOOP_ACTIVE_SLEEP_MS
    } else {
        QUEUE_LOOP_IDLE_SLEEP_MS
    };
    Duration::from_millis(sleep_ms)
}
