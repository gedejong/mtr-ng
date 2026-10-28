//! Probe engine for MTR-NG
//!
//! Sends ICMP echo requests with a chosen TTL and delivers the replies, each
//! timestamped by a dedicated receive thread the moment it arrives. Matching
//! replies to probes and deciding when a probe is lost happen in `tracer`.

use std::{
    io,
    mem::MaybeUninit,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::icmp_ext::{mpls_labels, MplsLabel};
use anyhow::{anyhow, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::sync::mpsc;

/// Maximum MTU size for network packets
const MAX_MTU: usize = 1500;

/// Starting sequence number for probe packets
const INITIAL_SEQUENCE: u16 = 32768;

/// Maximum sequence number before wrapping back to initial
const MAX_SEQUENCE: u16 = 60999;

/// How often blocked receive threads check whether the engine was dropped
const RECEIVE_POLL: Duration = Duration::from_millis(100);

/// Flows a trace can use. Flow `f` probes carry identifier `base + f` and a
/// checksum fixed per flow (see `echo_request`).
pub const MAX_FLOWS: u16 = 64;

/// Checksum of every probe in flow 0; flow `f` uses `FLOW_CHECKSUM + f`. Any
/// value works if the range avoids 0x0000 and 0xffff, the two one's-complement zeros.
const FLOW_CHECKSUM: u16 = 0x2c00;

/// Types of ICMP responses we care about
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IcmpResponseType {
    EchoReply,
    TimeExceeded,
    DestinationUnreachable,
    Timeout,
}

/// Information about a probe response
#[derive(Debug, Clone)]
pub struct ProbeResponse {
    pub hop: usize,
    /// Which probe to this hop it answers (see `HopStats::increment_sent`)
    pub ordinal: Option<u64>,
    pub seq: u16,
    pub source_addr: IpAddr,
    pub icmp_type: IcmpResponseType,
    pub rtt: Duration,
    pub send_time: Instant,
    pub receive_time: Instant, // High-precision receive timestamp
    pub precise_rtt_ns: u128,  // Nanosecond precision RTT
    /// TTL of the reply as it arrived (IPv4 only)
    pub reply_ttl: Option<u8>,
    /// MPLS labels the router quoted (RFC 4950)
    pub mpls: Vec<MplsLabel>,
}

/// A reply to one of our probes, timestamped when it was received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub kind: IcmpResponseType,
    pub source: IpAddr,
    pub seq: u16,
    pub received_at: Instant,
    /// TTL of the reply as it arrived (IPv4 only)
    pub ttl: Option<u8>,
    /// MPLS labels quoted in a Time Exceeded or Destination Unreachable
    pub mpls: Vec<MplsLabel>,
}

/// Sends probes and delivers replies. Implementations only move packets;
/// matching replies to probes and timeouts are the tracer's job.
pub trait Prober: Send {
    /// Send one probe with the given TTL (IPv4) or hop limit (IPv6) in `flow`
    /// (below `MAX_FLOWS`). Probes in one flow look alike to load balancers, so
    /// they follow one path. Returns its sequence number and the time it was sent.
    fn send(&mut self, dst: IpAddr, ttl: u8, flow: u16) -> io::Result<(u16, Instant)>;

    /// The stream of replies. Can be taken once.
    fn take_replies(&mut self) -> Option<mpsc::UnboundedReceiver<Reply>>;
}

/// Fields of an ICMP message that may answer one of our echo requests.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IcmpReply {
    kind: IcmpResponseType,
    source: IpAddr,
    id: u16,
    seq: u16,
    ttl: Option<u8>,
    mpls: Vec<MplsLabel>,
}

/// ICMP echo prober over raw or (on macOS) unprivileged datagram sockets
pub struct ProbeEngine {
    next_seq: u16,
    icmp_socket: Arc<Socket>,
    icmp6_socket: Option<Arc<Socket>>,
    packet_id: u16,
    unprivileged: bool,
    replies: Option<mpsc::UnboundedReceiver<Reply>>,
    stop: Arc<AtomicBool>,
}

