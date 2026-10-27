//! Simplified probe engine for MTR-NG
//!
//! This provides a cleaner interface for network probing without the complex
//! cross-platform error queue handling from the original probe_unix.c

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use rand::Rng;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::Interest;
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// Maximum MTU size for network packets
const MAX_MTU: usize = 1500;

/// Starting sequence number for probe packets
const INITIAL_SEQUENCE: u16 = 32768;

/// Maximum sequence number before wrapping back to initial
const MAX_SEQUENCE: u16 = 60999;

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
    pub seq: u16,
    pub source_addr: IpAddr,
    pub icmp_type: IcmpResponseType,
    pub rtt: Duration,
    pub send_time: Instant,
    pub receive_time: Instant, // High-precision receive timestamp
    pub precise_rtt_ns: u128,  // Nanosecond precision RTT
}

/// A probe that has been sent but not yet answered.
#[derive(Debug)]
struct ProbeInfo {
    hop: usize,
    sent_at: Instant,
    timeout: Duration,
}

impl ProbeInfo {
    fn timed_out(&self) -> bool {
        self.sent_at.elapsed() >= self.timeout
    }

    fn get_precise_rtt(&self, receive_time: Instant) -> (Duration, u128) {
        let rtt = receive_time.duration_since(self.sent_at);
        let rtt_ns = rtt.as_nanos();
        (rtt, rtt_ns)
    }
}

/// Fields of an ICMP message that may answer one of our echo requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IcmpReply {
    kind: IcmpResponseType,
    source: IpAddr,
    id: u16,
    seq: u16,
}

/// Simplified probe engine focused on core functionality
pub struct ProbeEngine {
    next_seq: u16,
    icmp_socket: Socket,
    icmp6_socket: Option<Socket>, // IPv6 ICMP socket
    pending: HashMap<u16, ProbeInfo>,
    packet_id: u16,
    unprivileged: bool,
}

