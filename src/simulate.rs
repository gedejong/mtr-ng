//! Demo data for `--simulate`: a made-up path behind the same `Prober` interface
//! as the real engine, so a simulated trace runs through the real probing loop
//! (flows, timeouts, loss) without sockets or privileges.

use crate::geo::{GeoFix, GeoSource};
use crate::icmp_ext::{Extensions, Interface, MplsLabel, Role};
use crate::probe::{IcmpResponseType, Nat, Prober, Reply};
use std::{
    io,
    net::IpAddr,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// One TTL of the made-up path.
struct SimHop {
    /// Routers answering here. With several, flow `f` reaches `routers[f % n]`: a
    /// load-balanced diamond, visible with `--flows 2` or more. Hops 4 and 5
    /// choose independently, so their branches cross, as consecutive balancers'
    /// often do.
    routers: &'static [([u8; 4], &'static str)],
    /// Typical RTT in milliseconds
    rtt_ms: f64,
    /// Chance that a probe goes unanswered
    loss: f64,
    /// MPLS label the router reports, inside a tunnel
    mpls: Option<u32>,
    /// Name and address of the interface probes come in on, if the router says
    /// (RFC 5837)
    interface: Option<(&'static str, [u8; 4])>,
    /// Routers more on the way back than on the way there
    detour: u8,
}

/// The address the home router's NAT gives probes. It leaves the routers' quotes
/// of them that way, which shows it; most NATs change the quotes back.
const NAT_SOURCE: [u8; 4] = [198, 51, 100, 77];

/// TTLs past the end reach the target. Addresses come from the documentation
/// ranges (RFC 5737) and names end in .example, so none of it is a real network.
const PATH: [SimHop; 8] = [
    SimHop {
        routers: &[([192, 168, 1, 1], "gateway.local")],
        rtt_ms: 1.8,
        loss: 0.0,
        mpls: None,
        interface: None,
        detour: 0,
    },
    SimHop {
        routers: &[([198, 51, 100, 1], "bng-1.isp.example")],
        rtt_ms: 8.5,
        loss: 0.0,
        mpls: None,
        interface: None,
        detour: 0,
    },
    SimHop {
        routers: &[([198, 51, 100, 9], "core-1.isp.example")],
        rtt_ms: 10.2,
        loss: 0.05,
        mpls: None,
        interface: None,
        detour: 0,
    },
    SimHop {
        routers: &[
            ([198, 51, 100, 41], "core-2a.isp.example"),
            ([198, 51, 100, 42], "core-2b.isp.example"),
        ],
        rtt_ms: 12.0,
        loss: 0.0,
        mpls: Some(24005),
        interface: None,
        detour: 0,
    },
    SimHop {
        routers: &[
            ([198, 51, 100, 51], "core-3a.isp.example"),
            ([198, 51, 100, 52], "core-3b.isp.example"),
            ([198, 51, 100, 53], "core-3c.isp.example"),
        ],
        rtt_ms: 13.5,
        loss: 0.0,
        mpls: Some(24017),
        interface: None,
        detour: 0,
    },
    // Rate-limits ICMP to zero, as many backbone routers do
    SimHop {
        routers: &[],
        rtt_ms: 0.0,
        loss: 1.0,
        mpls: None,
        interface: None,
        detour: 0,
    },
    // Answers come back through another transit network, two routers longer.
    SimHop {
        routers: &[([203, 0, 113, 7], "ix-1.transit.example")],
        rtt_ms: 21.0,
        loss: 0.01,
        mpls: None,
        interface: Some(("et-0/0/2.100", [203, 0, 113, 6])),
        detour: 2,
    },
    SimHop {
        routers: &[([203, 0, 113, 30], "edge-4.cdn.example")],
        rtt_ms: 23.5,
        loss: 0.0,
        mpls: None,
        interface: None,
        detour: 2,
    },
];

