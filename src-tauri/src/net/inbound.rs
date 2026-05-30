use super::display::{file_stream_label, file_stream_summary, payload_label, payload_summary};
use super::file_stream::RawFileStreamReader;
use super::logs::{push_log, set_error};
use super::marker::{file_stream_marker, is_stale_marker, update_latest_marker};
use super::members::mark_known_member;
use super::metrics::{elapsed_ms, format_mib_per_second, now_ms};
use super::transfers::{
    canonical_receive_transfer_id, mark_transfer_failed, update_transfer_progress, upsert_transfer,
    TransferProgress,
};
use super::wire::{
    decode_wire_body_bytes, read_wire_body_from_stream, read_wire_frame, FileStreamStart, WireBody,
};
use super::{
    enqueue_inbound_item, prune_stale_queue_entries, remember_active_local_ip,
    sanitize_file_component, should_skip_remote_item, RuntimeInner,
};
use crate::clipboard;
use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::Settings;
use std::collections::HashMap;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

struct IncomingFileStream {
    meta: FileStreamStart,
    archive_path: PathBuf,
    received_bytes: u64,
    peer: String,
}

pub(super) fn spawn_incoming_connection_worker(
    runtime: Arc<RuntimeInner>,
    settings: Settings,
    device_id: String,
    stream: TcpStream,
) {
    let _ = std::thread::Builder::new()
        .name("lan-clipboard-incoming".to_string())
        .spawn(move || {
            if let Err(error) = handle_incoming(&runtime, &settings, stream, &device_id) {
                set_error(&runtime, format!("incoming handler failed: {error}"));
            }
        });
}