impl ProbeEngine {
    pub fn new() -> Result<Self> {
        let (icmp_socket, unprivileged) = open_icmpv4_socket()?;
        tracing::info!(
            "Using {} ICMP socket",
            if unprivileged {
                "unprivileged datagram"
            } else {
                "raw"
            }
        );

        // Try to create IPv6 ICMP socket (optional)
        let icmp6_socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6)).ok();
        if icmp6_socket.is_none() {
            tracing::debug!("IPv6 ICMP socket creation failed - IPv6 support disabled");
        }

        let packet_id = std::process::id() as u16;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::unbounded_channel();

        let icmp_socket = Arc::new(icmp_socket);
        spawn_receiver(
            Arc::clone(&icmp_socket),
            false,
            packet_id,
            tx.clone(),
            &stop,
        )?;
        let icmp6_socket = match icmp6_socket {
            Some(socket) => {
                let socket = Arc::new(socket);
                spawn_receiver(Arc::clone(&socket), true, packet_id, tx, &stop)?;
                Some(socket)
            }
            None => None,
        };

        Ok(Self {
            // Random start so concurrent instances don't walk the same sequence numbers.
            next_seq: rand::random_range(INITIAL_SEQUENCE..=MAX_SEQUENCE),
            icmp_socket,
            icmp6_socket,
            packet_id,
            unprivileged,
            replies: Some(rx),
            stop,
        })
    }

    /// True when probing through an unprivileged datagram socket instead of a raw one.
    pub fn is_unprivileged(&self) -> bool {
        self.unprivileged
    }

    fn alloc_seq(&mut self) -> u16 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if self.next_seq > MAX_SEQUENCE {
            self.next_seq = INITIAL_SEQUENCE;
        }
        seq
    }
}

impl Prober for ProbeEngine {
    fn send(&mut self, dst: IpAddr, ttl: u8, flow: u16) -> io::Result<(u16, Instant)> {
        debug_assert!(flow < MAX_FLOWS);
        let seq = self.alloc_seq();
        let id = self.packet_id.wrapping_add(flow);
        let (socket, packet) = match dst {
            IpAddr::V4(_) => {
                self.icmp_socket.set_ttl_v4(ttl.into())?;
                let packet = echo_request(id, seq, FLOW_CHECKSUM + flow);
                (&self.icmp_socket, packet.to_vec())
            }
            IpAddr::V6(_) => {
                let Some(ref icmp6_sock) = self.icmp6_socket else {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "IPv6 not supported - no ICMPv6 socket available",
                    ));
                };
                icmp6_sock.set_unicast_hops_v6(ttl.into())?;
                (icmp6_sock, construct_icmp6_packet(seq, id))
            }
        };

        let sent_at = Instant::now();
        socket.send_to(&packet, &SocketAddr::new(dst, 0).into())?;
        tracing::debug!("Sent ICMP probe to {dst}: ttl={ttl}, flow={flow}, seq={seq}");
        Ok((seq, sent_at))
    }

    fn take_replies(&mut self) -> Option<mpsc::UnboundedReceiver<Reply>> {
        self.replies.take()
    }
}

impl Drop for ProbeEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Start a thread that blocks on `socket`, timestamps each packet on arrival and
/// forwards the ones that answer our probes.
fn spawn_receiver(
    socket: Arc<Socket>,
    ipv6: bool,
    packet_id: u16,
    tx: mpsc::UnboundedSender<Reply>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    socket.set_read_timeout(Some(RECEIVE_POLL))?;
    let stop = Arc::clone(stop);
    thread::Builder::new()
        .name(format!("mtr-ng-recv-v{}", if ipv6 { 6 } else { 4 }))
        .spawn(move || {
            let mut buf = [MaybeUninit::<u8>::uninit(); MAX_MTU];
            while !stop.load(Ordering::Relaxed) {
                match socket.recv_from(&mut buf) {
                    Ok((len, from)) => {
                        let received_at = Instant::now();
                        // SAFETY: recv_from initialised the first `len` bytes.
                        let bytes =
                            unsafe { &*(&buf[..len] as *const [MaybeUninit<u8>] as *const [u8]) };
                        let from = from.as_socket().map(|a| a.ip());
                        if let Some(reply) = decode(bytes, from, ipv6, packet_id, received_at) {
                            if tx.send(reply).is_err() {
                                break; // nobody listens any more
                            }
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock
                                | io::ErrorKind::TimedOut
                                | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => {
                        tracing::warn!("ICMP receive failed: {e}");
                        thread::sleep(RECEIVE_POLL);
                    }
                }
            }
        })?;
    Ok(())
}

/// Parse a received packet and keep it only if it answers one of our probes, whose
/// identifiers run from `packet_id` to `packet_id + MAX_FLOWS - 1`.
fn decode(
    bytes: &[u8],
    from: Option<IpAddr>,
    ipv6: bool,
    packet_id: u16,
    received_at: Instant,
) -> Option<Reply> {
    let reply = if ipv6 {
        parse_icmpv6(bytes, from?)?
    } else {
        parse_icmpv4(bytes)?
    };
    (reply.id.wrapping_sub(packet_id) < MAX_FLOWS).then_some(Reply {
        kind: reply.kind,
        source: reply.source,
        seq: reply.seq,
        received_at,
        ttl: reply.ttl,
        mpls: reply.mpls,
    })
}

/// Open the IPv4 ICMP socket. On macOS an unprivileged datagram socket works for
/// traceroute (it receives Time Exceeded with the IP header included), so prefer it
/// and fall back to a raw socket. Returns the socket and whether it is unprivileged.
fn open_icmpv4_socket() -> Result<(Socket, bool)> {
    if cfg!(target_os = "macos") {
        if let Ok(socket) = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::ICMPV4)) {
            return Ok((socket, true));
        }
    }
    Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4))
        .map(|socket| (socket, false))
        .map_err(|e| anyhow!("cannot open an ICMP socket: {e}. {}", privilege_hint(&e)))
}

