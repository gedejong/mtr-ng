//! Probe engine for MTR-NG
//!
//! Sends ICMP or ICMPv6 echo requests, or UDP datagrams, with a chosen TTL or hop
//! limit and delivers the replies, each timestamped by a dedicated receive thread
//! the moment it arrives. Matching replies to probes and deciding when a probe is
//! lost happen in `tracer`.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::args::ProbeProtocol;
use crate::icmp_ext::{extensions, extensions_v6, Extensions, Interface, MplsLabel};

#[cfg(windows)]
mod icmp_api;
#[cfg(target_os = "linux")]
mod ping_socket;
#[cfg(unix)]
mod recvmsg;
use anyhow::{anyhow, Result};
#[cfg(windows)]
pub use icmp_api::IcmpApi;
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

const ECHO_REQUEST: u8 = 8;
const ECHO_REQUEST_V6: u8 = 128;

/// Destination port of UDP probes in flow 0; flow `f` uses `UDP_PORT + f`.
/// traceroute's base port, unlikely to have a listener.
pub const UDP_PORT: u16 = 33434;

/// UDP probe payload after the two bytes that set the checksum
const UDP_PAYLOAD: &[u8] = b"mtr-ng";

/// Types of ICMP responses we care about
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IcmpResponseType {
    EchoReply,
    TimeExceeded,
    /// The target's port is closed: a UDP probe arrived
    PortUnreachable,
    DestinationUnreachable,
    /// A router could not forward the probe without fragmenting it (Fragmentation
    /// Needed, Packet Too Big); its next link takes `mtu` bytes, 0 if it didn't say
    TooBig {
        mtu: u16,
    },
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
    /// TTL or hop limit of the reply as it arrived
    pub reply_ttl: Option<u8>,
    /// MPLS labels the router quoted (RFC 4950)
    pub mpls: Vec<MplsLabel>,
    /// Interfaces the router named (RFC 5837)
    pub interfaces: Vec<Interface>,
}

/// A reply to one of our probes, timestamped when it was received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub kind: IcmpResponseType,
    pub source: IpAddr,
    pub seq: u16,
    pub received_at: Instant,
    /// TTL or hop limit of the reply as it arrived
    pub ttl: Option<u8>,
    /// MPLS labels quoted in a Time Exceeded or Destination Unreachable
    pub mpls: Vec<MplsLabel>,
    /// Interfaces named in one (RFC 5837)
    pub interfaces: Vec<Interface>,
}

/// Sends probes and delivers replies. Implementations only move packets;
/// matching replies to probes and timeouts are the tracer's job.
pub trait Prober: Send {
    /// Send one probe with the given TTL (IPv4) or hop limit (IPv6) in `flow`
    /// (below `MAX_FLOWS`), padded to `size` bytes with the IP header if given.
    /// Probes in one flow look alike to load balancers, so they follow one path.
    /// Returns its sequence number and the time it was sent.
    fn send(
        &mut self,
        dst: IpAddr,
        ttl: u8,
        flow: u16,
        size: Option<u16>,
    ) -> io::Result<(u16, Instant)>;

    /// The stream of replies. Can be taken once.
    fn take_replies(&mut self) -> Option<mpsc::UnboundedReceiver<Reply>>;
}

/// Fields of an ICMP message that may answer one of our probes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IcmpReply {
    kind: IcmpResponseType,
    source: IpAddr,
    probe: Quoted,
    ttl: Option<u8>,
    mpls: Vec<MplsLabel>,
    interfaces: Vec<Interface>,
}

/// The probe an ICMP message answers, as far as it shows it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quoted {
    /// An echo request, quoted in an error or answered by an echo reply
    Echo { id: u16, seq: u16 },
    /// The UDP header of a datagram, quoted in an error
    Udp {
        src_port: u16,
        dst_port: u16,
        checksum: u16,
    },
}

/// Which quoted probes are ours, and their sequence numbers
#[derive(Debug, Clone, Copy)]
struct Ours {
    /// Identifier of flow 0's echo requests
    packet_id: u16,
    /// Source port of our UDP probes, when probing with UDP
    udp_port: Option<u16>,
}

impl Ours {
    fn seq(&self, probe: Quoted) -> Option<u16> {
        match (probe, self.udp_port) {
            (Quoted::Echo { id, seq }, None) => {
                (id.wrapping_sub(self.packet_id) < MAX_FLOWS).then_some(seq)
            }
            (
                Quoted::Udp {
                    src_port,
                    dst_port,
                    checksum,
                },
                Some(port),
            ) => (src_port == port && dst_port.wrapping_sub(UDP_PORT) < MAX_FLOWS)
                .then_some(checksum),
            _ => None,
        }
    }
}

/// What kind of ICMP socket the engine probes through
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketKind {
    /// Needs root or CAP_NET_RAW; sees every ICMP packet, IP header included
    Raw,
    /// macOS without privileges; behaves like a raw socket for traceroute
    Datagram,
    /// Linux without privileges: the kernel sets the identifier, and errors arrive
    /// on the error queue (see `ping_socket`)
    #[cfg(target_os = "linux")]
    Ping,
}