fn handle_incoming(
    runtime: &RuntimeInner,
    settings: &Settings,
    stream: TcpStream,
    device_id: &str,
) -> anyhow::Result<()> {
    let remote_addr = stream.peer_addr().ok().map(|addr| addr.to_string());
    if let Ok(local_addr) = stream.local_addr() {
        remember_active_local_ip(runtime, local_addr.ip());
    }
    let mut stream = stream;
    let mut incoming_files: HashMap<String, IncomingFileStream> = HashMap::new();
    loop {
        let Some(frame_bytes) = (match read_wire_frame(&mut stream) {
            Ok(frame) => Ok(frame),
            Err(error) => {
                discard_incomplete_incoming_files(
                    runtime,
                    &mut incoming_files,
                    "发送方连接中断，已丢弃未完成传输",
                );
                Err(error)
            }
        })?
        else {
            discard_incomplete_incoming_files(
                runtime,
                &mut incoming_files,
                "发送方已下线，已丢弃未完成传输",
            );
            break;
        };
        let body = decode_wire_body_bytes(&frame_bytes, settings)?;
        match body {
            WireBody::ClipboardItem(item) => {
                handle_incoming_item(
                    runtime,
                    remote_addr.as_deref().unwrap_or("未知来源"),
                    item,
                    device_id,
                );
            }
            WireBody::FileStreamStart(meta) => {
                let canonical_transfer_id =
                    format!("recv:{}:{}", meta.source_device_id, meta.item_id);
                upsert_transfer(
                    runtime,
                    TransferProgress {
                        id: canonical_transfer_id.clone(),
                        direction: "receive".to_string(),
                        peer: remote_addr.as_deref().unwrap_or("未知来源").to_string(),
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
                    mark_transfer_failed(
                        runtime,
                        &canonical_transfer_id,
                        "已被更新内容替代".to_string(),
                    );
                    push_log(
                        runtime,
                        "DEBUG",
                        &format!(
                            "drop stale inbound file stream start item={} source_device_id={}",
                            meta.item_id, meta.source_device_id
                        ),
                    );
                    continue;
                }
                if update_latest_marker(runtime, marker) {
                    prune_stale_queue_entries(runtime);
                }

                if let Some(previous) = incoming_files.remove(&meta.item_id) {
                    let _ = std::fs::remove_file(&previous.archive_path);
                }

                let safe_source = sanitize_file_component(&meta.source_device_id);
                let safe_item = sanitize_file_component(&meta.item_id);
                let archive_path = std::env::temp_dir().join(format!(
                    "lan-clipboard-incoming-{safe_source}-{safe_item}.archive"
                ));
                let _ = std::fs::remove_file(&archive_path);
                if let Err(error) = std::fs::File::create(&archive_path) {
                    mark_transfer_failed(runtime, &canonical_transfer_id, error.to_string());
                    push_log(
                        runtime,
                        "WARN",
                        &format!(
                            "create inbound temp archive failed item={} path={} error={}",
                            meta.item_id,
                            archive_path.display(),
                            error
                        ),
                    );
                    continue;
                }

                incoming_files.insert(
                    meta.item_id.clone(),
                    IncomingFileStream {
                        meta,
                        archive_path,
                        received_bytes: 0,
                        peer: remote_addr.as_deref().unwrap_or("未知来源").to_string(),
                    },
                );
            }
            WireBody::FileStreamChunk { item_id, bytes } => {
                if let Some(file_stream) = incoming_files.get_mut(&item_id) {
                    let transfer_id = format!(
                        "recv:{}:{}",
                        file_stream.meta.source_device_id, file_stream.meta.item_id
                    );
                    match std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&file_stream.archive_path)
                        .and_then(|mut file| std::io::Write::write_all(&mut file, &bytes))
                    {
                        Ok(()) => {
                            file_stream.received_bytes = file_stream
                                .received_bytes
                                .saturating_add(bytes.len() as u64);
                            update_transfer_progress(
                                runtime,
                                &transfer_id,
                                file_stream.received_bytes,
                                file_stream.meta.size_bytes,
                            );
                        }
                        Err(error) => {
                            mark_transfer_failed(runtime, &transfer_id, error.to_string());
                            push_log(
                                runtime,
                                "WARN",
                                &format!(
                                    "write inbound file chunk failed item={} path={} error={}",
                                    file_stream.meta.item_id,
                                    file_stream.archive_path.display(),
                                    error
                                ),
                            );
                            let _ = std::fs::remove_file(&file_stream.archive_path);
                            incoming_files.remove(&item_id);
                        }
                    }
                }
            }
            WireBody::FileStreamEnd { item_id } => {
                if let Some(file_stream) = incoming_files.remove(&item_id) {
                    let transfer_id = format!(
                        "recv:{}:{}",
                        file_stream.meta.source_device_id, file_stream.meta.item_id
                    );
                    if file_stream.received_bytes != file_stream.meta.size_bytes {
                        mark_transfer_failed(
                            runtime,
                            &transfer_id,
                            format!(
                                "文件接收不完整：已接收 {} 字节，期望 {} 字节",
                                file_stream.received_bytes, file_stream.meta.size_bytes
                            ),
                        );
                        push_log(
                            runtime,
                            "WARN",
                            &format!(
                                "incomplete inbound file stream item={} received_bytes={} expected_bytes={} peer={}",
                                file_stream.meta.item_id,
                                file_stream.received_bytes,
                                file_stream.meta.size_bytes,
                                file_stream.peer
                            ),
                        );
                        let _ = std::fs::remove_file(&file_stream.archive_path);
                        continue;
                    }

                    let item = ClipboardItem {
                        id: file_stream.meta.item_id.clone(),
                        content_hash: file_stream.meta.content_hash,
                        created_at_us: file_stream.meta.created_at_us,
                        source_device_id: file_stream.meta.source_device_id,
                        size_bytes: file_stream.meta.size_bytes,
                        payload: ClipboardPayload::FileBundlePath {
                            archive_path: file_stream.archive_path,
                            top_level_names: file_stream.meta.top_level_names,
                        },
                    };
                    handle_incoming_item(runtime, &file_stream.peer, item, device_id);
                }
            }
            WireBody::FileStreamRawStart(meta) => {
                receive_raw_file_stream(
                    runtime,
                    settings,
                    &mut stream,
                    remote_addr.as_deref().unwrap_or("未知来源"),
                    meta,
                    device_id,
                )?;
            }
        }
    }
    Ok(())
}

