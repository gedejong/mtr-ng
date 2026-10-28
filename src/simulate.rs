//! Demo data for `--simulate`: a made-up path behind the same `Prober` interface
//! as the real engine, so a simulated trace runs through the real probing loop
//! (flows, timeouts, loss) without sockets or privileges.

use crate::icmp_ext::MplsLabel;
use crate::probe::{IcmpResponseType, Prober, Reply};
use std::{
    io,
    net::IpAddr,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// One TTL of the made-up path.
struct SimHop {
    /// Routers answering here. With two, flow `f` reaches `routers[f % 2]`: a
    /// load-balanced diamond, visible with `--flows 2` or more.
    routers: &'static [([u8; 4], &'static str)],
    /// Typical RTT in milliseconds
    rtt_ms: f64,
    /// Chance that a probe goes unanswered
    loss: f64,
    /// MPLS label the router reports, inside a tunnel
    mpls: Option<u32>,
}

/// TTLs past the end reach the target. Addresses come from the documentation
/// ranges (RFC 5737) and names end in .example, so none of it is a real network.
const PATH: [SimHop; 8] = [
    SimHop {
        routers: &[([192, 168, 1, 1], "gateway.local")],
        rtt_ms: 1.8,
        loss: 0.0,
        mpls: None,
    },
    SimHop {
        routers: &[([198, 51, 100, 1], "bng-1.isp.example")],
        rtt_ms: 8.5,
        loss: 0.0,
        mpls: None,
    },
    SimHop {
        routers: &[([198, 51, 100, 9], "core-1.isp.example")],
        rtt_ms: 10.2,
        loss: 0.05,
        mpls: None,
    },
    SimHop {
        routers: &[
            ([198, 51, 100, 41], "core-2a.isp.example"),
            ([198, 51, 100, 42], "core-2b.isp.example"),
        ],
        rtt_ms: 12.0,
        loss: 0.0,
        mpls: Some(24005),
    },
    SimHop {
        routers: &[
            ([198, 51, 100, 51], "core-3a.isp.example"),
            ([198, 51, 100, 52], "core-3b.isp.example"),
        ],
        rtt_ms: 13.5,
        loss: 0.0,
        mpls: Some(24017),
    },
    // Rate-limits ICMP to zero, as many backbone routers do
    SimHop {
        routers: &[],
        rtt_ms: 0.0,
        loss: 1.0,
        mpls: None,
    },
    SimHop {
        routers: &[([203, 0, 113, 7], "ix-1.transit.example")],
        rtt_ms: 21.0,
        loss: 0.01,
        mpls: None,
    },
    SimHop {
        routers: &[([203, 0, 113, 30], "edge-4.cdn.example")],
        rtt_ms: 23.5,
        loss: 0.0,
        mpls: None,
    },
];

/// RTT of the target, which answers every TTL past the end of `PATH`.
const TARGET_RTT_MS: f64 = 25.0;

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
    fn send(&mut self, dst: IpAddr, ttl: u8, flow: u16) -> io::Result<(u16, Instant)> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let sent_at = Instant::now();

        let index = usize::from(ttl) - 1;
        let reply = match PATH.get(index) {
            Some(hop) if hop.routers.is_empty() || rand::random::<f64>() < hop.loss => None,
            Some(hop) => {
                let (addr, _) = hop.routers[usize::from(flow) % hop.routers.len()];
                let mpls = hop.mpls.map(|label| MplsLabel {
                    label,
                    tc: 0,
                    bottom: true,
                    ttl: 1,
                });
                Some((
                    IcmpResponseType::TimeExceeded,
                    IpAddr::from(addr),
                    hop.rtt_ms,
                    255 - (ttl - 1),
                    mpls,
                ))
            }
            None => Some((
                IcmpResponseType::EchoReply,
                dst,
                TARGET_RTT_MS,
                64 - PATH.len() as u8,
                None,
            )),
        };

        if let Some((kind, source, rtt_ms, reply_ttl, mpls)) = reply {
            // Up to 15% jitter, and now and then a spike to three times the RTT
            let factor = if rand::random::<f64>() < 0.02 {
                3.0
            } else {
                1.0 + 0.15 * rand::random::<f64>()
            };
            let rtt = Duration::from_secs_f64(rtt_ms * factor / 1000.0);
            let reply = Reply {
                kind,
                source,
                seq,
                received_at: sent_at + rtt,
                ttl: Some(reply_ttl),
                mpls: mpls.into_iter().collect(),
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
            diamond.hostname.as_deref().map(|n| n.starts_with("core-2")),
            Some(true)
        );
        let labels = &diamond.responders[&diamond.addr.unwrap()].mpls;
        assert_eq!(labels.first().map(|l| l.label), Some(24005));

        assert_eq!(s.hops[5].received, 0, "silent router");
        assert_eq!(s.hops[5].loss_percent, 100.0);
    }
}