/// Prober for one address family. Sends ICMP echo requests or UDP datagrams and
/// reads the answers from an ICMP socket: raw, or without privileges an
/// unprivileged datagram socket (macOS) or a ping socket (Linux, ICMP only).
pub struct ProbeEngine {
    next_seq: u16,
    socket: Arc<Socket>,
    ipv6: bool,
    packet_id: u16,
    kind: SocketKind,
    udp: Option<UdpSender>,
    replies: Option<mpsc::UnboundedReceiver<Reply>>,
    stop: Arc<AtomicBool>,
}

/// The socket UDP probes leave from, bound to the address and port their
/// checksums are computed for
struct UdpSender {
    socket: Socket,
    source: IpAddr,
    port: u16,
}

impl ProbeEngine {
    /// Open the sockets for probing `target` with `protocol` and start receiving.
    pub fn new(target: IpAddr, protocol: ProbeProtocol) -> Result<Self> {
        let ipv6 = target.is_ipv6();
        let (socket, kind, kernel_ident) = open_icmp_socket(ipv6)?;
        tracing::info!(
            "Using a {kind:?} ICMP{} socket",
            if ipv6 { "v6" } else { "" }
        );
        if ipv6 {
            set_ipv6_options(&socket, kind);
        }
        let udp = match protocol {
            ProbeProtocol::Icmp => None,
            ProbeProtocol::Udp => Some(open_udp_sender(target, kind)?),
            ProbeProtocol::Tcp => return Err(anyhow!("TCP probes are not implemented yet")),
        };

        // A ping socket's probes all carry the identifier the kernel gave it.
        let packet_id = kernel_ident.unwrap_or(std::process::id() as u16);
        let ours = Ours {
            packet_id,
            udp_port: udp.as_ref().map(|udp| udp.port),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::unbounded_channel();
        let socket = Arc::new(socket);
        spawn_receiver(Arc::clone(&socket), kind, ipv6, ours, tx, &stop)?;

        Ok(Self {
            // Random start so concurrent instances don't walk the same sequence numbers.
            next_seq: rand::random_range(INITIAL_SEQUENCE..=MAX_SEQUENCE),
            socket,
            ipv6,
            packet_id,
            kind,
            udp,
            replies: Some(rx),
            stop,
        })
    }

    /// True when probing without privileges, through a datagram or ping socket.
    pub fn is_unprivileged(&self) -> bool {
        self.kind != SocketKind::Raw
    }

    /// Set Don't Fragment on every probe, so that a router whose next link is too
    /// narrow answers instead of fragmenting. Linux is told to ignore the path MTU
    /// it has cached, which would refuse larger probes before they leave.
    pub fn set_dont_fragment(&self) -> Result<()> {
        let sockets =
            std::iter::once(&*self.socket).chain(self.udp.as_ref().map(|udp| &udp.socket));
        for socket in sockets {
            if let Err(e) = dont_fragment(socket, self.ipv6) {
                if self.ipv6 && self.udp.is_none() {
                    // Routers never fragment IPv6, and probes are never larger than the
                    // interface's MTU, so only a path MTU cached here would interfere.
                    tracing::debug!("cannot set IPV6_DONTFRAG: {e}");
                } else {
                    return Err(anyhow!("cannot set Don't Fragment: {e}"));
                }
            }
        }
        Ok(())
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
    fn send(
        &mut self,
        dst: IpAddr,
        ttl: u8,
        flow: u16,
        size: Option<u16>,
    ) -> io::Result<(u16, Instant)> {
        debug_assert!(flow < MAX_FLOWS);
        debug_assert_eq!(dst.is_ipv6(), self.ipv6, "one engine probes one family");
        let seq = self.alloc_seq();
        // Bytes after the IP header and the ICMP or UDP header
        let ip_header = if self.ipv6 { 40 } else { 20 };
        let padded = |minimum: usize| {
            size.map_or(minimum, |size| {
                usize::from(size).saturating_sub(ip_header + 8)
            })
            .max(minimum)
        };
        if let Some(udp) = &self.udp {
            let port = UDP_PORT + flow;
            if self.ipv6 {
                udp.socket.set_unicast_hops_v6(ttl.into())?;
            } else {
                udp.socket.set_ttl_v4(ttl.into())?;
            }
            let len = padded(2 + UDP_PAYLOAD.len());
            let payload = udp_payload(udp.source, dst, udp.port, port, seq, len);
            let sent_at = Instant::now();
            udp.socket
                .send_to(&payload, &SocketAddr::new(dst, port).into())?;
            tracing::debug!("Sent UDP probe to {dst}:{port}: ttl={ttl}, flow={flow}, seq={seq}");
            return Ok((seq, sent_at));
        }
        // Flows differ in identifier and checksum, or on a ping socket, whose
        // identifier the kernel sets, in checksum alone.
        #[cfg(target_os = "linux")]
        let flow_id = if self.kind == SocketKind::Ping {
            0
        } else {
            flow
        };
        #[cfg(not(target_os = "linux"))]
        let flow_id = flow;
        let id = self.packet_id.wrapping_add(flow_id);
        let mut packet = if self.ipv6 {
            self.socket.set_unicast_hops_v6(ttl.into())?;
            echo_request(ECHO_REQUEST_V6, id, seq, FLOW_CHECKSUM + flow).to_vec()
        } else {
            self.socket.set_ttl_v4(ttl.into())?;
            echo_request(ECHO_REQUEST, id, seq, FLOW_CHECKSUM + flow).to_vec()
        };
        // Zeros add nothing to the checksum.
        packet.resize(8 + padded(packet.len() - 8), 0);

        let to = match dst {
            IpAddr::V4(_) => SocketAddr::new(dst, 0),
            IpAddr::V6(v6) => labelled(v6, flow_label(self.packet_id, flow)).into(),
        };
        let sent_at = Instant::now();
        self.socket.send_to(&packet, &to.into())?;
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
    kind: SocketKind,
    ipv6: bool,
    ours: Ours,
    tx: mpsc::UnboundedSender<Reply>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    socket.set_read_timeout(Some(RECEIVE_POLL))?;
    let stop = Arc::clone(stop);
    thread::Builder::new()
        .name(format!("mtr-ng-recv-v{}", if ipv6 { 6 } else { 4 }))
        .spawn(move || {
            #[cfg(target_os = "linux")]
            if kind == SocketKind::Ping {
                ping_socket::receive(&socket, ipv6, RECEIVE_POLL, &stop, |reply, at| {
                    match ours.reply(reply, at) {
                        Some(reply) => tx.send(reply).is_ok(),
                        None => true,
                    }
                });
                return;
            }
            #[cfg(not(target_os = "linux"))]
            let _ = kind; // every other kind reads whole packets
            let mut buf = [0u8; MAX_MTU];
            let mut control = [0u64; 16];
            while !stop.load(Ordering::Relaxed) {
                match read_packet(&socket, &mut buf, &mut control) {
                    Ok(Some(packet)) => {
                        let received_at = Instant::now();
                        if let Some(reply) = decode(packet, ipv6, ours, received_at) {
                            if tx.send(reply).is_err() {
                                break; // nobody listens any more
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("ICMP receive failed: {e}");
                        thread::sleep(RECEIVE_POLL);
                    }
                }
            }
        })?;
    Ok(())
}

/// A packet from the ICMP socket
struct Packet<'a> {
    bytes: &'a [u8],
    from: Option<IpAddr>,
    /// The hop limit an IPv6 packet arrived with; IPv4 has its TTL in the header
    hop_limit: Option<u8>,
}

/// The next packet on `socket`, or None if none came within its read timeout.
#[cfg(unix)]
fn read_packet<'a>(
    socket: &Socket,
    buf: &'a mut [u8],
    control: &mut [u64],
) -> io::Result<Option<Packet<'a>>> {
    use std::os::fd::AsRawFd;
    let message = recvmsg::read(socket.as_raw_fd(), buf, control, 0)?;
    Ok(message.map(|message| Packet {
        bytes: message.data,
        from: message.from,
        hop_limit: message.ttl,
    }))
}

/// The next packet on `socket`, or None if none came within its read timeout.
#[cfg(not(unix))]
fn read_packet<'a>(
    socket: &Socket,
    buf: &'a mut [u8],
    _control: &mut [u64],
) -> io::Result<Option<Packet<'a>>> {
    use std::mem::MaybeUninit;
    // SAFETY: MaybeUninit<u8> has the layout of u8, and recv_from only writes to it.
    let uninit = unsafe { &mut *(std::ptr::from_mut(buf) as *mut [MaybeUninit<u8>]) };
    match socket.recv_from(uninit) {
        Ok((len, from)) => Ok(Some(Packet {
            bytes: &buf[..len],
            from: from.as_socket().map(|a| a.ip()),
            hop_limit: None,
        })),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Parse a received packet and keep it only if it answers one of our probes.
fn decode(packet: Packet, ipv6: bool, ours: Ours, received_at: Instant) -> Option<Reply> {
    let reply = if ipv6 {
        parse_icmpv6(packet.bytes, packet.from?, packet.hop_limit)?
    } else {
        parse_icmpv4(packet.bytes)?
    };
    ours.reply(reply, received_at)
}

impl Ours {
    /// `reply` as a `Reply` if it answers one of our probes.
    fn reply(&self, reply: IcmpReply, received_at: Instant) -> Option<Reply> {
        Some(Reply {
            kind: reply.kind,
            source: reply.source,
            seq: self.seq(reply.probe)?,
            received_at,
            ttl: reply.ttl,
            mpls: reply.mpls,
            interfaces: reply.interfaces,
        })
    }
}

/// Raw ICMP and ICMPv6 sockets opened at startup, before privileges were given up
static RAW_SOCKETS: Mutex<[Option<Socket>; 2]> = Mutex::new([None, None]);

/// Open a raw socket for each family while this process may (as root, with
/// CAP_NET_RAW or setuid root), so that it can give those privileges up before
/// doing anything else (see `privileges`). The engine takes them later. Nothing
/// on macOS and Windows, which probe without raw sockets.
pub fn open_raw_sockets() {
    if cfg!(any(target_os = "macos", windows)) {
        return;
    }
    let families = [
        (Domain::IPV4, Protocol::ICMPV4),
        (Domain::IPV6, Protocol::ICMPV6),
    ];
    let mut sockets = RAW_SOCKETS.lock().unwrap_or_else(|e| e.into_inner());
    for (slot, (domain, protocol)) in sockets.iter_mut().zip(families) {
        *slot = Socket::new(domain, Type::RAW, Some(protocol)).ok();
    }
}

/// Open the ICMP or ICMPv6 socket, and say what kind it is and, for a Linux ping
/// socket, which identifier the kernel gave it.
///
/// On macOS an unprivileged datagram socket works for traceroute (it receives
/// Time Exceeded too, for IPv4 with the IP header included), so it comes first.
/// On Linux a raw socket comes first because it keeps the MPLS labels routers
/// quote, and a ping socket is the fallback without privileges.
fn open_icmp_socket(ipv6: bool) -> Result<(Socket, SocketKind, Option<u16>)> {
    let (domain, protocol, name) = match ipv6 {
        false => (Domain::IPV4, Protocol::ICMPV4, "ICMP"),
        true => (Domain::IPV6, Protocol::ICMPV6, "ICMPv6"),
    };
    if cfg!(target_os = "macos") {
        if let Ok(socket) = Socket::new(domain, Type::DGRAM, Some(protocol)) {
            return Ok((socket, SocketKind::Datagram, None));
        }
    }
    let opened = RAW_SOCKETS.lock().unwrap_or_else(|e| e.into_inner())[usize::from(ipv6)].take();
    if let Some(socket) = opened {
        return Ok((socket, SocketKind::Raw, None));
    }
    let raw_error = match Socket::new(domain, Type::RAW, Some(protocol)) {
        Ok(socket) => return Ok((socket, SocketKind::Raw, None)),
        Err(e) => e,
    };
    #[cfg(target_os = "linux")]
    if raw_error.kind() == io::ErrorKind::PermissionDenied {
        match ping_socket::open(ipv6) {
            Ok((socket, ident)) => return Ok((socket, SocketKind::Ping, Some(ident))),
            Err(e) => tracing::debug!("no ping socket either: {e}"),
        }
    }
    Err(anyhow!(
        "cannot open an {name} socket: {raw_error}. {}",
        privilege_hint(&raw_error)
    ))
}

/// Whether this machine has a route to `addr`.
pub fn routable(addr: IpAddr) -> bool {
    source_address(addr).is_ok()
}

/// Set Don't Fragment on `socket`.
#[cfg(target_os = "linux")]
fn dont_fragment(socket: &Socket, ipv6: bool) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let (level, name, value) = match ipv6 {
        false => (
            libc::IPPROTO_IP,
            libc::IP_MTU_DISCOVER,
            libc::IP_PMTUDISC_PROBE,
        ),
        true => (
            libc::IPPROTO_IPV6,
            libc::IPV6_MTU_DISCOVER,
            libc::IPV6_PMTUDISC_PROBE,
        ),
    };
    recvmsg::set_int_option(socket.as_raw_fd(), level, name, value)
}

/// Set Don't Fragment on `socket`.
#[cfg(all(unix, not(target_os = "linux")))]
fn dont_fragment(socket: &Socket, ipv6: bool) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let (level, name) = match ipv6 {
        false => (libc::IPPROTO_IP, libc::IP_DONTFRAG),
        true => (libc::IPPROTO_IPV6, libc::IPV6_DONTFRAG),
    };
    recvmsg::set_option(socket.as_raw_fd(), level, name)
}

