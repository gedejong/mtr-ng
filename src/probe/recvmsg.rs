//! Reading a datagram with its control messages (`recvmsg`): the TTL or hop
//! limit it arrived with and, on Linux, an ICMP error from the error queue.

use std::{
    io, mem,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    os::fd::RawFd,
    ptr,
};

/// Turn on the boolean socket option `name` at `level`.
pub(super) fn set_option(fd: RawFd, level: libc::c_int, name: libc::c_int) -> io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: a valid descriptor and a c_int option value of the size passed.
    let ret = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            ptr::from_ref(&on).cast(),
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    match ret {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// One datagram or queued error, with what its control messages said.
pub(super) struct Message<'a> {
    pub(super) data: &'a [u8],
    /// The sender of a datagram; for a queued error, the destination of our probe
    pub(super) from: Option<IpAddr>,
    /// TTL or hop limit it arrived with, if the socket asked for it
    pub(super) ttl: Option<u8>,
    #[cfg(target_os = "linux")]
    pub(super) error: Option<QueuedError>,
}

/// An ICMP error from the error queue
#[cfg(target_os = "linux")]
pub(super) struct QueuedError {
    pub(super) origin: u8,
    pub(super) icmp_type: u8,
    /// The router that sent it
    pub(super) offender: Option<IpAddr>,
}

/// Read one datagram, or on Linux with `MSG_ERRQUEUE` one queued error. None when
/// nothing came within the socket's read timeout, or with `MSG_DONTWAIT` when
/// nothing is waiting. `control` holds the control messages; u64s keep them aligned.
pub(super) fn read<'a>(
    fd: RawFd,
    buf: &'a mut [u8],
    control: &mut [u64],
    flags: libc::c_int,
) -> io::Result<Option<Message<'a>>> {
    // SAFETY: all-zero is a valid sockaddr_storage and msghdr.
    let mut name: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_name = ptr::from_mut(&mut name).cast();
    msg.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = mem::size_of_val(control) as _;

    // SAFETY: msg points at buffers that outlive the call.
    let len = unsafe { libc::recvmsg(fd, &mut msg, flags) };
    if len < 0 {
        let e = io::Error::last_os_error();
        return match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
            _ => Err(e),
        };
    }

    let mut message = Message {
        data: &buf[..len as usize],
        // SAFETY: the kernel wrote msg_namelen bytes of a socket address.
        from: unsafe { socket_addr(ptr::from_ref(&name).cast(), msg.msg_namelen as usize) },
        ttl: None,
        #[cfg(target_os = "linux")]
        error: None,
    };
    // SAFETY: the CMSG macros walk the control buffer the kernel filled in, and
    // every read stays within the length of its control message.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            let header = &*cmsg;
            let data = libc::CMSG_DATA(cmsg);
            let data_len = header.cmsg_len as usize - (data as usize - cmsg as usize);
            match (header.cmsg_level, header.cmsg_type) {
                #[cfg(target_os = "linux")]
                (libc::IPPROTO_IP, libc::IP_TTL) if data_len >= mem::size_of::<libc::c_int>() => {
                    let ttl = ptr::read_unaligned(data.cast::<libc::c_int>());
                    message.ttl = u8::try_from(ttl).ok();
                }
                (libc::IPPROTO_IPV6, libc::IPV6_HOPLIMIT)
                    if data_len >= mem::size_of::<libc::c_int>() =>
                {
                    let ttl = ptr::read_unaligned(data.cast::<libc::c_int>());
                    message.ttl = u8::try_from(ttl).ok();
                }
                #[cfg(target_os = "linux")]
                (libc::IPPROTO_IP, libc::IP_RECVERR) | (libc::IPPROTO_IPV6, libc::IPV6_RECVERR)
                    if data_len >= mem::size_of::<libc::sock_extended_err>() =>
                {
                    let err = ptr::read_unaligned(data.cast::<libc::sock_extended_err>());
                    let size = mem::size_of::<libc::sock_extended_err>();
                    // SO_EE_OFFENDER: the sender's address follows the struct.
                    let offender = socket_addr(data.add(size), data_len - size);
                    message.error = Some(QueuedError {
                        origin: err.ee_origin,
                        icmp_type: err.ee_type,
                        offender,
                    });
                }
                _ => {}
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok(Some(message))
}

/// The address in a sockaddr_in or sockaddr_in6 of at most `len` bytes at `addr`.
///
/// # Safety
/// `addr` must point at `len` readable bytes.
unsafe fn socket_addr(addr: *const u8, len: usize) -> Option<IpAddr> {
    // The family follows a length byte on the BSDs, so read it from the struct.
    let family_at = mem::offset_of!(libc::sockaddr, sa_family);
    if len < family_at + mem::size_of::<libc::sa_family_t>() {
        return None;
    }
    // SAFETY: the caller vouches for `len` bytes, and each read checks its size.
    unsafe {
        let family = ptr::read_unaligned(addr.add(family_at).cast::<libc::sa_family_t>());
        match libc::c_int::from(family) {
            libc::AF_INET if len >= mem::size_of::<libc::sockaddr_in>() => {
                let sin = ptr::read_unaligned(addr.cast::<libc::sockaddr_in>());
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                    sin.sin_addr.s_addr,
                ))))
            }
            libc::AF_INET6 if len >= mem::size_of::<libc::sockaddr_in6>() => {
                let sin6 = ptr::read_unaligned(addr.cast::<libc::sockaddr_in6>());
                Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use socket2::{Domain, Protocol, Socket, Type};
    use std::os::fd::AsRawFd;

    #[test]
    fn reads_the_sender_and_the_hop_limit() {
        let receiver = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        let loopback: std::net::SocketAddr = "[::1]:0".parse().unwrap();
        if receiver.bind(&loopback.into()).is_err() {
            eprintln!("skipped: no IPv6 loopback here (a container without IPv6?)");
            return;
        }
        set_option(
            receiver.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_RECVHOPLIMIT,
        )
        .unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let sender = std::net::UdpSocket::bind(loopback).unwrap();
        let to = receiver.local_addr().unwrap().as_socket().unwrap();
        sender.send_to(b"hello", to).unwrap();

        let mut buf = [0u8; 64];
        let mut control = [0u64; 16];
        let message = read(receiver.as_raw_fd(), &mut buf, &mut control, 0)
            .unwrap()
            .expect("a datagram");
        assert_eq!(message.data, b"hello");
        assert_eq!(message.from, Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        // Unicast to ::1 leaves with the default hop limit, 64 on Linux and macOS.
        assert_eq!(message.ttl, Some(64));
    }
}