/// RTT of the target, which answers every TTL past the end of `PATH`.
const TARGET_RTT_MS: f64 = 25.0;

/// The gateway's uplink is PPPoE, whose 8-byte header leaves 1492 bytes of an
/// Ethernet frame: larger probes past it get Fragmentation Needed from the gateway.
const UPLINK_MTU: u16 = 1492;

/// Where the made-up routers are: (address, city, country, latitude, longitude).
/// edge-4 claims Ashburn although it answers within 25 ms of Amsterdam, the kind
/// of registrant-address mistake the plausibility check exists for.
const PLACES: [([u8; 4], &str, &str, f64, f64); 9] = [
    ([198, 51, 100, 1], "Amsterdam", "NL", 52.37, 4.89),
    ([198, 51, 100, 9], "Amsterdam", "NL", 52.35, 4.92),
    ([198, 51, 100, 41], "Amsterdam", "NL", 52.30, 4.94),
    ([198, 51, 100, 42], "Amsterdam", "NL", 52.30, 4.94),
    ([198, 51, 100, 51], "Frankfurt am Main", "DE", 50.11, 8.68),
    ([198, 51, 100, 52], "Frankfurt am Main", "DE", 50.11, 8.68),
    ([198, 51, 100, 53], "Frankfurt am Main", "DE", 50.11, 8.68),
    ([203, 0, 113, 7], "London", "GB", 51.51, -0.13),
    ([203, 0, 113, 30], "Ashburn", "US", 39.04, -77.49),
];

/// The target, wherever it really is
const TARGET_PLACE: (&str, &str, f64, f64) = ("London", "GB", 51.50, -0.12);

/// Locations for the made-up path, without a database.
pub struct SimGeo {
    target: IpAddr,
}

impl SimGeo {
    pub fn new(target: IpAddr) -> Self {
        Self { target }
    }
}

impl GeoSource for SimGeo {
    fn locate(&self, addr: IpAddr) -> Option<GeoFix> {
        let (city, country, lat, lon) = match addr == self.target {
            true => TARGET_PLACE,
            false => PLACES
                .iter()
                .find(|place| IpAddr::from(place.0) == addr)
                .map(|&(_, city, country, lat, lon)| (city, country, lat, lon))?,
        };
        Some(GeoFix {
            lat,
            lon,
            accuracy_km: None,
            city: Some(city.to_string()),
            country: Some(country.to_string()),
            from_name: None,
            from_ipmap: None,
        })
    }

    fn attribution(&self) -> &str {
        "made-up locations (--simulate)"
    }
}

/// Host names of the made-up routers, for the DNS cache: a simulation sends no
/// lookups.
pub fn names() -> impl Iterator<Item = (IpAddr, String)> {
    PATH.iter()
        .flat_map(|hop| hop.routers)
        .map(|&(addr, name)| (IpAddr::from(addr), name.to_string()))
}

/// Answers probes along `PATH`, each after its RTT.
pub struct SimProber {
    next_seq: u16,
    tx: mpsc::UnboundedSender<Reply>,
    rx: Option<mpsc::UnboundedReceiver<Reply>>,
}

impl SimProber {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            next_seq: 0,
            tx,
            rx: Some(rx),
        }
    }
}

impl Default for SimProber {
    fn default() -> Self {
        Self::new()
    }
}

impl Prober for SimProber {
    /// Must run inside a Tokio runtime: each reply is delivered by a timer task.
    fn send(&mut self, dst: IpAddr, ttl: u8, flow: u16, size: u16) -> io::Result<(u16, Instant)> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let sent_at = Instant::now();