/// Set Don't Fragment on `socket`.
#[cfg(not(unix))]
fn dont_fragment(_socket: &Socket, _ipv6: bool) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// Whether sending failed because the packet is larger than this machine will
/// send to the target.
pub fn is_too_big(e: &io::Error) -> bool {
    #[cfg(unix)]
    return e.raw_os_error() == Some(libc::EMSGSIZE);
    #[cfg(not(unix))]
    return e.raw_os_error() == Some(10040); // WSAEMSGSIZE
}

/// MTU of the interface packets to `target` leave from.
pub fn interface_mtu(target: IpAddr) -> Option<u16> {
    let source = source_address(target).ok()?;
    #[cfg(unix)]
    return recvmsg::interface_mtu(source);
    #[cfg(not(unix))]
    {
        let _ = source;
        None
    }
}

/// Whether `addr` is this machine: packets to it never leave.
pub fn is_local(addr: IpAddr) -> bool {
    addr.is_loopback() || source_address(addr).is_ok_and(|source| source == addr)
}

/// The address packets to `addr` leave from. Connecting a UDP socket consults the
/// routing table without sending anything.
fn source_address(addr: IpAddr) -> io::Result<IpAddr> {
    let any = match addr {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    let socket = UdpSocket::bind((any, 0))?;
    socket.connect((addr, UDP_PORT))?;
    Ok(socket.local_addr()?.ip())
}

/// Open the socket UDP probes to `target` leave from. Their answers come to the
/// ICMP socket, which must see errors about other sockets' packets: a ping
/// socket only sees its own pings.
fn open_udp_sender(target: IpAddr, kind: SocketKind) -> Result<UdpSender> {
    #[cfg(target_os = "linux")]
    if kind == SocketKind::Ping {
        return Err(anyhow!(
            "UDP probes need a raw ICMP socket on Linux to read the routers' answers. \
             Run with sudo, or grant the capability once: sudo setcap cap_net_raw+ep $(which mtr-ng)"
        ));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = kind;
    let source = source_address(target)?;
    let domain = if target.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.bind(&SocketAddr::new(source, 0).into())?;
    let port = socket
        .local_addr()?
        .as_socket()
        .map_or(0, |addr| addr.port());
    Ok(UdpSender {
        socket,
        source,
        port,
    })
}

/// Ask the kernel for each reply's hop limit, which an ICMPv6 socket doesn't show
/// in the packet, and on Linux to send the flow label in each destination address
/// (it ignores it otherwise; elsewhere the label is always sent). Routers balance
/// IPv6 on the flow label, so each flow sets its own. A raw or datagram ICMPv6
/// socket also gets a filter for the types a trace reads; a ping socket only
/// sees its own echo replies anyway.
fn set_ipv6_options(socket: &Socket, kind: SocketKind) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = socket.as_raw_fd();
        if matches!(kind, SocketKind::Raw | SocketKind::Datagram) {
            if let Err(e) = recvmsg::filter_icmpv6(fd) {
                tracing::debug!("cannot filter ICMPv6 types: {e}");
            }
        }
        if let Err(e) = recvmsg::set_option(fd, libc::IPPROTO_IPV6, libc::IPV6_RECVHOPLIMIT) {
            tracing::debug!("cannot read IPv6 hop limits: {e}");
        }
        #[cfg(target_os = "linux")]
        if let Err(e) = recvmsg::set_option(fd, libc::IPPROTO_IPV6, libc::IPV6_FLOWINFO_SEND) {
            tracing::debug!("cannot set IPv6 flow labels: {e}");
        }
    }
    #[cfg(not(unix))]
    let _ = (socket, kind);
}

