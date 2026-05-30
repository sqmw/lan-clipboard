use super::dedupe::{clear_content_inflight, mark_shared_fingerprint};
use super::display::{payload_label, payload_summary};
use super::file_stream::FileStreamNetworkWriter;
use super::logs::{push_log, set_error};
use super::marker::item_marker;
use super::members::mark_known_member;
use super::metrics::{elapsed_ms, format_mib_per_second, item_age_ms, now_ms};
use super::socket::{send_transfer_id, tune_stream_for_send, write_timeout_for_payload};
use super::transfers::{
    mark_transfer_completed, mark_transfer_failed, transfer_should_abort, update_transfer_progress,
    upsert_transfer, TransferProgress,
};
use super::wire::{encode_wire_message, write_wire_body_to_stream, FileStreamStart, WireBody};
use super::{collect_peer_targets, remember_active_local_ip, RuntimeInner};
use crate::clipboard;
use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::Settings;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT_MS: u64 = 2_000;
const TRANSFER_CHUNK_BYTES: usize = 4 * 1024 * 1024;

pub(super) struct BroadcastReport {
    pub(super) attempted: usize,
    pub(super) delivered: usize,
    pub(super) deferred: bool,
}

pub(super) fn send_to_all_peers(
    runtime: &RuntimeInner,
    settings: &Settings,
    item: &ClipboardItem,
) -> BroadcastReport {
    let peers = collect_peer_targets(runtime, settings);
    if matches!(item.payload, ClipboardPayload::FileList { .. }) {
        return send_file_list_to_all_peers(runtime, settings, item, peers);
    }

    let payload = match encode_wire_message(item, settings) {
        Ok(payload) => payload,
        Err(error) => {
            set_error(runtime, format!("encode payload failed: {error}"));
            return BroadcastReport {
                attempted: 0,
                delivered: 0,
                deferred: false,
            };
        }
    };

    let attempted = peers.len();
    let delivered = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(peers.len());
        let payload_ref = &payload;
        for peer in peers {
            handles
                .push(scope.spawn(move || send_payload_to_peer(runtime, item, payload_ref, peer)));
        }

        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .filter(|delivered| *delivered)
            .count()
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
            deferred: false,
        };
    }

    let runtime_addr = runtime as *const RuntimeInner as usize;
    let settings = settings.clone();
    let item = item.clone();
    let item_id = item.id.clone();
    let content_hash = item.content_hash.clone();
    let spawn_result = std::thread::Builder::new()
        .name(format!("lan-clipboard-file-send-{item_id}"))
        .spawn(move || {
            let runtime = unsafe { &*(runtime_addr as *const RuntimeInner) };
            let delivered = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(peers.len());
                for peer in peers {
                    let settings_ref = &settings;
                    let item_ref = &item;
                    handles.push(scope.spawn(move || {
                        send_file_list_to_peer(runtime, settings_ref, item_ref, &peer)
                    }));
                }

                handles
                    .into_iter()
                    .filter_map(|handle| handle.join().ok())
                    .filter(|delivered| *delivered)
                    .count()
            });

            if delivered > 0 {
                mark_shared_fingerprint(runtime, &item.content_hash);
            }
            clear_content_inflight(runtime, &item.content_hash);
            push_log(
                runtime,
                "DEBUG",
                &format!(
                    "file stream completed item={} delivered={} attempted={}",
                    item.id, delivered, attempted
                ),
            );
        });

    if let Err(error) = spawn_result {
        set_error(
            runtime,
            format!("spawn file stream sender failed: item={item_id} error={error}"),
        );
        clear_content_inflight(runtime, &content_hash);
        return BroadcastReport {
            attempted,
            delivered: 0,
            deferred: false,
        };
    }

    BroadcastReport {
        attempted,
        delivered: attempted,
        deferred: true,
    }
}

