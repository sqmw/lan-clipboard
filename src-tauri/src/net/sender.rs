use super::dedupe::mark_shared_fingerprint;
use super::display::{payload_label, payload_summary};
use super::file_stream::{FileStreamNetworkWriter, FileStreamTransfer};
use super::handshake::client_handshake;
use super::logs::{push_log, set_error};
use super::marker::item_marker;
use super::members::mark_known_member;
use super::metrics::{elapsed_ms, format_mib_per_second, item_age_ms, now_ms};
use super::socket::{
    connect_tcp_from, send_transfer_id, tune_stream_for_send, write_timeout_for_payload,
};
use super::transfers::{
    mark_transfer_completed, mark_transfer_failed, transfer_should_abort, update_transfer_progress,
    upsert_transfer, TransferProgress,
};
use super::wire::{
    encode_wire_message, write_wire_body_to_stream, FileStreamStart, ImageStreamStart,
    PayloadStreamEnd, WireBody, RAW_PAYLOAD_PLAIN_BYTES,
};
use super::{collect_peer_targets, remember_active_local_ip, RuntimeInner};
use crate::clipboard;
use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::Settings;
use std::io::Write;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT_MS: u64 = 2_000;
const TRANSFER_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const MAX_PARALLEL_PEER_SENDS: usize = 8;

struct OutboundConnectionGuard<'a> {
    runtime: &'a RuntimeInner,
    connection_id: u64,
}

impl Drop for OutboundConnectionGuard<'_> {
    fn drop(&mut self) {
        let mut sockets = self
            .runtime
            .outbound_sockets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sockets.remove(&self.connection_id);
    }
}

fn register_outbound_connection<'a>(
    runtime: &'a RuntimeInner,
    stream: &TcpStream,
) -> anyhow::Result<OutboundConnectionGuard<'a>> {
    let tracked_stream = stream.try_clone()?;
    let connection_id = runtime.next_connection_id.fetch_add(1, Ordering::Relaxed);
    let mut sockets = runtime
        .outbound_sockets
        .lock()
        .map_err(|_| anyhow::anyhow!("outbound socket registry lock poisoned"))?;
    if runtime.stop_flag.load(Ordering::SeqCst) {
        let _ = stream.shutdown(Shutdown::Both);
        return Err(anyhow::anyhow!("sync stopped"));
    }
    sockets.insert(connection_id, tracked_stream);
    Ok(OutboundConnectionGuard {
        runtime,
        connection_id,
    })
}

pub(super) fn shutdown_outbound_connections(runtime: &RuntimeInner) {
    let sockets = runtime
        .outbound_sockets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for stream in sockets.values() {
        let _ = stream.shutdown(Shutdown::Both);
    }
}

pub(super) struct BroadcastReport {
    pub(super) attempted: usize,
    pub(super) delivered: usize,
    pub(super) failed_peers: Vec<String>,
    pub(super) deferred: bool,
}

fn run_peer_workers<F>(peers: Vec<String>, send: F) -> (usize, Vec<String>)
where
    F: Fn(&str) -> bool + Sync,
{
    if peers.is_empty() {
        return (0, Vec::new());
    }

    let next_peer = AtomicUsize::new(0);
    let outcomes = Mutex::new(vec![None; peers.len()]);
    let worker_count = peers.len().min(MAX_PARALLEL_PEER_SENDS);
    std::thread::scope(|scope| {
        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            workers.push(scope.spawn(|| loop {
                let index = next_peer.fetch_add(1, Ordering::Relaxed);
                if index >= peers.len() {
                    break;
                }
                let delivered =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| send(&peers[index])))
                        .unwrap_or(false);
                outcomes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)[index] = Some(delivered);
            }));
        }
        for worker in workers {
            let _ = worker.join();
        }
    });

    let outcomes = outcomes
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut delivered = 0usize;
    let mut failed_peers = Vec::new();
    for (peer, outcome) in peers.into_iter().zip(outcomes) {
        if outcome == Some(true) {
            delivered += 1;
        } else {
            failed_peers.push(peer);
        }
    }
    (delivered, failed_peers)
}