/// `addr` with flow label `label`. sin6_flowinfo is in network byte order, and
/// std passes the value on as it is.
fn labelled(addr: Ipv6Addr, label: u32) -> SocketAddrV6 {
    SocketAddrV6::new(addr, 0, label.to_be(), 0)
}

/// The IPv6 flow label of `flow`: fixed per flow, different between flows, never
/// zero (no label), and in the range Linux keeps for stateless labels (RFC 6437),
/// which needs no registration.
fn flow_label(packet_id: u16, flow: u16) -> u32 {
    0x80000 | (u32::from(packet_id) << 6 | u32::from(flow)) & 0x7ffff
}

fn privilege_hint(e: &io::Error) -> &'static str {
    if e.kind() != io::ErrorKind::PermissionDenied {
        return "";
    }
    if cfg!(target_os = "linux") {
        "Run with sudo, grant the capability once (sudo setcap cap_net_raw+ep $(which mtr-ng)), \
         or let your group send pings: sudo sysctl -w net.ipv4.ping_group_range=\"0 2147483647\""
    } else if cfg!(windows) {
        "Run from an Administrator terminal"
    } else {
        "Run with sudo"
    }
}

/// Parse an IPv4 ICMP packet including its IP header (raw sockets, and datagram
/// sockets on macOS, deliver it). Errors only count if they quote an echo request
/// or a UDP datagram.
fn parse_icmpv4(buf: &[u8]) -> Option<IcmpReply> {
    let ihl = usize::from(buf.first()? & 0x0f) * 4;
    if ihl < 20 || buf.len() < ihl + 8 {
        return None;
    }
    let source = Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]);
    let icmp = &buf[ihl..];
    let kind = match (icmp[0], icmp[1]) {
        (0, _) => IcmpResponseType::EchoReply,
        (11, _) => IcmpResponseType::TimeExceeded,
        (3, 3) => IcmpResponseType::PortUnreachable,
        // RFC 1191: the next link's MTU in the second half of the unused word
        (3, 4) => IcmpResponseType::TooBig {
            mtu: u16::from_be_bytes([icmp[6], icmp[7]]),
        },
        (3, _) => IcmpResponseType::DestinationUnreachable,
        _ => return None,
    };
    let probe = if kind == IcmpResponseType::EchoReply {
        echo_fields(icmp)
    } else {
        // 8-byte ICMP header, then the quoted IP header and the first 8 bytes of our probe
        let inner = &icmp[8..];
        let inner_ihl = usize::from(inner.first()? & 0x0f) * 4;
        if inner_ihl < 20 || inner.len() < inner_ihl + 8 {
            return None;
        }
        quoted_probe(inner[9], &inner[inner_ihl..], ECHO_REQUEST)?
    };
    let ext = match kind {
        IcmpResponseType::EchoReply => Extensions::default(),
        _ => extensions(icmp),
    };
    Some(IcmpReply {
        kind,
        source: IpAddr::V4(source),
        probe,
        ttl: Some(buf[8]),
        mpls: ext.mpls,
        interfaces: ext.interfaces,
    })
}