fn send_payload_to_peer(
    runtime: &RuntimeInner,
    item: &ClipboardItem,
    payload: &[u8],
    peer: String,
) -> bool {
    let connect_timeout = Duration::from_millis(CONNECT_TIMEOUT_MS);
    let socket_addr = match peer.parse::<SocketAddr>() {
        Ok(socket_addr) => socket_addr,
        Err(_) => {
            push_log(runtime, "WARN", &format!("skip bad peer addr: {}", peer));
            return false;
        }
    };
    let transfer_id = send_transfer_id(&peer, item);
    upsert_transfer(
        runtime,
        TransferProgress {
            id: transfer_id.clone(),
            direction: "send".to_string(),
            peer: peer.clone(),
            item_kind: item.payload.kind().to_string(),
            item_label: payload_label(&item.payload),
            item_summary: payload_summary(&item.payload),
            item_id: item.id.clone(),
            transferred_bytes: 0,
            total_bytes: payload.len() as u64,
            percent: 0,
            status: "sending".to_string(),
            updated_at_ms: now_ms(),
            error: None,
        },
    );
    let stream = TcpStream::connect_timeout(&socket_addr, connect_timeout);
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
    tune_stream_for_send(&stream, payload.len() as u64);
    if let Ok(local_addr) = stream.local_addr() {
        remember_active_local_ip(runtime, local_addr.ip());
    }
    if write_all_with_progress(runtime, &mut stream, payload, &transfer_id).is_ok() {
        mark_transfer_completed(runtime, &transfer_id);
        mark_known_member(runtime, "addr", &peer);
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
    let mut stream =
        match TcpStream::connect_timeout(&socket_addr, Duration::from_millis(CONNECT_TIMEOUT_MS)) {
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
    tune_stream_for_send(&stream, *estimated_archive_bytes);
    if let Ok(local_addr) = stream.local_addr() {
        remember_active_local_ip(runtime, local_addr.ip());
    }

    let start = WireBody::FileStreamRawStart(FileStreamStart {
        item_id: item.id.clone(),
        content_hash: item.content_hash.clone(),
        created_at_us: item.created_at_us,
        source_device_id: item.source_device_id.clone(),
        size_bytes: *estimated_archive_bytes,
        top_level_names: top_level_names.clone(),
    });
    let item_age_to_first_payload_ms = item_age_ms(item);
    let start_frame_started = Instant::now();
    if let Err(error) = write_wire_body_to_stream(&mut stream, settings, &start) {
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
    if let Err(error) = stream_file_list_archive_to_peer(
        runtime,
        settings,
        &mut stream,
        item,
        paths,
        *estimated_archive_bytes,
        &transfer_id,
    ) {
        mark_transfer_failed(runtime, &transfer_id, error.to_string());
        push_log(
            runtime,
            "DEBUG",
            &format!("stream file archive failed peer={peer} error={error}"),
        );
        return false;
    }
    let stream_ms = elapsed_ms(stream_started);

    let end_frame_started = Instant::now();
    if let Err(error) = write_wire_body_to_stream(
        &mut stream,
        settings,
        &WireBody::FileStreamEnd {
            item_id: item.id.clone(),
        },
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
    settings: &Settings,
    stream: &mut TcpStream,
    item: &ClipboardItem,
    paths: &[PathBuf],
    size_bytes: u64,
    transfer_id: &str,
) -> anyhow::Result<()> {
    let archive_started_at = Instant::now();
    let mut writer = FileStreamNetworkWriter::new(
        runtime,
        settings,
        stream,
        transfer_id,
        size_bytes,
        item_marker(item),
    );
    clipboard::stream_file_bundle_archive(paths, &mut writer)?;
    writer.finish()?;
    let sent_bytes = writer.sent_bytes();
    let write_frame_ms = writer.write_frame_ms();
    let write_frame_max_ms = writer.write_frame_max_ms();
    let frame_count = writer.frame_count();
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
    Ok(())
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
        if transfer_should_abort(runtime, transfer_id) {
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
