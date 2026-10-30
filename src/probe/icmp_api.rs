//! Windows: echo requests through the ICMP API (`IcmpSendEcho2Ex`,
//! `Icmp6SendEcho2`), as tracert and WinMTR send them. It needs no
//! administrator, but Windows builds each echo request itself, so a flow can't
//! keep its checksum (no `--flows`), and passes on neither UDP probes, ICMP
//! extensions (no MPLS labels) nor the MTU a router reports (no `--pmtu`).
//!
//! Each call blocks until the answer or the timeout, so every probe runs on a
//! thread of its own, with its own handle. The API reports whole milliseconds;
//! the thread times the call with `Instant` instead.

use super::{source_address, IcmpResponseType, Prober, Reply};
use crate::icmp_ext::Extensions;
use crate::tracer::PROBE_TIMEOUT;
use socket2::SockAddr;
use std::{
    ffi::c_void,
    io,
    mem::size_of,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    ptr, thread,
    time::Instant,
};
use tokio::sync::mpsc;
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE},
    NetworkManagement::IpHelper::{
        Icmp6CreateFile, Icmp6SendEcho2, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho2Ex,
        ICMPV6_ECHO_REPLY_LH, ICMP_ECHO_REPLY, IP_DEST_HOST_UNREACHABLE, IP_DEST_NET_UNREACHABLE,
        IP_DEST_PORT_UNREACHABLE, IP_DEST_PROT_UNREACHABLE, IP_DEST_SCOPE_MISMATCH,
        IP_DEST_UNREACHABLE, IP_OPTION_INFORMATION, IP_PACKET_TOO_BIG, IP_SUCCESS,
        IP_TTL_EXPIRED_TRANSIT,
    },
};

/// Bytes of data in each echo request, as Windows' ping sends
const PAYLOAD: [u8; 32] = [0; 32];

/// Room for the reply: the reply structure, the data echoed back, and the
/// quoted packet of an error, with plenty to spare
const REPLY_WORDS: usize = 512;

/// The `Prober` for Windows without raw sockets
pub struct IcmpApi {
    /// The address probes leave from, as the routing table picks it
    source: Option<IpAddr>,
    next_seq: u16,
    tx: mpsc::UnboundedSender<Reply>,
    rx: Option<mpsc::UnboundedReceiver<Reply>>,
}

impl IcmpApi {
    pub fn new(target: IpAddr) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            source: source_address(target).ok(),
            next_seq: 1,
            tx,
            rx: Some(rx),
        }
    }
}