/// Parse an ICMPv6 packet that came from `source` with `hop_limit` (ICMPv6
/// sockets deliver no IPv6 header). Errors only count if they quote an echo
/// request or a UDP datagram.
fn parse_icmpv6(buf: &[u8], source: IpAddr, hop_limit: Option<u8>) -> Option<IcmpReply> {
    if buf.len() < 8 {
        return None;
    }
    let kind = match (buf[0], buf[1]) {
        (129, _) => IcmpResponseType::EchoReply,
        (3, _) => IcmpResponseType::TimeExceeded,
        (1, 4) => IcmpResponseType::PortUnreachable,
        (1, _) => IcmpResponseType::DestinationUnreachable,
        (2, _) => IcmpResponseType::TooBig {
            mtu: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]])
                .try_into()
                .unwrap_or(u16::MAX),
        },
        _ => return None,
    };
    let probe = if kind == IcmpResponseType::EchoReply {
        echo_fields(buf)
    } else {
        // 8-byte ICMPv6 header, then the 40-byte quoted IPv6 header and our probe
        let inner = buf.get(8..8 + 40 + 8)?;
        quoted_probe(inner[6], &inner[40..], ECHO_REQUEST_V6)?
    };
    let ext = match kind {
        IcmpResponseType::EchoReply => Extensions::default(),
        _ => extensions_v6(buf),
    };
    Some(IcmpReply {
        kind,
        source,
        probe,
        ttl: hop_limit,
        mpls: ext.mpls,
        interfaces: ext.interfaces,
    })
}