        let index = usize::from(ttl) - 1;
        let too_big = ttl > 1 && size > UPLINK_MTU;
        let reply = match PATH.get(index) {
            _ if too_big => Some((
                IcmpResponseType::TooBig { mtu: UPLINK_MTU },
                IpAddr::from(PATH[0].routers[0].0),
                PATH[0].rtt_ms,
                255,
                None,
                Vec::new(),
            )),
            Some(hop) if hop.routers.is_empty() || rand::random::<f64>() < hop.loss => None,
            Some(hop) => {
                let (addr, _) = hop.routers[usize::from(flow) % hop.routers.len()];
                let mpls = hop.mpls.map(|label| MplsLabel {
                    label,
                    tc: 0,
                    bottom: true,
                    ttl: 1,
                });
                let interface = hop.interface.map(|(name, addr)| Interface {
                    role: Role::Incoming,
                    index: Some(531),
                    addr: Some(IpAddr::from(addr)),
                    name: Some(name.to_string()),
                    mtu: Some(9192),
                });
                Some((
                    IcmpResponseType::TimeExceeded,
                    IpAddr::from(addr),
                    hop.rtt_ms,
                    255 - (ttl - 1) - hop.detour,
                    mpls,
                    interface.into_iter().collect(),
                ))
            }
            None => Some((
                IcmpResponseType::EchoReply,
                dst,
                TARGET_RTT_MS,
                64 - PATH.len() as u8 - PATH[PATH.len() - 1].detour,
                None,
                Vec::new(),
            )),
        };

        if let Some((kind, source, rtt_ms, reply_ttl, mpls, interfaces)) = reply {
            // Up to 15% jitter, and now and then a spike to three times the RTT
            let factor = if rand::random::<f64>() < 0.02 {
                3.0
            } else {
                1.0 + 0.15 * rand::random::<f64>()
            };
            let rtt = Duration::from_secs_f64(rtt_ms * factor / 1000.0);
            // Routers past the home router quote the probe from its NAT.
            let quoted = matches!(kind, IcmpResponseType::TimeExceeded);
            let nat = (quoted && ttl > 1).then_some(Nat::Source(IpAddr::from(NAT_SOURCE)));
            let reply = Reply {
                kind,
                source,
                seq,
                received_at: sent_at + rtt,
                ttl: Some(reply_ttl),
                ext: Extensions {
                    mpls: mpls.into_iter().collect(),
                    interfaces,
                },
                nat,
            };
            let tx = self.tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(rtt).await;
                let _ = tx.send(reply);
            });
        }
        Ok((seq, sent_at))
    }

    fn take_replies(&mut self) -> Option<mpsc::UnboundedReceiver<Reply>> {
        self.rx.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{session::MtrSession, tracer, Args};
    use clap::Parser;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn a_simulated_trace_goes_through_the_tracer() {
        let args = Args::parse_from([
            "mtr-ng",
            "--simulate",
            "-c",
            "4",
            "-i",
            "5",
            "--flows",
            "2",
            "192.0.2.80",
        ]);
        let mut session = MtrSession::new(args).await.unwrap();
        session.probe_timeout = Duration::from_millis(200);
        let prober = session.open_probe_engine().unwrap();
        let shared = Arc::new(Mutex::new(session));
        tracer::run(Arc::clone(&shared), prober).await.unwrap();
        let s = shared.lock().unwrap().clone();

        assert_eq!(s.display_hop_count(), PATH.len() + 1, "ends at the target");
        let target = &s.hops[PATH.len()];
        assert!(target.is_target);
        assert_eq!(target.addr, Some(IpAddr::from([192, 0, 2, 80])));

        let diamond = &s.hops[3];
        assert_eq!(diamond.responders.len(), 2, "one router per flow");
        assert!(diamond.flows.values().all(|f| f.responders.len() == 1));
        assert_eq!(
            diamond.hostname().map(|n| n.starts_with("core-2")),
            Some(true)
        );
        let labels = &diamond.responders[&diamond.addr.unwrap()].ext.mpls;
        assert_eq!(labels.first().map(|l| l.label), Some(24005));

        assert_eq!(s.hops[5].received, 0, "silent router");
        assert_eq!(s.hops[5].loss_percent, 100.0);
    }
}
