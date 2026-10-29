//! Linux ping sockets: ICMP without privileges, for users whose group is in
//! `net.ipv4.ping_group_range` (every group on most distributions since systemd
//! 243; the same range covers IPv6).
//!
//! The kernel fills in the echo identifier (the socket's "port") and the
//! checksum, and hands echo replies to the socket like datagrams, without an IP
//! header. ICMP errors such as Time Exceeded go to the socket's error queue when
//! `IP_RECVERR` is on, with the router as the offender and our own probe as the
//! data. A router's MPLS labels don't survive that, so they need a raw socket.

use super::{echo_fields, IcmpReply, IcmpResponseType};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    io, mem,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::fd::{AsRawFd, RawFd},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Open a ping socket and bind it, so the kernel picks the identifier that every
/// probe will carry. Returns the socket and that identifier.
pub(super) fn open(ipv6: bool) -> io::Result<(Socket, u16)> {
    let (domain, protocol) = match ipv6 {
        false => (Domain::IPV4, Protocol::ICMPV4),
        true => (Domain::IPV6, Protocol::ICMPV6),
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(protocol))?;
    let fd = socket.as_raw_fd();
    if ipv6 {
        set_option(fd, libc::SOL_IPV6, libc::IPV6_RECVERR)?;
        set_option(fd, libc::SOL_IPV6, libc::IPV6_RECVHOPLIMIT)?;
    } else {
        set_option(fd, libc::SOL_IP, libc::IP_RECVERR)?;
        set_option(fd, libc::SOL_IP, libc::IP_RECVTTL)?;
    }
    let any = match ipv6 {
        false => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        true => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    socket.bind(&SocketAddr::new(any, 0).into())?;
    let ident = socket
        .local_addr()?
        .as_socket()
        .map_or(0, |addr| addr.port());
    Ok((socket, ident))
}

fn set_option(fd: RawFd, level: libc::c_int, name: libc::c_int) -> io::Result<()> {
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

/// Deliver every echo reply and ICMP error that reaches `socket`, each with the
/// time it was read, until `stop` is set or `deliver` returns false.
pub(super) fn receive(
    socket: &Socket,
    ipv6: bool,
    poll_interval: Duration,
    stop: &AtomicBool,
    mut deliver: impl FnMut(IcmpReply, Instant) -> bool,
) {
    let fd = socket.as_raw_fd();
    let mut buf = [0u8; 1500];
    // u64s keep the control messages aligned
    let mut control = [0u64; 64];
    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = wait(fd, poll_interval) {
            tracing::warn!("ICMP receive failed: {e}");
            std::thread::sleep(poll_interval);
            continue;
        }
        // Errors first: while one is queued, a plain read fails with its errno.
        for flags in [libc::MSG_ERRQUEUE, 0] {
            loop {
                let message = match read(fd, &mut buf, &mut control, flags) {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(e) => {
                        // The errno of an error that sits on the queue
                        tracing::debug!("ping socket read: {e}");
                        break;
                    }
                };
                let received_at = Instant::now();
                if let Some(reply) = message.reply(ipv6) {
                    if !deliver(reply, received_at) {
                        return;
                    }
                }
            }
        }
    }
}

/// Wait until something can be read or an error is queued, or `timeout` passes.
fn wait(fd: RawFd, timeout: Duration) -> io::Result<()> {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLERR,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    match unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as libc::c_int) } {
        -1 if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted => {
            Err(io::Error::last_os_error())
        }
        _ => Ok(()),
    }
}

/// One datagram or queued error, with what its control messages said.
struct Message<'a> {
    data: &'a [u8],
    /// The sender of a datagram; for a queued error, the destination of our probe
    from: Option<IpAddr>,
    /// TTL or hop limit the reply arrived with
    ttl: Option<u8>,
    error: Option<QueuedError>,
}

/// An ICMP error from the error queue
struct QueuedError {
    origin: u8,
    icmp_type: u8,
    /// The router that sent it
    offender: Option<IpAddr>,
}

impl Message<'_> {
    fn reply(&self, ipv6: bool) -> Option<IcmpReply> {
        let echo = self.data.get(..8)?;
        let (kind, source) = match &self.error {
            None => {
                let echo_reply = if ipv6 { 129 } else { 0 };
                if echo[0] != echo_reply {
                    return None;
                }
                (IcmpResponseType::EchoReply, self.from?)
            }
            Some(error) => {
                let icmp_origin = match ipv6 {
                    false => libc::SO_EE_ORIGIN_ICMP,
                    true => libc::SO_EE_ORIGIN_ICMP6,
                };
                // The data is the probe we sent.
                let echo_request = if ipv6 { 128 } else { 8 };
                if error.origin != icmp_origin || echo[0] != echo_request {
                    return None;
                }
                let kind = match (ipv6, error.icmp_type) {
                    (false, 11) | (true, 3) => IcmpResponseType::TimeExceeded,
                    (false, 3) | (true, 1) => IcmpResponseType::DestinationUnreachable,
                    _ => return None,
                };
                (kind, error.offender?)
            }
        };
        Some(IcmpReply {
            kind,
            source,
            probe: echo_fields(echo),
            ttl: self.ttl,
            mpls: Vec::new(),
        })
    }
}

