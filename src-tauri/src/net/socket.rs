use crate::protocol::ClipboardItem;
use socket2::SockRef;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

const MIN_WRITE_TIMEOUT_MS: u64 = 8_000;
const MAX_WRITE_TIMEOUT_MS: u64 = 120_000;
const WRITE_TIMEOUT_BYTES_PER_MS: u64 = 512;
const TCP_BUFFER_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn send_transfer_id(peer: &str, item: &ClipboardItem) -> String {
    let peer_key = peer
        .parse::<SocketAddr>()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| peer.to_string());
    format!("send:{peer_key}:{}", item.id)
}

pub(super) fn write_timeout_for_payload(payload_bytes: u64) -> Duration {
    let estimated_ms = payload_bytes
        .checked_div(WRITE_TIMEOUT_BYTES_PER_MS)
        .unwrap_or(MAX_WRITE_TIMEOUT_MS)
        .saturating_add(MIN_WRITE_TIMEOUT_MS);
    Duration::from_millis(estimated_ms.clamp(MIN_WRITE_TIMEOUT_MS, MAX_WRITE_TIMEOUT_MS))
}

pub(super) fn tune_stream_for_send(stream: &TcpStream, payload_bytes: u64) {
    let timeout = write_timeout_for_payload(payload_bytes);
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(timeout));
    tune_socket_buffers(stream);
}

pub(super) fn tune_stream_for_receive(stream: &TcpStream, payload_bytes: u64) {
    let timeout = write_timeout_for_payload(payload_bytes);
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(timeout));
    tune_socket_buffers(stream);
}

pub(super) fn is_self_socket_addr(
    socket_addr: &SocketAddr,
    effective_local_ip: Option<&str>,
) -> bool {
    if socket_addr.ip().is_loopback() {
        return true;
    }
    effective_local_ip
        .and_then(|ip| ip.parse::<IpAddr>().ok())
        .map(|local_ip| socket_addr.ip() == local_ip)
        .unwrap_or(false)
}

fn tune_socket_buffers(stream: &TcpStream) {
    let socket = SockRef::from(stream);
    let _ = socket.set_send_buffer_size(TCP_BUFFER_BYTES);
    let _ = socket.set_recv_buffer_size(TCP_BUFFER_BYTES);
}