fn privilege_hint(e: &io::Error) -> &'static str {
    if e.kind() != io::ErrorKind::PermissionDenied {
        return "";
    }
    if cfg!(target_os = "linux") {
        "Run with sudo, or grant the capability once: sudo setcap cap_net_raw+ep $(which mtr-ng)"
    } else if cfg!(windows) {
        "Run from an Administrator terminal"
    } else {
        "Run with sudo"
    }
}

/// Parse an IPv4 ICMP packet including its IP header (raw sockets, and datagram
/// sockets on macOS, deliver it). Errors only count if they quote an echo request.
fn parse_icmpv4(buf: &[u8]) -> Option<IcmpReply> {
    let ihl = usize::from(buf.first()? & 0x0f) * 4;
    if ihl < 20 || buf.len() < ihl + 8 {
        return None;
    }
    let source = Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]);
    let icmp = &buf[ihl..];
    let kind = match icmp[0] {
        0 => IcmpResponseType::EchoReply,
        11 => IcmpResponseType::TimeExceeded,
        3 => IcmpResponseType::DestinationUnreachable,
        _ => return None,
    };
    let echo = if kind == IcmpResponseType::EchoReply {
        icmp
    } else {
        // 8-byte ICMP header, then the quoted IP header and the first 8 bytes of our probe
        let inner = &icmp[8..];
        let inner_ihl = usize::from(inner.first()? & 0x0f) * 4;
        if inner_ihl < 20 || inner.len() < inner_ihl + 8 || inner[9] != 1 {
            return None;
        }
        let quoted = &inner[inner_ihl..];
        if quoted[0] != 8 {
            return None;
        }
        quoted
    };
    Some(IcmpReply {
        kind,
        source: IpAddr::V4(source),
        id: u16::from_be_bytes([echo[4], echo[5]]),
        seq: u16::from_be_bytes([echo[6], echo[7]]),
        ttl: Some(buf[8]),
        mpls: match kind {
            IcmpResponseType::EchoReply => Vec::new(),
            _ => mpls_labels(icmp),
        },
    })
}

/// Parse an ICMPv6 packet (raw ICMPv6 sockets deliver no IPv6 header).
fn parse_icmpv6(buf: &[u8], source: IpAddr) -> Option<IcmpReply> {
    if buf.len() < 8 {
        return None;
    }
    let kind = match buf[0] {
        129 => IcmpResponseType::EchoReply,
        3 => IcmpResponseType::TimeExceeded,
        1 => IcmpResponseType::DestinationUnreachable,
        _ => return None,
    };
    let echo = if kind == IcmpResponseType::EchoReply {
        buf
    } else {
        // 8-byte ICMPv6 header, 40-byte quoted IPv6 header (next header ICMPv6), then our probe
        let inner = buf.get(8..8 + 40 + 8)?;
        if inner[6] != 58 || inner[40] != 128 {
            return None;
        }
        &inner[40..]
    };
    Some(IcmpReply {
        kind,
        source,
        id: u16::from_be_bytes([echo[4], echo[5]]),
        seq: u16::from_be_bytes([echo[6], echo[7]]),
        ttl: None,
        mpls: Vec::new(),
    })
}