pub(super) fn send_to_all_peers(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
    pending_peers: Option<&[String]>,
) -> BroadcastReport {
    let mut peers = pending_peers
        .map(|peers| peers.to_vec())
        .unwrap_or_else(|| collect_peer_targets(runtime, settings));
    peers.sort();
    peers.dedup();
    match item.payload {
        ClipboardPayload::FileList { .. } => {
            return send_file_list_to_all_peers(runtime, settings, item, peers)
        }
        ClipboardPayload::ImagePng { .. } => {
            return send_image_to_all_peers(runtime, settings, item, peers)
        }
        _ => {}
    }

    let attempted = peers.len();
    let (delivered, failed_peers) = run_peer_workers(peers, |peer| {
        send_payload_to_peer(runtime, settings, item, peer)
    });

    if delivered > 0 {
        push_log(
            runtime,
            "DEBUG",
            &format!("broadcast delivered={}", delivered),
        );
    }
    BroadcastReport {
        attempted,
        delivered,
        failed_peers,
        deferred: false,
    }
}

fn send_file_list_to_all_peers(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
    peers: Vec<String>,
) -> BroadcastReport {
    let attempted = peers.len();
    if attempted == 0 {
        return BroadcastReport {
            attempted,
            delivered: 0,
            failed_peers: Vec::new(),
            deferred: false,
        };
    }

    let (delivered, failed_peers) = run_peer_workers(peers, |peer| {
        send_file_list_to_peer(runtime, settings, item, peer)
    });
    if delivered > 0 {
        mark_shared_fingerprint(runtime, &item.content_hash);
    }
    push_log(
        runtime,
        "DEBUG",
        &format!(
            "file stream completed item={} delivered={} attempted={}",
            item.id, delivered, attempted
        ),
    );

    BroadcastReport {
        attempted,
        delivered,
        failed_peers,
        deferred: false,
    }
}

fn send_image_to_all_peers(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
    peers: Vec<String>,
) -> BroadcastReport {
    let attempted = peers.len();
    if attempted == 0 {
        return BroadcastReport {
            attempted,
            delivered: 0,
            failed_peers: Vec::new(),
            deferred: false,
        };
    }
    let (delivered, failed_peers) = run_peer_workers(peers, |peer| {
        send_image_to_peer(runtime, settings, item, peer)
    });
    if delivered > 0 {
        mark_shared_fingerprint(runtime, &item.content_hash);
    }
    push_log(
        runtime,
        "DEBUG",
        &format!(
            "image stream completed item={} delivered={} attempted={}",
            item.id, delivered, attempted
        ),
    );
    BroadcastReport {
        attempted,
        delivered,
        failed_peers,
        deferred: false,
    }
}