fn receive_raw_file_stream(
    runtime: &RuntimeInner,
    settings: &Settings,
    stream: &mut TcpStream,
    peer: &str,
    meta: FileStreamStart,
    device_id: &str,
) -> anyhow::Result<()> {
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
    if update_latest_marker(runtime, marker) {
        prune_stale_queue_entries(runtime);
    }

    let safe_source = sanitize_file_component(&meta.source_device_id);
    let safe_item = sanitize_file_component(&meta.item_id);
    let safe_bundle_id = format!("{safe_source}-{safe_item}");

    let stream_started = Instant::now();
    let mut reader = RawFileStreamReader::new(
        runtime,
        settings,
        stream,
        &transfer_id,
        meta.size_bytes,
        file_stream_marker(&meta),
    );
    let bundle_dir =
        match clipboard::unpack_file_bundle_archive_reader(&mut reader, &safe_bundle_id) {
            Ok(bundle_dir) => bundle_dir,
            Err(error) => {
                mark_transfer_failed(runtime, &transfer_id, error.to_string());
                return Err(error.into());
            }
        };
    if let Err(error) = reader.ensure_complete() {
        let _ = std::fs::remove_dir_all(&bundle_dir);
        mark_transfer_failed(runtime, &transfer_id, error.to_string());
        return Err(error);
    }
    drop(reader);
    let stream_ms = elapsed_ms(stream_started);

    let end_frame_started = Instant::now();
    match read_wire_body_from_stream(stream, settings)? {
        Some(WireBody::FileStreamEnd { item_id }) if item_id == meta.item_id => {}
        Some(_) => {
            let _ = std::fs::remove_dir_all(&bundle_dir);
            mark_transfer_failed(runtime, &transfer_id, "文件流结束帧不匹配".to_string());
            return Err(anyhow::anyhow!("unexpected raw file stream end frame"));
        }
        None => {
            let _ = std::fs::remove_dir_all(&bundle_dir);
            mark_transfer_failed(
                runtime,
                &transfer_id,
                "发送方已下线，已丢弃未完成传输".to_string(),
            );
            return Err(anyhow::anyhow!(
                "sender disconnected before raw file stream end"
            ));
        }
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
    handle_incoming_item(runtime, peer, item, device_id);
    Ok(())
}

fn discard_incomplete_incoming_files(
    runtime: &RuntimeInner,
    incoming_files: &mut HashMap<String, IncomingFileStream>,
    reason: &str,
) {
    for (_, file_stream) in incoming_files.drain() {
        let _ = std::fs::remove_file(&file_stream.archive_path);
        let transfer_id = format!(
            "recv:{}:{}",
            file_stream.meta.source_device_id, file_stream.meta.item_id
        );
        mark_transfer_failed(runtime, &transfer_id, reason.to_string());
        push_log(
            runtime,
            "WARN",
            &format!(
                "discard incomplete inbound file item={} received_bytes={} expected_bytes={} peer={} reason={}",
                file_stream.meta.item_id,
                file_stream.received_bytes,
                file_stream.meta.size_bytes,
                file_stream.peer,
                reason
            ),
        );
    }
}

fn handle_incoming_item(runtime: &RuntimeInner, peer: &str, item: ClipboardItem, device_id: &str) {
    let canonical_transfer_id = canonical_receive_transfer_id(&item);
    if item.source_device_id == device_id {
        return;
    }
    mark_known_member(runtime, "device", &item.source_device_id);
    if should_skip_remote_item(runtime, &item) {
        return;
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
}