/// An ICMP echo request whose checksum is `checksum` whatever `seq` is: two
/// payload bytes absorb the change (Paris traceroute, as scamper does it for
/// ICMP). Load balancers that hash the ICMP header then see one flow per
/// identifier and checksum, and route every probe of it the same way.
fn echo_request(id: u16, seq: u16, checksum: u16) -> [u8; 10] {
    let mut packet = [0u8; 10];
    packet[0] = 8; // Echo Request, code 0
    packet[4..6].copy_from_slice(&id.to_be_bytes());
    packet[6..8].copy_from_slice(&seq.to_be_bytes());
    // With the checksum field zero the words must add up to !checksum, so the
    // payload is that minus what the header adds up to so far.
    let header_sum = !calculate_icmp_checksum(&packet);
    let payload = ones_complement_add(!checksum, !header_sum);
    packet[8..10].copy_from_slice(&payload.to_be_bytes());
    packet[2..4].copy_from_slice(&checksum.to_be_bytes());
    packet
}

fn ones_complement_add(a: u16, b: u16) -> u16 {
    let sum = u32::from(a) + u32::from(b);
    ((sum & 0xffff) + (sum >> 16)) as u16
}

pub(crate) fn calculate_icmp_checksum(packet: &[u8]) -> u16 {
    let mut sum = 0u32;

    // Sum all 16-bit words
    for chunk in packet.chunks(2) {
        if chunk.len() == 2 {
            sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
        } else {
            sum += (chunk[0] as u32) << 8;
        }
    }

    // Add carry
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }

    // One's complement
    !(sum as u16)
}

// Helper function to construct ICMPv6 packet
fn construct_icmp6_packet(seq: u16, id: u16) -> Vec<u8> {
    let mut packet = vec![0u8; 8];

    // ICMPv6 Type (128 = Echo Request)
    packet[0] = 128;
    // ICMPv6 Code (0)
    packet[1] = 0;
    // Checksum (0 initially, kernel will calculate for ICMPv6)
    packet[2] = 0;
    packet[3] = 0;
    // Identifier
    packet[4..6].copy_from_slice(&id.to_be_bytes());
    // Sequence Number
    packet[6..8].copy_from_slice(&seq.to_be_bytes());

    // Note: For ICMPv6, the kernel typically calculates the checksum
    // so we don't need to manually calculate it like we do for ICMP

    packet
}
#[cfg(test)]
mod tests {
    use super::*;

    const ROUTER: [u8; 4] = [10, 0, 0, 1];

    /// IPv4 header with the given header length (in bytes), protocol and source.
    fn ipv4_header(ihl: usize, proto: u8, src: [u8; 4]) -> Vec<u8> {
        let mut h = vec![0u8; ihl];
        h[0] = 0x40 | (ihl / 4) as u8;
        h[8] = 57; // TTL
        h[9] = proto;
        h[12..16].copy_from_slice(&src);
        h
    }

    fn echo(kind: u8, id: u16, seq: u16) -> Vec<u8> {
        let mut e = vec![kind, 0, 0, 0];
        e.extend_from_slice(&id.to_be_bytes());
        e.extend_from_slice(&seq.to_be_bytes());
        e
    }

    /// Time Exceeded from ROUTER quoting `inner` (quoted IP header + first 8 bytes).
    fn time_exceeded(inner: &[u8]) -> Vec<u8> {
        let mut p = ipv4_header(20, 1, ROUTER);
        p.extend_from_slice(&[11, 0, 0, 0, 0, 0, 0, 0]);
        p.extend_from_slice(inner);
        p
    }