fn send_payload_to_peer(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
    peer: &str,
) -> bool {
    let connect_timeout = Duration::from_millis(CONNECT_TIMEOUT_MS);
    let socket_addr = match peer.parse::<SocketAddr>() {
        Ok(socket_addr) => socket_addr,
        Err(_) => {
            push_log(runtime, "WARN", &format!("skip bad peer addr: {}", peer));
            return false;
        }
    };
    let transfer_id = send_transfer_id(peer, item);
    upsert_transfer(
        runtime,
        TransferProgress {
            id: transfer_id.clone(),
            direction: "send".to_string(),
            peer: peer.to_string(),
            item_kind: item.payload.kind().to_string(),
            item_label: payload_label(&item.payload),
            item_summary: payload_summary(&item.payload),
            item_id: item.id.clone(),
            transferred_bytes: 0,
            total_bytes: item.size_bytes,
            percent: 0,
            status: "sending".to_string(),
            updated_at_ms: now_ms(),
            error: None,
        },
    );
    let stream = connect_tcp_from(&socket_addr, Some(&settings.sync.local_ip), connect_timeout);
    let mut stream = match stream {
        Ok(stream) => stream,
        Err(error) => {
            push_log(
                runtime,
                "DEBUG",
                &format!("connect peer failed peer={peer} error={error}"),
            );
            return false;
        }
    };
    let _connection_guard = match register_outbound_connection(runtime, &stream) {
        Ok(guard) => guard,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            return false;
        }
    };
    if let Ok(local_addr) = stream.local_addr() {
        remember_active_local_ip(runtime, local_addr.ip());
    }
    let mut session = match client_handshake(
        &mut stream,
        &settings.sync.shared_code,
        &settings.sync_device_id(),
    ) {
        Ok(session) => session,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            push_log(
                runtime,
                "DEBUG",
                &format!("peer handshake failed peer={peer} error={error}"),
            );
            return false;
        }
    };
    let payload = match encode_wire_message(item, settings, &mut session) {
        Ok(payload) => payload,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            set_error(runtime, format!("encode payload failed: {error}"));
            return false;
        }
    };
    tune_stream_for_send(&stream, payload.len() as u64);
    if write_all_with_progress(runtime, &mut stream, &payload, &transfer_id).is_ok() {
        mark_transfer_completed(runtime, &transfer_id);
        mark_known_member(runtime, "addr", peer);
        true
    } else {
        mark_transfer_failed(runtime, &transfer_id, "发送失败".to_string());
        push_log(
            runtime,
            "DEBUG",
            &format!(
                "write peer failed peer={peer} payload_bytes={} timeout_ms={}",
                payload.len(),
                write_timeout_for_payload(payload.len() as u64).as_millis()
            ),
        );
        false
    }
}