/// Read one datagram, or with `MSG_ERRQUEUE` one queued error, without blocking.
fn read<'a>(
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
    let len = unsafe { libc::recvmsg(fd, &mut msg, flags | libc::MSG_DONTWAIT) };
    if len < 0 {
        let e = io::Error::last_os_error();
        return match e.kind() {
            io::ErrorKind::WouldBlock => Ok(None),
            _ => Err(e),
        };
    }

    let mut message = Message {
        data: &buf[..len as usize],
        // SAFETY: the kernel wrote msg_namelen bytes of a socket address.
        from: unsafe { socket_addr(ptr::from_ref(&name).cast(), msg.msg_namelen as usize) },
        ttl: None,
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
                (libc::SOL_IP, libc::IP_TTL) | (libc::SOL_IPV6, libc::IPV6_HOPLIMIT)
                    if data_len >= mem::size_of::<libc::c_int>() =>
                {
                    let ttl = ptr::read_unaligned(data.cast::<libc::c_int>());
                    message.ttl = u8::try_from(ttl).ok();
                }
                (libc::SOL_IP, libc::IP_RECVERR) | (libc::SOL_IPV6, libc::IPV6_RECVERR)
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
    if len < mem::size_of::<libc::sa_family_t>() {
        return None;
    }
    // SAFETY: the caller vouches for `len` bytes, and each read checks its size.
    unsafe {
        match libc::c_int::from(ptr::read_unaligned(addr.cast::<libc::sa_family_t>())) {
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
    use crate::probe::Quoted;

    fn probe(kind: u8, seq: u16) -> Vec<u8> {
        let mut p = vec![kind, 0, 0, 0, 0x12, 0x34];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&[0, 0]);
        p
    }

    fn message<'a>(data: &'a [u8], error: Option<QueuedError>) -> Message<'a> {
        Message {
            data,
            from: Some(IpAddr::from([192, 0, 2, 9])),
            ttl: Some(61),
            error,
        }
    }

    #[test]
    fn an_echo_reply_comes_from_the_sender() {
        let data = probe(0, 40000);
        let reply = message(&data, None).reply(false).unwrap();
        assert_eq!(reply.kind, IcmpResponseType::EchoReply);
        assert_eq!(reply.source, IpAddr::from([192, 0, 2, 9]));
        let echo = Quoted::Echo {
            id: 0x1234,
            seq: 40000,
        };
        assert_eq!((reply.probe, reply.ttl), (echo, Some(61)));
        let request = probe(8, 40000);
        assert!(
            message(&request, None).reply(false).is_none(),
            "our own probe"
        );
    }

    #[test]
    fn a_queued_error_comes_from_the_offender_and_names_our_probe() {
        let router = IpAddr::from([10, 0, 0, 1]);
        let error = |icmp_type| QueuedError {
            origin: libc::SO_EE_ORIGIN_ICMP,
            icmp_type,
            offender: Some(router),
        };
        let data = probe(8, 40001);
        let reply = message(&data, Some(error(11))).reply(false).unwrap();
        assert_eq!(reply.kind, IcmpResponseType::TimeExceeded);
        let echo = Quoted::Echo {
            id: 0x1234,
            seq: 40001,
        };
        assert_eq!((reply.source, reply.probe), (router, echo));
        let reply = message(&data, Some(error(3))).reply(false).unwrap();
        assert_eq!(reply.kind, IcmpResponseType::DestinationUnreachable);
        assert!(message(&data, Some(error(12))).reply(false).is_none());

        let local = QueuedError {
            origin: libc::SO_EE_ORIGIN_LOCAL,
            ..error(11)
        };
        assert!(message(&data, Some(local)).reply(false).is_none());
    }

    #[test]
    fn icmpv6_numbers_its_messages_differently() {
        let router: IpAddr = "2001:db8::1".parse().unwrap();
        let data = probe(128, 40002);
        let time_exceeded = QueuedError {
            origin: libc::SO_EE_ORIGIN_ICMP6,
            icmp_type: 3,
            offender: Some(router),
        };
        let reply = message(&data, Some(time_exceeded)).reply(true).unwrap();
        assert_eq!(
            (reply.kind, reply.source),
            (IcmpResponseType::TimeExceeded, router)
        );
        let echo_reply = probe(129, 40003);
        let reply = message(&echo_reply, None).reply(true).unwrap();
        let echo = Quoted::Echo {
            id: 0x1234,
            seq: 40003,
        };
        assert_eq!(
            (reply.kind, reply.probe),
            (IcmpResponseType::EchoReply, echo)
        );
    }
}
