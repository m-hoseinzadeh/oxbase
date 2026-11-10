//! Which process opened a connection to a tunnel's loopback listener.
//!
//! Every local user and process can connect to `127.0.0.1`, and each accepted connection would
//! ride on the Oxbase user's SSH session. A tunnel therefore serves only connections whose client
//! end is a socket of this process (the drivers' own connections, audit lead 19). The client end
//! of a connection accepted from `peer` on a listener at `listener` is a TCP socket bound to
//! `peer` and connected to `listener`; the connection is ours when this process holds a socket
//! with exactly those two addresses:
//!
//! - Unix (Linux, macOS): every open descriptor of this process (listed in `/proc/self/fd` or
//!   `/dev/fd`) is asked for its local and peer address (`getsockname` / `getpeername`).
//! - Windows: the IPv4 connection table with owning process ids (`GetExtendedTcpTable`).
//! - Anything else, or any failure: not ours. The caller closes the connection without opening
//!   a channel.
#![allow(unsafe_code)]

use std::io;
use std::net::{IpAddr, SocketAddr};

/// Whether the connection accepted from `peer` on the listener bound to `listener` was opened
/// by a socket of this process.
pub(crate) fn opened_by_this_process(peer: SocketAddr, listener: SocketAddr) -> io::Result<bool> {
    imp::find(canonical(peer), canonical(listener))
}

/// `addr` with an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`, a dual-stack socket's view of
/// an IPv4 peer) as plain IPv4.
fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => v6
            .ip()
            .to_ipv4_mapped()
            .map_or(addr, |ip| SocketAddr::new(IpAddr::V4(ip), v6.port())),
        SocketAddr::V4(_) => addr,
    }
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::net::SocketAddr;
    use std::os::fd::{BorrowedFd, RawFd};

    use super::canonical;

    /// This process's open descriptors, one entry per descriptor number.
    const FD_DIR: &str = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };

    pub(super) fn find(peer: SocketAddr, listener: SocketAddr) -> io::Result<bool> {
        for entry in std::fs::read_dir(FD_DIR)? {
            let name = entry?.file_name();
            let Some(fd) = name
                .to_str()
                .and_then(|n| n.parse::<RawFd>().ok())
                .filter(|fd| *fd >= 0)
            else {
                continue;
            };
            // SAFETY: `fd` is a non-negative descriptor number of this process, listed just
            // now, and it is only borrowed for the two address queries below: nothing is read
            // from, written to or closed through it (`SockRef` never closes). Should another
            // thread close or reuse the number meanwhile, the queries fail (EBADF, ENOTSOCK) or
            // describe that other descriptor, which is harmless.
            let fd = unsafe { BorrowedFd::borrow_raw(fd) };
            let socket = socket2::SockRef::from(&fd);
            let (Ok(local), Ok(remote)) = (socket.local_addr(), socket.peer_addr()) else {
                continue;
            };
            let (Some(local), Some(remote)) = (local.as_socket(), remote.as_socket()) else {
                continue;
            };
            if canonical(local) == peer && canonical(remote) == listener {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, TCP_TABLE_OWNER_PID_CONNECTIONS,
    };

    const AF_INET: u32 = 2;
    const NO_ERROR: u32 = 0;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    /// `u32`s per `MIB_TCPROW_OWNER_PID`: state, local address, local port, remote address,
    /// remote port, owning process id.
    const ROW: usize = 6;

    pub(super) fn find(peer: SocketAddr, listener: SocketAddr) -> io::Result<bool> {
        let (SocketAddr::V4(peer), SocketAddr::V4(listener)) = (peer, listener) else {
            return Ok(false);
        };
        let table = connections()?;
        // `MIB_TCPTABLE_OWNER_PID`: the number of rows, then the rows.
        let count = table
            .first()
            .and_then(|n| usize::try_from(*n).ok())
            .unwrap_or(0);
        let own = std::process::id();
        Ok(table
            .get(1..)
            .unwrap_or_default()
            .chunks_exact(ROW)
            .take(count)
            .any(|row| {
                endpoint(row[1], row[2]) == peer
                    && endpoint(row[3], row[4]) == listener
                    && row[5] == own
            }))
    }

    /// An address and port as the table stores them (both in network byte order).
    fn endpoint(addr: u32, port: u32) -> SocketAddrV4 {
        let port = port.to_ne_bytes();
        SocketAddrV4::new(
            Ipv4Addr::from(addr.to_ne_bytes()),
            u16::from_be_bytes([port[0], port[1]]),
        )
    }

    /// The IPv4 TCP connection table with owning process ids, as `u32`s.
    fn connections() -> io::Result<Vec<u32>> {
        let mut table: Vec<u32> = Vec::new();
        for _ in 0..8 {
            let mut size = u32::try_from(table.len() * 4).unwrap_or(u32::MAX);
            let buffer: *mut core::ffi::c_void = if table.is_empty() {
                std::ptr::null_mut()
            } else {
                table.as_mut_ptr().cast()
            };
            // SAFETY: `buffer` is null with `size` 0, or points to `table`, which holds `size`
            // writable bytes, aligned for the `u32` fields the call writes. `size` is a valid
            // `u32` the call updates. Both stay valid for the duration of the call.
            let status = unsafe {
                GetExtendedTcpTable(
                    buffer,
                    &mut size,
                    0,
                    AF_INET,
                    TCP_TABLE_OWNER_PID_CONNECTIONS,
                    0,
                )
            };
            match status {
                NO_ERROR => return Ok(table),
                // The table can grow between two calls: leave room for a few more rows.
                ERROR_INSUFFICIENT_BUFFER => {
                    let needed = usize::try_from(size).unwrap_or(usize::MAX) / 4 + 16 * ROW;
                    table = vec![0; needed];
                }
                other => {
                    return Err(io::Error::from_raw_os_error(
                        i32::try_from(other).unwrap_or(i32::MAX),
                    ));
                }
            }
        }
        Err(io::Error::other("the TCP connection table kept growing"))
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::io;
    use std::net::SocketAddr;

    pub(super) fn find(_peer: SocketAddr, _listener: SocketAddr) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot tell which process opened a local connection on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};

    #[test]
    fn a_connection_of_this_process_is_ours() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (_accepted, peer) = listener.accept().unwrap();
        assert_eq!(peer, client.local_addr().unwrap());
        assert!(opened_by_this_process(peer, addr).unwrap());
    }

    #[test]
    fn an_unknown_peer_is_not_ours() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = TcpStream::connect(addr).unwrap();
        let (_accepted, peer) = listener.accept().unwrap();
        // Another port on the same host, and the right port towards another listener.
        let other = SocketAddr::new(peer.ip(), peer.port().wrapping_add(1).max(1));
        assert!(!opened_by_this_process(other, addr).unwrap());
        let elsewhere = SocketAddr::new(addr.ip(), addr.port().wrapping_add(1).max(1));
        assert!(!opened_by_this_process(peer, elsewhere).unwrap());
    }

    #[test]
    fn mapped_addresses_compare_as_ipv4() {
        let mapped = SocketAddr::new(IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped()), 5432);
        assert_eq!(
            canonical(mapped),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5432)
        );
        let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5432);
        assert_eq!(canonical(v6), v6);
    }
}
