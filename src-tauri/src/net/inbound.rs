use super::display::{file_stream_label, file_stream_summary, payload_label, payload_summary};
use super::file_stream::{FileStreamTransfer, RawFileStreamReader};
use super::handshake::{server_handshake, Session};
use super::logs::{push_log, set_error};
use super::marker::{file_stream_marker, is_stale_marker};
use super::members::mark_known_member;
use super::metrics::{elapsed_ms, format_mib_per_second, now_ms};
use super::socket::tune_stream_for_receive;
use super::transfers::{
    canonical_receive_transfer_id, mark_transfer_failed, upsert_transfer, TransferProgress,
};
use super::wire::{
    decode_wire_body_bytes, read_wire_body_from_stream, read_wire_frame, FileStreamStart, WireBody,
};
use super::{
    enqueue_inbound_item, remember_active_local_ip, should_skip_remote_item, RuntimeInner,
};
use crate::clipboard;
use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::Settings;
use std::net::{IpAddr, Shutdown, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

const MAX_INCOMING_CONNECTIONS: usize = 16;
const MAX_INCOMING_CONNECTIONS_PER_IP: usize = 4;
const MAX_ACTIVE_FILE_RECEIVES: usize = 2;

struct IncomingConnectionGuard {
    runtime: Arc<RuntimeInner>,
    connection_id: u64,
}

impl Drop for IncomingConnectionGuard {
    fn drop(&mut self) {
        if let Ok(mut sockets) = self.runtime.incoming_sockets.lock() {
            sockets.remove(&self.connection_id);
        }
    }
}

struct ActiveFileReceiveGuard<'a> {
    runtime: &'a RuntimeInner,
}

impl<'a> ActiveFileReceiveGuard<'a> {
    fn acquire(runtime: &'a RuntimeInner) -> anyhow::Result<Self> {
        runtime
            .active_file_receives
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_ACTIVE_FILE_RECEIVES).then_some(active + 1)
            })
            .map_err(|_| anyhow::anyhow!("concurrent file receive limit reached"))?;
        Ok(Self { runtime })
    }
}

impl Drop for ActiveFileReceiveGuard<'_> {
    fn drop(&mut self) {
        self.runtime
            .active_file_receives
            .fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) fn spawn_incoming_connection_worker(
    runtime: Arc<RuntimeInner>,
    settings: Settings,
    device_id: String,
    stream: TcpStream,
) {
    prune_finished_incoming_workers(&runtime);
    let peer_ip = stream.peer_addr().ok().map(|address| address.ip());
    let connection_id = runtime.next_connection_id.fetch_add(1, Ordering::Relaxed);
    let tracked_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(error) => {
            set_error(&runtime, format!("clone incoming socket failed: {error}"));
            return;
        }
    };
    let registered = runtime
        .incoming_sockets
        .lock()
        .map(|mut sockets| {
            let peer_count = peer_ip
                .map(|target| sockets.values().filter(|(ip, _)| *ip == target).count())
                .unwrap_or(0);
            if sockets.len() >= MAX_INCOMING_CONNECTIONS
                || peer_count >= MAX_INCOMING_CONNECTIONS_PER_IP
            {
                return false;
            }
            sockets.insert(
                connection_id,
                (
                    peer_ip.unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
                    tracked_stream,
                ),
            );
            true
        })
        .unwrap_or(false);
    if !registered {
        let _ = stream.shutdown(Shutdown::Both);
        push_log(&runtime, "WARN", "incoming connection limit reached");
        return;
    }

    let worker_runtime = Arc::clone(&runtime);
    let spawn_result = std::thread::Builder::new()
        .name("lan-clipboard-incoming".to_string())
        .spawn(move || {
            let guard = IncomingConnectionGuard {
                runtime: Arc::clone(&worker_runtime),
                connection_id,
            };
            if let Err(error) = handle_incoming(&guard.runtime, &settings, stream, &device_id) {
                if !guard.runtime.stop_flag.load(Ordering::SeqCst) {
                    push_log(
                        &guard.runtime,
                        "DEBUG",
                        &format!("incoming connection closed: {error}"),
                    );
                }
            }
        });
    match spawn_result {
        Ok(handle) => {
            if let Ok(mut workers) = runtime.incoming_workers.lock() {
                workers.push(handle);
            }
        }
        Err(error) => {
            if let Ok(mut sockets) = runtime.incoming_sockets.lock() {
                sockets.remove(&connection_id);
            }
            set_error(&runtime, format!("spawn incoming worker failed: {error}"));
        }
    }
}

