use crate::protocol::ClipboardItem;
use socket2::{Domain, Protocol, SockAddr, SockRef, Socket, Type};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

const MIN_WRITE_TIMEOUT_MS: u64 = 8_000;
const MAX_WRITE_TIMEOUT_MS: u64 = 120_000;
const WRITE_TIMEOUT_BYTES_PER_MS: u64 = 512;
const TCP_BUFFER_BYTES: usize = 16 * 1024 * 1024;
const HANDSHAKE_TCP_BUFFER_BYTES: usize = 16 * 1024;
pub(super) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn connect_tcp_from(
    remote: &SocketAddr,
    local_ip: Option<&str>,
    timeout: Duration,
) -> std::io::Result<TcpStream> {
    let Some(local_ip) = local_ip.map(str::trim).filter(|value| !value.is_empty()) else {
        return TcpStream::connect_timeout(remote, timeout);
    };
    let local_ip = local_ip.parse::<IpAddr>().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "selected local ip is invalid",
        )
    })?;
    if local_ip.is_ipv4() != remote.ip().is_ipv4() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "selected local ip and peer use different address families",
        ));
    }

    let domain = if remote.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.bind(&SockAddr::from(SocketAddr::new(local_ip, 0)))?;
    socket.connect_timeout(&SockAddr::from(*remote), timeout)?;
    Ok(socket.into())
}

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
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(Some(timeout));
    tune_socket_buffers(stream);
}

pub(super) fn tune_stream_for_receive(stream: &TcpStream, payload_bytes: u64) {
    let timeout = write_timeout_for_payload(payload_bytes);
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(None);
    tune_socket_buffers(stream);
}

pub(super) fn tune_stream_for_handshake(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let socket = SockRef::from(stream);
    socket.set_send_buffer_size(HANDSHAKE_TCP_BUFFER_BYTES)?;
    socket.set_recv_buffer_size(HANDSHAKE_TCP_BUFFER_BYTES)?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};

    #[test]
    fn explicit_tcp_source_is_bound_before_connect() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind listener");
        let remote = listener.local_addr().expect("listener address");

        let stream = connect_tcp_from(&remote, Some("127.0.0.1"), Duration::from_millis(500))
            .expect("connect from explicit source");
        let (accepted, _) = listener.accept().expect("accept connection");

        assert_eq!(
            stream.local_addr().expect("client local address").ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        drop(accepted);
    }
}
