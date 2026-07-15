use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::num::NonZeroU32;

pub(super) fn bind_discovery_socket(
    selected_ip: Option<Ipv4Addr>,
    port: u16,
) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.bind(&SockAddr::from(SocketAddr::from((
        Ipv4Addr::UNSPECIFIED,
        port,
    ))))?;
    if let Some(selected_ip) = selected_ip {
        let interface_index = interface_index_for_ipv4(selected_ip)?;
        bind_to_interface(&socket, interface_index)?;
    }
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

fn interface_index_for_ipv4(selected_ip: Ipv4Addr) -> io::Result<NonZeroU32> {
    let interface = if_addrs::get_if_addrs()?
        .into_iter()
        .find(|interface| interface.ip() == IpAddr::V4(selected_ip))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("selected local ip is no longer assigned: {selected_ip}"),
            )
        })?;
    interface.index.and_then(NonZeroU32::new).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("selected local ip has no usable interface index: {selected_ip}"),
        )
    })
}

#[cfg(target_os = "macos")]
fn bind_to_interface(socket: &Socket, interface_index: NonZeroU32) -> io::Result<()> {
    socket.bind_device_by_index_v4(Some(interface_index))
}

#[cfg(windows)]
fn bind_to_interface(socket: &Socket, interface_index: NonZeroU32) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, IPPROTO_IP, IP_ADD_IFLIST, IP_IFLIST, IP_UNICAST_IF, SOCKET_ERROR,
    };

    fn set_option(socket: &Socket, option: i32, value: u32) -> io::Result<()> {
        let result = unsafe {
            setsockopt(
                socket.as_raw_socket() as usize,
                IPPROTO_IP,
                option,
                (&value as *const u32).cast(),
                std::mem::size_of::<u32>() as i32,
            )
        };
        if result == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    let receive_index = interface_index.get();
    set_option(socket, IP_IFLIST, 1)?;
    set_option(socket, IP_ADD_IFLIST, receive_index)?;
    set_option(socket, IP_UNICAST_IF, receive_index.to_be())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn bind_to_interface(_socket: &Socket, _interface_index: NonZeroU32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "interface-scoped UDP discovery is unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_discovery_socket_keeps_the_shared_receive_port() {
        let socket = bind_discovery_socket(None, 0).expect("bind wildcard discovery socket");
        let local = socket.local_addr().expect("read discovery socket address");
        assert_eq!(local.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_ne!(local.port(), 0);
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn selected_ipv4_resolves_to_a_kernel_interface_index() {
        let selected = if_addrs::get_if_addrs()
            .expect("list interfaces")
            .into_iter()
            .find_map(|interface| match (interface.ip(), interface.index) {
                (IpAddr::V4(ip), Some(index)) if index != 0 => Some(ip),
                _ => None,
            })
            .expect("an indexed IPv4 interface");

        let index = interface_index_for_ipv4(selected).expect("resolve selected interface");
        assert_ne!(index.get(), 0);
        bind_discovery_socket(Some(selected), 0).expect("bind interface-scoped discovery socket");
    }
}