pub(super) fn shutdown_incoming_workers(runtime: &RuntimeInner) {
    if let Ok(sockets) = runtime.incoming_sockets.lock() {
        for (_, stream) in sockets.values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
    let workers = runtime
        .incoming_workers
        .lock()
        .map(|mut workers| workers.drain(..).collect::<Vec<_>>())
        .unwrap_or_default();
    for worker in workers {
        let _ = worker.join();
    }
    if let Ok(mut sockets) = runtime.incoming_sockets.lock() {
        sockets.clear();
    }
}

fn prune_finished_incoming_workers(runtime: &RuntimeInner) {
    let finished = runtime
        .incoming_workers
        .lock()
        .map(|mut workers| {
            let mut finished = Vec::new();
            let mut index = 0;
            while index < workers.len() {
                if workers[index].is_finished() {
                    finished.push(workers.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            finished
        })
        .unwrap_or_default();
    for worker in finished {
        let _ = worker.join();
    }
}

fn handle_incoming(
    runtime: &RuntimeInner,
    settings: &Settings,
    stream: TcpStream,
    device_id: &str,
) -> anyhow::Result<()> {
    let mut stream = stream;
    let mut session = server_handshake(&mut stream, &settings.sync.shared_code, device_id)?;
    tune_stream_for_receive(&stream, settings.limits.max_item_bytes);
    let remote_addr = stream.peer_addr().ok().map(|addr| addr.to_string());
    if let Ok(local_addr) = stream.local_addr() {
        remember_active_local_ip(runtime, local_addr.ip());
    }
    while !runtime.stop_flag.load(Ordering::SeqCst) {
        let Some(frame_bytes) = read_wire_frame(&mut stream, settings)? else {
            break;
        };
        if runtime.stop_flag.load(Ordering::SeqCst) {
            break;
        }
        match decode_wire_body_bytes(&frame_bytes, settings, &mut session)? {
            WireBody::ClipboardItem(item) => {
                handle_incoming_item(
                    runtime,
                    remote_addr.as_deref().unwrap_or("未知来源"),
                    item,
                    device_id,
                );
            }
            WireBody::FileStreamRawStart(meta) => {
                receive_raw_file_stream(
                    runtime,
                    settings,
                    &mut session,
                    &mut stream,
                    remote_addr.as_deref().unwrap_or("未知来源"),
                    meta,
                    device_id,
                )?;
            }
            WireBody::FileStreamEnd(_) => {
                return Err(anyhow::anyhow!("unexpected file stream end frame"));
            }
        }
    }
    Ok(())
}

fn receive_raw_file_stream(
    runtime: &RuntimeInner,
    settings: &Settings,
    session: &mut Session,
    stream: &mut TcpStream,
    peer: &str,
    meta: FileStreamStart,
    device_id: &str,
) -> anyhow::Result<()> {
    if runtime.stop_flag.load(Ordering::SeqCst) {
        return Err(anyhow::anyhow!("sync stopped"));
    }
    let _active_receive = ActiveFileReceiveGuard::acquire(runtime)?;
    let receive_started = Instant::now();
    let transfer_id = format!("recv:{}:{}", meta.source_device_id, meta.item_id);
    upsert_transfer(
        runtime,
        TransferProgress {
            id: transfer_id.clone(),
            direction: "receive".to_string(),
            peer: peer.to_string(),
            item_kind: "file_bundle".to_string(),
            item_label: file_stream_label(&meta.top_level_names),
            item_summary: file_stream_summary(&meta.top_level_names),
            item_id: meta.item_id.clone(),
            transferred_bytes: 0,
            total_bytes: meta.size_bytes,
            percent: 0,
            status: "receiving".to_string(),
            updated_at_ms: now_ms(),
            error: None,
        },
    );

    let marker = file_stream_marker(&meta);
    if is_stale_marker(runtime, &marker) {
        mark_transfer_failed(runtime, &transfer_id, "已被更新内容替代".to_string());
        return Err(anyhow::anyhow!("stale inbound raw file stream"));
    }

    let stream_started = Instant::now();
    let mut reader = RawFileStreamReader::new(
        runtime,
        settings,
        session,
        stream,
        FileStreamTransfer::new(&transfer_id, &meta.item_id, meta.size_bytes, marker),
    );
    let received_bundle = match clipboard::unpack_file_bundle_archive_reader(
        &mut reader,
        &meta.top_level_names,
        settings.limits.max_item_bytes,
    ) {
        Ok(bundle_dir) => bundle_dir,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            return Err(error.into());
        }
    };
    if let Err(error) = reader.ensure_complete() {
        mark_transfer_failed(runtime, &transfer_id, error.to_string());
        return Err(error);
    }
    let received_chunk_count = reader.chunk_count();
    let received_digest = reader.digest_sha256();
    drop(reader);
    let stream_ms = elapsed_ms(stream_started);

    let end_frame_started = Instant::now();
    let valid_end = match read_wire_body_from_stream(stream, settings, session)? {
        Some(WireBody::FileStreamEnd(end)) => {
            end.item_id == meta.item_id
                && end.chunk_count == meta.chunk_count
                && end.chunk_count == received_chunk_count
                && end.digest_sha256 == received_digest
        }
        _ => false,
    };
    if !valid_end {
        mark_transfer_failed(runtime, &transfer_id, "文件流完整性校验失败".to_string());
        return Err(anyhow::anyhow!("file stream integrity check failed"));
    }
    if runtime.stop_flag.load(Ordering::SeqCst) {
        return Err(anyhow::anyhow!("sync stopped"));
    }
    let end_frame_ms = elapsed_ms(end_frame_started);
    push_log(
        runtime,
        "DEBUG",
        &format!(
            "profile file_recv item={} peer={} encryption={} size_bytes={} stream_ms={} end_frame_ms={} total_ms={} throughput_mib_s={}",
            meta.item_id,
            peer,
            settings.security.encryption_enabled,
            meta.size_bytes,
            stream_ms,
            end_frame_ms,
            elapsed_ms(receive_started),
            format_mib_per_second(meta.size_bytes, stream_ms)
        ),
    );

    let bundle_dir = received_bundle.into_path();
    let cleanup_payload = ClipboardPayload::FileBundleDir {
        bundle_dir: bundle_dir.clone(),
        top_level_names: meta.top_level_names.clone(),
    };
    let item = ClipboardItem {
        id: meta.item_id,
        content_hash: meta.content_hash,
        created_at_us: meta.created_at_us,
        source_device_id: meta.source_device_id,
        size_bytes: meta.size_bytes,
        payload: ClipboardPayload::FileBundleDir {
            bundle_dir,
            top_level_names: meta.top_level_names,
        },
    };
    if !handle_incoming_item(runtime, peer, item, device_id) {
        if let Err(error) = crate::clipboard::remove_internal_file_payload(&cleanup_payload) {
            push_log(
                runtime,
                "WARN",
                &format!("failed to remove rejected inbound file payload: {error}"),
            );
        }
    }
    Ok(())
}

fn handle_incoming_item(
    runtime: &RuntimeInner,
    peer: &str,
    item: ClipboardItem,
    device_id: &str,
) -> bool {
    let canonical_transfer_id = canonical_receive_transfer_id(&item);
    if runtime.stop_flag.load(Ordering::SeqCst) || item.source_device_id == device_id {
        return false;
    }
    mark_known_member(runtime, "device", &item.source_device_id);
    if should_skip_remote_item(runtime, &item) {
        return false;
    }
    upsert_transfer(
        runtime,
        TransferProgress {
            id: canonical_transfer_id.clone(),
            direction: "receive".to_string(),
            peer: peer.to_string(),
            item_kind: item.payload.kind().to_string(),
            item_label: payload_label(&item.payload),
            item_summary: payload_summary(&item.payload),
            item_id: item.id.clone(),
            transferred_bytes: item.size_bytes,
            total_bytes: item.size_bytes,
            percent: 100,
            status: "received".to_string(),
            updated_at_ms: now_ms(),
            error: None,
        },
    );
    push_log(
        runtime,
        "INFO",
        &format!(
            "received item {} kind={} size_bytes={} from {}",
            item.id,
            item.payload.kind(),
            item.size_bytes,
            item.source_device_id
        ),
    );
    enqueue_inbound_item(runtime, item, &canonical_transfer_id, peer);
    true
}