impl ProbeEngine {
    pub fn new() -> Result<Self> {
        let (icmp_socket, unprivileged) = open_icmpv4_socket()?;
        icmp_socket.set_nonblocking(true)?;
        tracing::info!(
            "Using {} ICMP socket",
            if unprivileged {
                "unprivileged datagram"
            } else {
                "raw"
            }
        );

        // Try to create IPv6 ICMP socket (optional)
        let icmp6_socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))
            .inspect(|sock| {
                let _ = sock.set_nonblocking(true);
            })
            .ok();

        if icmp6_socket.is_some() {
            tracing::info!("IPv6 ICMP socket created successfully");
        } else {
            tracing::debug!("IPv6 ICMP socket creation failed - IPv6 support disabled");
        }

        Ok(Self {
            // Random start so concurrent instances don't walk the same sequence numbers.
            next_seq: rand::thread_rng().gen_range(INITIAL_SEQUENCE..=MAX_SEQUENCE),
            icmp_socket,
            icmp6_socket,
            pending: HashMap::new(),
            packet_id: std::process::id() as u16,
            unprivileged,
        })
    }

    /// True when probing through an unprivileged datagram socket instead of a raw one.
    pub fn is_unprivileged(&self) -> bool {
        self.unprivileged
    }

    /// Send an ICMP echo request with the given TTL (IPv4) or hop limit (IPv6).
    pub fn send_probe(
        &mut self,
        hop: usize,
        dst: SocketAddr,
        ttl: u8,
        timeout: Duration,
    ) -> Result<u16> {
        let seq = self.alloc_seq();

        // Select appropriate socket based on destination address family
        let (socket, packet) = match dst {
            SocketAddr::V4(_) => {
                self.icmp_socket.set_ttl(ttl.into())?;
                (
                    &self.icmp_socket,
                    construct_icmp_packet(seq, self.packet_id)?,
                )
            }
            SocketAddr::V6(_) => {
                let Some(ref icmp6_sock) = self.icmp6_socket else {
                    return Err(anyhow!("IPv6 not supported - no ICMPv6 socket available"));
                };
                icmp6_sock.set_unicast_hops_v6(ttl.into())?;
                (icmp6_sock, construct_icmp6_packet(seq, self.packet_id)?)
            }
        };

        socket.send_to(&packet, &dst.into())?;

        // Track the probe
        let now = Instant::now();
        let probe = ProbeInfo {
            hop,
            sent_at: now,
            timeout,
        };

        self.pending.insert(seq, probe);

        let addr_family = match dst {
            SocketAddr::V4(_) => "IPv4",
            SocketAddr::V6(_) => "IPv6",
        };

        tracing::debug!(
            "Sent ICMP probe ({}): hop={}, ttl={}, seq={}",
            addr_family,
            hop + 1,
            ttl,
            seq
        );

        Ok(seq)
    }

    /// Event-driven async response collection (no polling!)
    pub async fn collect_responses_async(&mut self) -> Result<Vec<ProbeResponse>> {
        let mut responses = Vec::new();
        let mut buffer = [0u8; MAX_MTU];

        // Use tokio's async socket operations for event-driven I/O
        // This waits for actual socket events instead of polling

        // Check IPv4 ICMP socket for readiness
        if let Ok(ready) = timeout(Duration::from_micros(1), async {
            // Convert to tokio socket for async operations
            let std_socket = std::net::UdpSocket::from(self.icmp_socket.try_clone()?);
            std_socket.set_nonblocking(true)?;
            let tokio_socket = UdpSocket::from_std(std_socket)?;

            // Wait for socket to become readable (event-driven!)
            tokio_socket.ready(Interest::READABLE).await
        })
        .await
        {
            if ready.is_ok() {
                // Socket is ready - collect all available responses
                loop {
                    let mut uninit_buffer = [std::mem::MaybeUninit::<u8>::uninit(); MAX_MTU];
                    match self.icmp_socket.recv_from(&mut uninit_buffer) {
                        Ok((len, _addr)) => {
                            let received_at = Instant::now();
                            // Convert MaybeUninit to initialized bytes
                            for i in 0..len {
                                buffer[i] = unsafe { uninit_buffer[i].assume_init() };
                            }
                            let reply = parse_icmpv4(&buffer[..len]);
                            if let Some(response) =
                                reply.and_then(|r| self.match_reply(r, received_at))
                            {
                                responses.push(response);
                            }
                        }
                        Err(_) => break, // No more data available
                    }
                }
            }
        }

        // Handle IPv6 socket if available
        if self.icmp6_socket.is_some() {
            // Similar async approach for IPv6
            if let Ok(ready) = timeout(Duration::from_micros(1), async {
                // IPv6 socket readiness check would go here
                // For now, fall back to non-blocking check
                Ok::<(), anyhow::Error>(())
            })
            .await
            {
                if ready.is_ok() {
                    loop {
                        let mut uninit_buffer = [std::mem::MaybeUninit::<u8>::uninit(); MAX_MTU];
                        let recv_result = if let Some(ref icmp6_socket) = self.icmp6_socket {
                            icmp6_socket.recv_from(&mut uninit_buffer)
                        } else {
                            break;
                        };

                        match recv_result {
                            Ok((len, addr)) => {
                                let received_at = Instant::now();
                                for i in 0..len {
                                    buffer[i] = unsafe { uninit_buffer[i].assume_init() };
                                }
                                let reply = addr
                                    .as_socket()
                                    .and_then(|from| parse_icmpv6(&buffer[..len], from.ip()));
                                if let Some(response) =
                                    reply.and_then(|r| self.match_reply(r, received_at))
                                {
                                    responses.push(response);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        }

        // Check for timeouts (no change needed here)
        let timed_out: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, probe)| probe.timed_out())
            .map(|(seq, probe)| (*seq, probe.hop, probe.sent_at))
            .collect();

        for (seq, hop, send_time) in timed_out {
            if let Some(probe) = self.pending.remove(&seq) {
                let (rtt, precise_rtt_ns) = probe.get_precise_rtt(Instant::now());
                responses.push(ProbeResponse {
                    hop,
                    seq,
                    source_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    icmp_type: IcmpResponseType::Timeout,
                    rtt,
                    send_time,
                    receive_time: Instant::now(),
                    precise_rtt_ns,
                });
            }
        }

        Ok(responses)
    }

    fn alloc_seq(&mut self) -> u16 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if self.next_seq > MAX_SEQUENCE {
            self.next_seq = INITIAL_SEQUENCE;
        }
        seq
    }

    /// Turn a parsed reply into a response if it answers one of our pending probes.
    fn match_reply(&mut self, reply: IcmpReply, received_at: Instant) -> Option<ProbeResponse> {
        if reply.id != self.packet_id {
            return None; // another process's ICMP traffic
        }
        let probe = self.pending.remove(&reply.seq)?;
        let (rtt, precise_rtt_ns) = probe.get_precise_rtt(received_at);
        Some(ProbeResponse {
            hop: probe.hop,
            seq: reply.seq,
            source_addr: reply.source,
            icmp_type: reply.kind,
            rtt,
            send_time: probe.sent_at,
            receive_time: received_at,
            precise_rtt_ns,
        })
    }
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
    })
}