/// Identifier and sequence number of an echo request or reply
fn echo_fields(echo: &[u8]) -> Quoted {
    Quoted::Echo {
        id: u16::from_be_bytes([echo[4], echo[5]]),
        seq: u16::from_be_bytes([echo[6], echo[7]]),
    }
}

/// The probe in the first 8 quoted bytes of a datagram of IP protocol `protocol`:
/// an echo request (`echo_request` is its ICMP type) or a UDP header.
fn quoted_probe(protocol: u8, quoted: &[u8], echo_request: u8) -> Option<Quoted> {
    let word = |at: usize| u16::from_be_bytes([quoted[at], quoted[at + 1]]);
    match protocol {
        1 | 58 if quoted[0] == echo_request => Some(echo_fields(quoted)),
        17 => Some(Quoted::Udp {
            src_port: word(0),
            dst_port: word(2),
            checksum: word(6),
        }),
        _ => None,
    }
}

/// The `len`-byte payload of a UDP probe whose checksum, as the kernel will compute
/// it, is `seq`: two payload bytes absorb the rest (Paris traceroute's UDP).
/// Addresses, ports and length then stay fixed within a flow, so load balancers
/// route every probe of it alike, and routers that quote only the UDP header still
/// say which probe they answer.
fn udp_payload(
    src: IpAddr,
    dst: IpAddr,
    src_port: u16,
    dst_port: u16,
    seq: u16,
    len: usize,
) -> Vec<u8> {
    let mut payload = vec![0, 0];
    payload.extend_from_slice(UDP_PAYLOAD);
    payload.resize(len.max(payload.len()), 0);
    let datagram = udp_datagram(src_port, dst_port, 0, &payload);
    let sum =
        !calculate_icmp_checksum(&[pseudo_header(src, dst, datagram.len()), datagram].concat());
    // The checksum is the complement of the sum, so the sum must come to !seq.
    let adjust = ones_complement_add(!seq, !sum);
    payload[..2].copy_from_slice(&adjust.to_be_bytes());
    payload
}

/// A UDP header with `checksum`, followed by `payload`
fn udp_datagram(src_port: u16, dst_port: u16, checksum: u16, payload: &[u8]) -> Vec<u8> {
    let len = (8 + payload.len()) as u16;
    [src_port, dst_port, len, checksum]
        .iter()
        .flat_map(|word| word.to_be_bytes())
        .chain(payload.iter().copied())
        .collect()
}