impl Prober for IcmpApi {
    fn send(
        &mut self,
        dst: IpAddr,
        ttl: u8,
        _flow: u16,
        _size: Option<u16>,
    ) -> io::Result<(u16, Instant)> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let (source, tx) = (self.source, self.tx.clone());
        let sent_at = Instant::now();
        thread::Builder::new()
            .name("icmp-echo".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let start = Instant::now();
                let answer = echo(source, dst, ttl);
                // The tracer measures from `sent_at`; the thread may have started late.
                let received_at = sent_at + start.elapsed();
                match answer {
                    Ok(Some((kind, from, reply_ttl))) => {
                        let _ = tx.send(Reply {
                            kind,
                            source: from,
                            seq,
                            received_at,
                            ttl: reply_ttl,
                            ext: Extensions::default(),
                            nat: None,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => tracing::debug!("Echo request to {dst} with TTL {ttl}: {e}"),
                }
            })?;
        Ok((seq, sent_at))
    }

    fn take_replies(&mut self) -> Option<mpsc::UnboundedReceiver<Reply>> {
        self.rx.take()
    }
}

/// An ICMP handle, closed when dropped
struct Handle(HANDLE);

impl Handle {
    fn open(ipv6: bool) -> io::Result<Self> {
        // SAFETY: no arguments; the handle is checked before use.
        let handle = unsafe {
            match ipv6 {
                false => IcmpCreateFile(),
                true => Icmp6CreateFile(),
            }
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a valid handle from IcmpCreateFile or Icmp6CreateFile, closed once.
        unsafe { IcmpCloseHandle(self.0) };
    }
}

/// Send one echo request with `ttl` and wait for what answers it: the kind of
/// answer, who sent it, and the TTL it arrived with (IPv4 only). None when
/// nothing answered in time or the answer is of no use.
fn echo(
    source: Option<IpAddr>,
    dst: IpAddr,
    ttl: u8,
) -> io::Result<Option<(IcmpResponseType, IpAddr, Option<u8>)>> {
    let handle = Handle::open(dst.is_ipv6())?;
    let options = IP_OPTION_INFORMATION {
        Ttl: ttl,
        Tos: 0,
        Flags: 0,
        OptionsSize: 0,
        OptionsData: ptr::null_mut(),
    };
    let timeout = u32::try_from(PROBE_TIMEOUT.as_millis()).unwrap_or(u32::MAX);
    // u64 words keep the reply structures aligned.
    let mut reply = [0u64; REPLY_WORDS];
    let reply_size = u32::try_from(size_of::<[u64; REPLY_WORDS]>()).unwrap_or(u32::MAX);
    let data = PAYLOAD.as_ptr().cast::<c_void>();
    let data_size = PAYLOAD.len() as u16;

    match dst {
        IpAddr::V4(v4) => {
            let from = match source {
                Some(IpAddr::V4(source)) => source,
                _ => Ipv4Addr::UNSPECIFIED,
            };
            // SAFETY: every pointer is to a live local of the size given; without
            // an event or APC routine the call returns once it is done with them.
            let replies = unsafe {
                IcmpSendEcho2Ex(
                    handle.0,
                    ptr::null_mut(),
                    None,
                    ptr::null(),
                    u32::from_ne_bytes(from.octets()),
                    u32::from_ne_bytes(v4.octets()),
                    data,
                    data_size,
                    &options,
                    reply.as_mut_ptr().cast(),
                    reply_size,
                    timeout,
                )
            };
            if replies == 0 {
                return no_answer();
            }
            // SAFETY: a call that returns a reply starts the buffer with it.
            let answer = unsafe { ptr::read_unaligned(reply.as_ptr().cast::<ICMP_ECHO_REPLY>()) };
            let from = IpAddr::from(Ipv4Addr::from(answer.Address.to_ne_bytes()));
            let reply_ttl = Some(answer.Options.Ttl).filter(|&ttl| ttl > 0);
            Ok(kind(answer.Status).map(|kind| (kind, from, reply_ttl)))
        }
        IpAddr::V6(v6) => {
            let from = match source {
                Some(IpAddr::V6(source)) => source,
                _ => Ipv6Addr::UNSPECIFIED,
            };
            let from = SockAddr::from(SocketAddr::new(from.into(), 0));
            let to = SockAddr::from(SocketAddr::new(v6.into(), 0));
            // SAFETY: as above; both addresses are sockaddr_in6, as socket2 builds them.
            let replies = unsafe {
                Icmp6SendEcho2(
                    handle.0,
                    ptr::null_mut(),
                    None,
                    ptr::null(),
                    from.as_ptr().cast(),
                    to.as_ptr().cast(),
                    data,
                    data_size,
                    &options,
                    reply.as_mut_ptr().cast(),
                    reply_size,
                    timeout,
                )
            };
            if replies == 0 {
                return no_answer();
            }
            // SAFETY: as above.
            let answer =
                unsafe { ptr::read_unaligned(reply.as_ptr().cast::<ICMPV6_ECHO_REPLY_LH>()) };
            // Eight 16-bit words, each in network byte order as stored. The
            // structure is packed: copy them out rather than borrow them.
            let words = answer.Address.sin6_addr;
            let octets: Vec<u8> = words.iter().flat_map(|word| word.to_ne_bytes()).collect();
            let octets: [u8; 16] = octets.try_into().expect("eight words are 16 bytes");
            let from = IpAddr::from(Ipv6Addr::from(octets));
            Ok(kind(answer.Status).map(|kind| (kind, from, None)))
        }
    }
}

/// A call that returned no reply: a timeout, or an error worth logging
fn no_answer() -> io::Result<Option<(IcmpResponseType, IpAddr, Option<u8>)>> {
    const IP_REQ_TIMED_OUT: i32 = 11010;
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(IP_REQ_TIMED_OUT) => Ok(None),
        _ => Err(e),
    }
}

/// What an answer's status says about the probe
fn kind(status: u32) -> Option<IcmpResponseType> {
    match status {
        IP_SUCCESS => Some(IcmpResponseType::EchoReply),
        IP_TTL_EXPIRED_TRANSIT => Some(IcmpResponseType::TimeExceeded),
        IP_DEST_PORT_UNREACHABLE => Some(IcmpResponseType::PortUnreachable),
        IP_DEST_NET_UNREACHABLE
        | IP_DEST_HOST_UNREACHABLE
        | IP_DEST_PROT_UNREACHABLE
        | IP_DEST_UNREACHABLE
        | IP_DEST_SCOPE_MISMATCH => Some(IcmpResponseType::DestinationUnreachable),
        IP_PACKET_TOO_BIG => Some(IcmpResponseType::TooBig { mtu: 0 }),
        other => {
            tracing::debug!("Echo request answered with status {other}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Echo requests to `addr` itself, the one test Windows CI can run: its
    /// runners reach no router by ICMP.
    fn echo_to(addr: IpAddr) {
        let mut prober = IcmpApi::new(addr);
        let mut replies = prober.take_replies().unwrap();
        let (seq, sent_at) = prober.send(addr, 64, 0, None).unwrap();
        drop(prober); // so that the channel closes if the probe's thread sends nothing
        let reply = replies.blocking_recv().expect("an answer");
        assert_eq!(reply.seq, seq);
        assert_eq!(reply.kind, IcmpResponseType::EchoReply);
        assert_eq!(reply.source, addr);
        assert!(reply.received_at - sent_at < Duration::from_secs(1));
    }

    #[test]
    fn loopback_answers_an_echo_request() {
        echo_to(IpAddr::from(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn ipv6_loopback_answers_an_echo_request() {
        echo_to(IpAddr::from(Ipv6Addr::LOCALHOST));
    }
}