// Helper function to construct ICMP packet
fn construct_icmp_packet(seq: u16, id: u16) -> Result<Vec<u8>> {
    let mut packet = vec![0u8; 8];

    // ICMP Type (8 = Echo Request)
    packet[0] = 8;
    // ICMP Code (0)
    packet[1] = 0;
    // Checksum (0 initially, calculated later)
    packet[2] = 0;
    packet[3] = 0;
    // Identifier
    packet[4..6].copy_from_slice(&id.to_be_bytes());
    // Sequence Number
    packet[6..8].copy_from_slice(&seq.to_be_bytes());

    // Calculate checksum
    let checksum = calculate_icmp_checksum(&packet);
    packet[2..4].copy_from_slice(&checksum.to_be_bytes());

    Ok(packet)
}

fn calculate_icmp_checksum(packet: &[u8]) -> u16 {
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
fn construct_icmp6_packet(seq: u16, id: u16) -> Result<Vec<u8>> {
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

    Ok(packet)
}
#[cfg(test)]
mod tests {
    use super::*;

    const ROUTER: [u8; 4] = [10, 0, 0, 1];

    /// IPv4 header with the given header length (in bytes), protocol and source.
    fn ipv4_header(ihl: usize, proto: u8, src: [u8; 4]) -> Vec<u8> {
        let mut h = vec![0u8; ihl];
        h[0] = 0x40 | (ihl / 4) as u8;
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
        assert_eq!((r.id, r.seq), (0x1234, 40000));
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

    fn engine_with_pending(seq: u16) -> ProbeEngine {
        let (icmp_socket, unprivileged) = (
            Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap(),
            false,
        );
        let mut engine = ProbeEngine {
            next_seq: INITIAL_SEQUENCE,
            icmp_socket,
            icmp6_socket: None,
            pending: HashMap::new(),
            packet_id: 0x1234,
            unprivileged,
        };
        engine.pending.insert(
            seq,
            ProbeInfo {
                hop: 4,
                sent_at: Instant::now(),
                timeout: Duration::from_secs(1),
            },
        );
        engine
    }

    #[test]
    fn matches_only_our_identifier() {
        let mut engine = engine_with_pending(40000);
        let mut reply = IcmpReply {
            kind: IcmpResponseType::EchoReply,
            source: IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            id: 0x9999,
            seq: 40000,
        };
        assert!(engine.match_reply(reply, Instant::now()).is_none());
        assert!(
            engine.pending.contains_key(&40000),
            "foreign reply must not consume the probe"
        );

        reply.id = 0x1234;
        let response = engine.match_reply(reply, Instant::now()).unwrap();
        assert_eq!(response.hop, 4);
        assert!(engine.pending.is_empty());
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