/// The pseudo-header UDP's checksum covers besides the datagram (RFC 768, RFC 8200)
fn pseudo_header(src: IpAddr, dst: IpAddr, len: usize) -> Vec<u8> {
    match (src, dst) {
        (IpAddr::V4(src), IpAddr::V4(dst)) => [
            &src.octets()[..],
            &dst.octets(),
            &[0, 17],
            &(len as u16).to_be_bytes(),
        ]
        .concat(),
        _ => {
            let v6 = |addr: IpAddr| match addr {
                IpAddr::V6(v6) => v6.octets(),
                IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
            };
            [
                &v6(src)[..],
                &v6(dst),
                &(len as u32).to_be_bytes(),
                &[0, 0, 0, 17],
            ]
            .concat()
        }
    }
}

/// An echo request of type `kind` whose checksum is `checksum` whatever `seq` is:
/// two payload bytes absorb the change (Paris traceroute, as scamper does it for
/// ICMP). Load balancers that hash the ICMP header then see one flow per
/// identifier and checksum, and route every probe of it the same way.
///
/// For ICMPv6 the kernel replaces the checksum with one that also covers both
/// addresses. Those don't change during a trace, and the rest of the message adds
/// up to the same for every probe of a flow, so the kernel's checksum is fixed per
/// flow as well.
fn echo_request(kind: u8, id: u16, seq: u16, checksum: u16) -> [u8; 10] {
    let mut packet = [0u8; 10];
    packet[0] = kind; // code 0
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

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTER: [u8; 4] = [10, 0, 0, 1];

    fn echo_of(id: u16, seq: u16) -> Quoted {
        Quoted::Echo { id, seq }
    }

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
        assert_eq!((r.probe, r.ttl), (echo_of(0x1234, 40000), Some(57)));
        assert!(r.mpls.is_empty());
    }

    #[test]
    fn parses_time_exceeded_quoting_our_echo() {
        let mut inner = ipv4_header(20, 1, [192, 168, 1, 2]);
        inner.extend(echo(8, 0x1234, 40001));
        let r = parse_icmpv4(&time_exceeded(&inner)).unwrap();
        assert_eq!(r.kind, IcmpResponseType::TimeExceeded);
        assert_eq!(r.source, IpAddr::V4(Ipv4Addr::from(ROUTER)));
        assert_eq!(r.probe, echo_of(0x1234, 40001));
    }

    #[test]
    fn honours_quoted_header_with_ip_options() {
        let mut inner = ipv4_header(24, 1, [192, 168, 1, 2]);
        inner.extend(echo(8, 0x1234, 40002));
        let r = parse_icmpv4(&time_exceeded(&inner)).unwrap();
        assert_eq!(r.probe, echo_of(0x1234, 40002));
    }

    #[test]
    fn ignores_errors_about_other_protocols() {
        // A TCP SYN: ports and sequence number where id/seq would be.
        let mut inner = ipv4_header(20, 6, [192, 168, 1, 2]);
        inner.extend_from_slice(&[0x82, 0x9a, 0x00, 0x50, 0, 0, 0x9c, 0x41]);
        assert_eq!(parse_icmpv4(&time_exceeded(&inner)), None);
    }

    #[test]
    fn reads_the_udp_header_an_error_quotes() {
        let mut inner = ipv4_header(20, 17, [192, 168, 1, 2]);
        inner.extend_from_slice(&[0xec, 0x6f, 0x82, 0x9a, 0, 16, 0x9c, 0x41]);
        let r = parse_icmpv4(&time_exceeded(&inner)).unwrap();
        assert_eq!(r.kind, IcmpResponseType::TimeExceeded);
        let udp = Quoted::Udp {
            src_port: 0xec6f,
            dst_port: UDP_PORT,
            checksum: 0x9c41,
        };
        assert_eq!(r.probe, udp);

        let mut unreachable = time_exceeded(&inner);
        unreachable[20..22].copy_from_slice(&[3, 3]);
        let r = parse_icmpv4(&unreachable).unwrap();
        assert_eq!(r.kind, IcmpResponseType::PortUnreachable, "the target");
        unreachable[21] = 13;
        let r = parse_icmpv4(&unreachable).unwrap();
        assert_eq!(r.kind, IcmpResponseType::DestinationUnreachable, "filtered");
    }

    #[test]
    fn udp_probes_are_ours_by_source_port_and_flow_ports() {
        let udp = |src_port, dst_port| Quoted::Udp {
            src_port,
            dst_port,
            checksum: 40000,
        };
        let ours = Ours {
            packet_id: 0x1234,
            udp_port: Some(0xec6f),
        };
        assert_eq!(ours.seq(udp(0xec6f, UDP_PORT)), Some(40000));
        assert_eq!(ours.seq(udp(0xec6f, UDP_PORT + MAX_FLOWS - 1)), Some(40000));
        assert_eq!(
            ours.seq(udp(0xec6f, UDP_PORT + MAX_FLOWS)),
            None,
            "no such flow"
        );
        assert_eq!(ours.seq(udp(0xec70, UDP_PORT)), None, "another socket");
        assert_eq!(
            ours.seq(echo_of(0x1234, 40000)),
            None,
            "not probing with ICMP"
        );
        let icmp = Ours {
            udp_port: None,
            ..ours
        };
        assert_eq!(
            icmp.seq(udp(0xec6f, UDP_PORT)),
            None,
            "not probing with UDP"
        );
        assert_eq!(icmp.seq(echo_of(0x1234, 40000)), Some(40000));
    }

    #[test]
    fn a_udp_probe_checksums_to_its_sequence_number() {
        let pairs: [(IpAddr, IpAddr); 2] = [
            ([192, 168, 1, 2].into(), [1, 1, 1, 1].into()),
            (
                "2001:db8::2".parse().unwrap(),
                "2606:4700:4700::1111".parse().unwrap(),
            ),
        ];
        for (src, dst) in pairs {
            for (flow, len) in [(0, 0), (MAX_FLOWS - 1, 0), (1, 1001), (2, 1452)] {
                for seq in [INITIAL_SEQUENCE, 40000, MAX_SEQUENCE] {
                    let payload = udp_payload(src, dst, 0xec6f, UDP_PORT + flow, seq, len);
                    assert_eq!(payload[2..8], *UDP_PAYLOAD);
                    assert_eq!(payload.len(), len.max(8), "same length every time");
                    // As sent: the checksum field holds seq, and the whole adds up.
                    let datagram = udp_datagram(0xec6f, UDP_PORT + flow, seq, &payload);
                    let pseudo = pseudo_header(src, dst, datagram.len());
                    assert_eq!(
                        calculate_icmp_checksum(&[pseudo, datagram].concat()),
                        0,
                        "{dst} flow {flow} seq {seq}"
                    );
                }
            }
        }
    }

    #[test]
    fn each_flow_has_its_own_stateless_flow_label() {
        let labels: Vec<u32> = (0..MAX_FLOWS).map(|f| flow_label(0xffff, f)).collect();
        assert!(labels.iter().all(|&l| (0x80000..=0xfffff).contains(&l)));
        let mut unique = labels.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), labels.len());
        assert_eq!(flow_label(0x1234, 3), flow_label(0x1234, 3));
    }

    #[test]
    fn the_flow_label_reaches_the_kernel_in_network_byte_order() {
        let addr = socket2::SockAddr::from(labelled(Ipv6Addr::LOCALHOST, 0x812c3));
        // SAFETY: as_ptr points at len() bytes of a sockaddr_in6.
        let bytes =
            unsafe { std::slice::from_raw_parts(addr.as_ptr().cast::<u8>(), addr.len() as usize) };
        // sin6_flowinfo follows family and port on Linux, and length, family and
        // port on the BSDs: bytes 4 to 8 either way.
        assert_eq!(bytes[4..8], [0x00, 0x08, 0x12, 0xc3]);
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
        assert_eq!(r.probe, echo_of(0x1234, 40005));
        let labels: Vec<_> = r.mpls.iter().map(|l| (l.label, l.bottom, l.ttl)).collect();
        assert_eq!(labels, [(24005, true, 1)]);
    }

    #[test]
    fn decode_keeps_only_our_identifiers() {
        let now = Instant::now();
        let reply_with_id = |id| {
            let mut p = ipv4_header(20, 1, [1, 1, 1, 1]);
            p.extend(echo(0, id, 40000));
            let ours = Ours {
                packet_id: 0x1234,
                udp_port: None,
            };
            let packet = Packet {
                bytes: &p,
                from: None,
                hop_limit: None,
            };
            decode(packet, false, ours, now)
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
                    let packet = echo_request(ECHO_REQUEST, id, seq, checksum);
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
    fn an_icmpv6_flow_adds_up_alike_whatever_the_sequence_number() {
        // The kernel's checksum depends on everything but the checksum field.
        let sum_without_checksum = |flow, seq| {
            let mut packet = echo_request(ECHO_REQUEST_V6, 0x1234, seq, FLOW_CHECKSUM + flow);
            assert_eq!((packet[0], packet[1]), (128, 0));
            packet[2..4].fill(0);
            calculate_icmp_checksum(&packet)
        };
        let flow_3 = sum_without_checksum(3, 0);
        assert!((1..=u16::MAX).all(|seq| sum_without_checksum(3, seq) == flow_3));
        assert_ne!(sum_without_checksum(4, 0), flow_3, "flows differ");
    }

    #[test]
    fn parses_icmpv6_time_exceeded() {
        let mut p = vec![3, 0, 0, 0, 0, 0, 0, 0];
        let mut inner = vec![0u8; 40];
        inner[6] = 58;
        p.extend(inner);
        p.extend(echo(128, 0x1234, 40004));
        let src: IpAddr = "2001:db8::1".parse().unwrap();
        let r = parse_icmpv6(&p, src, Some(62)).unwrap();
        assert_eq!(r.kind, IcmpResponseType::TimeExceeded);
        assert_eq!((r.probe, r.source), (echo_of(0x1234, 40004), src));
        assert_eq!(r.ttl, Some(62), "the hop limit from the control message");
    }
}