fn send_image_to_peer(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
    peer: &str,
) -> bool {
    let ClipboardPayload::ImagePng { png_bytes } = &item.payload else {
        return false;
    };
    let send_started = Instant::now();
    let socket_addr: SocketAddr = match peer.parse() {
        Ok(socket_addr) => socket_addr,
        Err(error) => {
            push_log(
                runtime,
                "WARN",
                &format!("skip bad image peer addr peer={peer} error={error}"),
            );
            return false;
        }
    };
    let transfer_id = send_transfer_id(peer, item);
    upsert_transfer(
        runtime,
        TransferProgress {
            id: transfer_id.clone(),
            direction: "send".to_string(),
            peer: peer.to_string(),
            item_kind: item.payload.kind().to_string(),
            item_label: payload_label(&item.payload),
            item_summary: payload_summary(&item.payload),
            item_id: item.id.clone(),
            transferred_bytes: 0,
            total_bytes: item.size_bytes,
            percent: 0,
            status: "sending".to_string(),
            updated_at_ms: now_ms(),
            error: None,
        },
    );
    let connect_started = Instant::now();
    let mut stream = match connect_tcp_from(
        &socket_addr,
        Some(&settings.sync.local_ip),
        Duration::from_millis(CONNECT_TIMEOUT_MS),
    ) {
        Ok(stream) => stream,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            push_log(
                runtime,
                "DEBUG",
                &format!("connect image peer failed peer={peer} error={error}"),
            );
            return false;
        }
    };
    let connect_ms = elapsed_ms(connect_started);
    let _connection_guard = match register_outbound_connection(runtime, &stream) {
        Ok(guard) => guard,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            return false;
        }
    };
    if let Ok(local_addr) = stream.local_addr() {
        remember_active_local_ip(runtime, local_addr.ip());
    }
    let mut session = match client_handshake(
        &mut stream,
        &settings.sync.shared_code,
        &settings.sync_device_id(),
    ) {
        Ok(session) => session,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            push_log(
                runtime,
                "DEBUG",
                &format!("image peer handshake failed peer={peer} error={error}"),
            );
            return false;
        }
    };
    tune_stream_for_send(&stream, item.size_bytes);
    let start = WireBody::ImageStreamRawStart(ImageStreamStart {
        item_id: item.id.clone(),
        content_hash: item.content_hash.clone(),
        created_at_us: item.created_at_us,
        source_device_id: item.source_device_id.clone(),
        size_bytes: item.size_bytes,
        chunk_count: item.size_bytes.div_ceil(RAW_PAYLOAD_PLAIN_BYTES as u64),
    });
    if let Err(error) = write_wire_body_to_stream(&mut stream, settings, &mut session, &start) {
        mark_transfer_failed(runtime, &transfer_id, error.to_string());
        push_log(
            runtime,
            "DEBUG",
            &format!("stream image start failed peer={peer} error={error}"),
        );
        return false;
    }

    let stream_started = Instant::now();
    let stream_result = {
        let transfer =
            FileStreamTransfer::new(&transfer_id, &item.id, item.size_bytes, item_marker(item));
        let mut writer =
            FileStreamNetworkWriter::new(runtime, settings, &session, &mut stream, transfer);
        (|| -> anyhow::Result<CompletedFileStream> {
            writer.write_all(png_bytes)?;
            writer.finish()?;
            if writer.sent_bytes() != item.size_bytes {
                return Err(anyhow::anyhow!(
                    "streamed image size mismatch: sent {} bytes, expected {} bytes",
                    writer.sent_bytes(),
                    item.size_bytes
                ));
            }
            Ok(CompletedFileStream {
                chunk_count: writer.frame_count(),
                digest_sha256: writer.digest_sha256(),
            })
        })()
    };
    let stream_result = match stream_result {
        Ok(result) => result,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            push_log(
                runtime,
                "DEBUG",
                &format!("stream image payload failed peer={peer} error={error}"),
            );
            return false;
        }
    };
    if let Err(error) = write_wire_body_to_stream(
        &mut stream,
        settings,
        &mut session,
        &WireBody::PayloadStreamEnd(PayloadStreamEnd {
            item_id: item.id.clone(),
            chunk_count: stream_result.chunk_count,
            digest_sha256: stream_result.digest_sha256,
        }),
    ) {
        mark_transfer_failed(runtime, &transfer_id, error.to_string());
        push_log(
            runtime,
            "DEBUG",
            &format!("stream image end failed peer={peer} error={error}"),
        );
        return false;
    }
    mark_transfer_completed(runtime, &transfer_id);
    mark_known_member(runtime, "addr", peer);
    push_log(
        runtime,
        "DEBUG",
        &format!(
            "profile image_send item={} peer={} encryption={} size_bytes={} connect_ms={} stream_ms={} total_ms={} throughput_mib_s={}",
            item.id,
            peer,
            settings.security.encryption_enabled,
            item.size_bytes,
            connect_ms,
            elapsed_ms(stream_started),
            elapsed_ms(send_started),
            format_mib_per_second(item.size_bytes, elapsed_ms(stream_started)),
        ),
    );
    true
}

