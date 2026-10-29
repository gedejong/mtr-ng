//! Linux ping sockets: ICMP without privileges, for users whose group is in
//! `net.ipv4.ping_group_range` (every group on most distributions since systemd
//! 243; the same range covers IPv6).
//!
//! The kernel fills in the echo identifier (the socket's "port") and the
//! checksum, and hands echo replies to the socket like datagrams, without an IP
//! header. ICMP errors such as Time Exceeded go to the socket's error queue when
//! `IP_RECVERR` is on, with the router as the offender and our own probe as the
//! data. A router's MPLS labels don't survive that, so they need a raw socket.

use super::recvmsg::{read, set_option, Message};
use super::{echo_fields, IcmpReply, IcmpResponseType};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::fd::{AsRawFd, RawFd},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Open a ping socket and bind it, so the kernel picks the identifier that every
/// probe will carry. Returns the socket and that identifier. The engine asks for
/// IPv6 hop limits on every socket; IPv4 TTLs are asked for here, since a ping
/// socket's replies come without their IP header.
pub(super) fn open(ipv6: bool) -> io::Result<(Socket, u16)> {
    let (domain, protocol) = match ipv6 {
        false => (Domain::IPV4, Protocol::ICMPV4),
        true => (Domain::IPV6, Protocol::ICMPV6),
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(protocol))?;
    let fd = socket.as_raw_fd();
    if ipv6 {
        set_option(fd, libc::IPPROTO_IPV6, libc::IPV6_RECVERR)?;
    } else {
        set_option(fd, libc::IPPROTO_IP, libc::IP_RECVERR)?;
        set_option(fd, libc::IPPROTO_IP, libc::IP_RECVTTL)?;
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
                let message = match read(fd, &mut buf, &mut control, flags | libc::MSG_DONTWAIT) {
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
                let kind = match (ipv6, error.icmp_type, error.code) {
                    (false, 11, _) | (true, 3, _) => IcmpResponseType::TimeExceeded,
                    (false, 3, 4) | (true, 2, _) => IcmpResponseType::TooBig {
                        mtu: error.info.try_into().unwrap_or(u16::MAX),
                    },
                    (false, 3, _) | (true, 1, _) => IcmpResponseType::DestinationUnreachable,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{recvmsg::QueuedError, Quoted};

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
            code: 0,
            info: 0,
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
        let frag_needed = QueuedError {
            code: 4,
            info: 1400,
            ..error(3)
        };
        let reply = message(&data, Some(frag_needed)).reply(false).unwrap();
        assert_eq!(reply.kind, IcmpResponseType::TooBig { mtu: 1400 });

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
            code: 0,
            info: 0,
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