    #[test]
    fn parses_echo_reply() {
        let mut p = ipv4_header(20, 1, [1, 1, 1, 1]);
        p.extend(echo(0, 0x1234, 40000));
        let r = parse_icmpv4(&p).unwrap();
        assert_eq!(r.kind, IcmpResponseType::EchoReply);
        assert_eq!(r.source, IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)));
        assert_eq!((r.id, r.seq, r.ttl), (0x1234, 40000, Some(57)));
        assert!(r.mpls.is_empty());
    }

    #[test]
    fn parses_time_exceeded_quoting_our_echo() {
        let mut inner = ipv4_header(20, 1, [192, 168, 1, 2]);
        inner.extend(echo(8, 0x1234, 40001));
        let r = parse_icmpv4(&time_exceeded(&inner)).unwrap();
        assert_eq!(r.kind, IcmpResponseType::TimeExceeded);
        assert_eq!(r.source, IpAddr::V4(Ipv4Addr::from(ROUTER)));
        assert_eq!((r.id, r.seq), (0x1234, 40001));
    }

    #[test]
    fn honours_quoted_header_with_ip_options() {
        let mut inner = ipv4_header(24, 1, [192, 168, 1, 2]);
        inner.extend(echo(8, 0x1234, 40002));
        let r = parse_icmpv4(&time_exceeded(&inner)).unwrap();
        assert_eq!((r.id, r.seq), (0x1234, 40002));
    }

    #[test]
    fn ignores_errors_about_other_protocols() {
        // Someone else's UDP traceroute: ports and checksum where id/seq would be.
        let mut inner = ipv4_header(20, 17, [192, 168, 1, 2]);
        inner.extend_from_slice(&[0x82, 0x9a, 0x82, 0x9b, 0, 8, 0x9c, 0x41]);
        assert_eq!(parse_icmpv4(&time_exceeded(&inner)), None);
    }

    #[test]
    fn rejects_truncated_packets() {
        let mut inner = ipv4_header(20, 1, [192, 168, 1, 2]);
        inner.extend(echo(8, 0x1234, 40003));
        let full = time_exceeded(&inner);
        for len in 0..full.len() {
            assert_eq!(parse_icmpv4(&full[..len]), None, "length {len}");
        }
    }

    #[test]
    fn time_exceeded_carries_mpls_labels() {
        let mut inner = ipv4_header(20, 1, [192, 168, 1, 2]);
        inner.extend(echo(8, 0x1234, 40005));
        inner.resize(128, 0);
        let mut p = time_exceeded(&inner);
        p[20 + 5] = 32; // RFC 4884 length: 128 bytes quoted
        let entry: u32 = 24005 << 12 | 1 << 8 | 1;
        p.extend_from_slice(&[0x20, 0, 0, 0, 0, 8, 1, 1]);
        p.extend_from_slice(&entry.to_be_bytes());
        let r = parse_icmpv4(&p).unwrap();
        assert_eq!(r.seq, 40005);
        let labels: Vec<_> = r.mpls.iter().map(|l| (l.label, l.bottom, l.ttl)).collect();
        assert_eq!(labels, [(24005, true, 1)]);
    }

    #[test]
    fn decode_keeps_only_our_identifiers() {
        let now = Instant::now();
        let reply_with_id = |id| {
            let mut p = ipv4_header(20, 1, [1, 1, 1, 1]);
            p.extend(echo(0, id, 40000));
            decode(&p, None, false, 0x1234, now)
        };
        assert_eq!(reply_with_id(0x9999), None, "another process's ping");
        assert_eq!(
            reply_with_id(0x1234 + MAX_FLOWS),
            None,
            "past our last flow"
        );
        assert!(
            reply_with_id(0x1234 + MAX_FLOWS - 1).is_some(),
            "our last flow"
        );
        let reply = reply_with_id(0x1234).unwrap();
        assert_eq!(reply.kind, IcmpResponseType::EchoReply);
        assert_eq!((reply.seq, reply.received_at), (40000, now));
    }

    #[test]
    fn a_flow_keeps_its_checksum_whatever_the_sequence_number() {
        for id in [0, 0x1234, 0xffff] {
            for flow in [0, 1, MAX_FLOWS - 1] {
                let checksum = FLOW_CHECKSUM + flow;
                for seq in 0..=u16::MAX {
                    let packet = echo_request(id, seq, checksum);
                    assert_eq!(packet[2..4], checksum.to_be_bytes());
                    assert_eq!(packet[6..8], seq.to_be_bytes());
                    assert_eq!(
                        calculate_icmp_checksum(&packet),
                        0,
                        "id {id:#x} flow {flow} seq {seq}: invalid checksum"
                    );
                }
            }
        }
    }

    #[test]
    fn parses_icmpv6_time_exceeded() {
        let mut p = vec![3, 0, 0, 0, 0, 0, 0, 0];
        let mut inner = vec![0u8; 40];
        inner[6] = 58;
        p.extend(inner);
        p.extend(echo(128, 0x1234, 40004));
        let src: IpAddr = "2001:db8::1".parse().unwrap();
        let r = parse_icmpv6(&p, src).unwrap();
        assert_eq!(r.kind, IcmpResponseType::TimeExceeded);
        assert_eq!((r.id, r.seq, r.source), (0x1234, 40004, src));
    }
}