fn send_file_list_to_peer(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
    peer: &str,
) -> bool {
    let ClipboardPayload::FileList {
        paths,
        top_level_names,
        estimated_archive_bytes,
    } = &item.payload
    else {
        return false;
    };
    let send_started = Instant::now();
    let socket_addr: SocketAddr = match peer.parse() {
        Ok(socket_addr) => socket_addr,
        Err(error) => {
            push_log(
                runtime,
                "WARN",
                &format!("skip bad file peer addr peer={peer} error={error}"),
            );
            return false;
        }
    };
    let transfer_id = send_transfer_id(peer, item);
    upsert_transfer(
        runtime,
        TransferProgress {
            id: transfer_id.clone(),
            direction: "send".to_string(),
            peer: peer.to_string(),
            item_kind: item.payload.kind().to_string(),
            item_label: payload_label(&item.payload),
            item_summary: payload_summary(&item.payload),
            item_id: item.id.clone(),
            transferred_bytes: 0,
            total_bytes: *estimated_archive_bytes,
            percent: 0,
            status: "sending".to_string(),
            updated_at_ms: now_ms(),
            error: None,
        },
    );
    let connect_started = Instant::now();
    let mut stream = match connect_tcp_from(
        &socket_addr,
        Some(&settings.sync.local_ip),
        Duration::from_millis(CONNECT_TIMEOUT_MS),
    ) {
        Ok(stream) => stream,
        Err(error) => {
            push_log(
                runtime,
                "DEBUG",
                &format!("connect file peer failed peer={peer} error={error}"),
            );
            return false;
        }
    };
    let connect_ms = elapsed_ms(connect_started);
    let _connection_guard = match register_outbound_connection(runtime, &stream) {
        Ok(guard) => guard,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            return false;
        }
    };
    if let Ok(local_addr) = stream.local_addr() {
        remember_active_local_ip(runtime, local_addr.ip());
    }
    let mut session = match client_handshake(
        &mut stream,
        &settings.sync.shared_code,
        &settings.sync_device_id(),
    ) {
        Ok(session) => session,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            push_log(
                runtime,
                "DEBUG",
                &format!("file peer handshake failed peer={peer} error={error}"),
            );
            return false;
        }
    };
    tune_stream_for_send(&stream, *estimated_archive_bytes);

    let start = WireBody::FileStreamRawStart(FileStreamStart {
        item_id: item.id.clone(),
        content_hash: item.content_hash.clone(),
        created_at_us: item.created_at_us,
        source_device_id: item.source_device_id.clone(),
        size_bytes: *estimated_archive_bytes,
        chunk_count: estimated_archive_bytes.div_ceil(RAW_PAYLOAD_PLAIN_BYTES as u64),
        top_level_names: top_level_names.clone(),
    });
    let item_age_to_first_payload_ms = item_age_ms(item);
    let start_frame_started = Instant::now();
    if let Err(error) = write_wire_body_to_stream(&mut stream, settings, &mut session, &start) {
        mark_transfer_failed(runtime, &transfer_id, error.to_string());
        push_log(
            runtime,
            "DEBUG",
            &format!("stream file start failed peer={peer} error={error}"),
        );
        return false;
    }
    let start_frame_ms = elapsed_ms(start_frame_started);

    let stream_started = Instant::now();
    let stream_attempt = {
        let transfer = FileStreamTransfer::new(
            &transfer_id,
            &item.id,
            *estimated_archive_bytes,
            item_marker(item),
        );
        let mut writer =
            FileStreamNetworkWriter::new(runtime, settings, &session, &mut stream, transfer);
        stream_file_list_archive_to_peer(
            runtime,
            &mut writer,
            item,
            paths,
            *estimated_archive_bytes,
        )
    };
    let stream_result = match stream_attempt {
        Ok(result) => result,
        Err(error) => {
            mark_transfer_failed(runtime, &transfer_id, error.to_string());
            push_log(
                runtime,
                "DEBUG",
                &format!("stream file archive failed peer={peer} error={error}"),
            );
            return false;
        }
    };
    let stream_ms = elapsed_ms(stream_started);

    let end_frame_started = Instant::now();
    if let Err(error) = write_wire_body_to_stream(
        &mut stream,
        settings,
        &mut session,
        &WireBody::PayloadStreamEnd(PayloadStreamEnd {
            item_id: item.id.clone(),
            chunk_count: stream_result.chunk_count,
            digest_sha256: stream_result.digest_sha256,
        }),
    ) {
        mark_transfer_failed(runtime, &transfer_id, error.to_string());
        push_log(
            runtime,
            "DEBUG",
            &format!("stream file end failed peer={peer} error={error}"),
        );
        return false;
    }
    let end_frame_ms = elapsed_ms(end_frame_started);
    mark_transfer_completed(runtime, &transfer_id);
    mark_known_member(runtime, "addr", peer);
    push_log(
        runtime,
        "DEBUG",
        &format!(
            "profile file_send item={} peer={} encryption={} size_bytes={} connect_ms={} start_frame_ms={} stream_ms={} end_frame_ms={} total_ms={} throughput_mib_s={} item_age_to_first_payload_ms={}",
            item.id,
            peer,
            settings.security.encryption_enabled,
            estimated_archive_bytes,
            connect_ms,
            start_frame_ms,
            stream_ms,
            end_frame_ms,
            elapsed_ms(send_started),
            format_mib_per_second(*estimated_archive_bytes, stream_ms),
            item_age_to_first_payload_ms
        ),
    );
    true
}

fn stream_file_list_archive_to_peer(
    runtime: &RuntimeInner,
    writer: &mut FileStreamNetworkWriter<'_>,
    item: &ClipboardItem,
    paths: &[PathBuf],
    size_bytes: u64,
) -> anyhow::Result<CompletedFileStream> {
    let archive_started_at = Instant::now();
    let streamed_content_hash = clipboard::stream_file_bundle_archive(paths, &mut *writer)?;
    if streamed_content_hash != item.content_hash {
        return Err(anyhow::anyhow!(
            "clipboard files changed while the transfer was being prepared"
        ));
    }
    writer.finish()?;
    let sent_bytes = writer.sent_bytes();
    let write_frame_ms = writer.write_frame_ms();
    let write_frame_max_ms = writer.write_frame_max_ms();
    let frame_count = writer.frame_count();
    let digest_sha256 = writer.digest_sha256();
    if sent_bytes != size_bytes {
        return Err(anyhow::anyhow!(
            "streamed archive size mismatch: sent {sent_bytes} bytes, expected {size_bytes} bytes"
        ));
    }
    push_log(
        runtime,
        "DEBUG",
        &format!(
            "streamed file archive item={} size_bytes={} elapsed_ms={} frame_count={} write_frame_ms={} write_frame_max_ms={}",
            item.id,
            sent_bytes,
            archive_started_at.elapsed().as_millis(),
            frame_count,
            write_frame_ms,
            write_frame_max_ms
        ),
    );
    Ok(CompletedFileStream {
        chunk_count: frame_count,
        digest_sha256,
    })
}

struct CompletedFileStream {
    chunk_count: u64,
    digest_sha256: [u8; 32],
}

fn write_all_with_progress(
    runtime: &RuntimeInner,
    stream: &mut TcpStream,
    buffer: &[u8],
    transfer_id: &str,
) -> anyhow::Result<()> {
    let total_bytes = buffer.len() as u64;
    let mut offset = 0usize;
    while offset < buffer.len() {
        if runtime.stop_flag.load(Ordering::SeqCst) || transfer_should_abort(runtime, transfer_id) {
            return Err(anyhow::anyhow!("transfer canceled"));
        }
        let end = (offset + TRANSFER_CHUNK_BYTES).min(buffer.len());
        if let Err(error) = stream.write_all(&buffer[offset..end]) {
            mark_transfer_failed(runtime, transfer_id, error.to_string());
            return Err(error.into());
        }
        offset = end;
        update_transfer_progress(runtime, transfer_id, offset as u64, total_bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Barrier};

    #[test]
    fn peer_worker_pool_caps_peak_concurrency_and_preserves_failures() {
        let peers = (0..16)
            .map(|index| format!("peer-{index}"))
            .collect::<Vec<_>>();
        let barrier = Arc::new(Barrier::new(MAX_PARALLEL_PEER_SENDS));
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let (delivered, failed) = run_peer_workers(peers, |peer| {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            let index = peer
                .strip_prefix("peer-")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap();
            if index < MAX_PARALLEL_PEER_SENDS {
                barrier.wait();
            }
            active.fetch_sub(1, Ordering::SeqCst);
            peer != "peer-3" && peer != "peer-12"
        });

        assert_eq!(peak.load(Ordering::SeqCst), MAX_PARALLEL_PEER_SENDS);
        assert_eq!(delivered, 14);
        assert_eq!(failed, vec!["peer-3".to_string(), "peer-12".to_string()]);
    }

    #[test]
    fn outbound_registry_shutdown_closes_live_connection() {
        let runtime = RuntimeInner::default();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let guard = register_outbound_connection(&runtime, &stream).unwrap();
        assert_eq!(runtime.outbound_sockets.lock().unwrap().len(), 1);
        shutdown_outbound_connections(&runtime);
        let mut byte = [0u8; 1];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
        assert!(stream.write_all(b"closed").is_err());

        drop(guard);
        assert!(runtime.outbound_sockets.lock().unwrap().is_empty());
    }
}
