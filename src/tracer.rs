//! The probing loop shared by report mode and the TUI.
//!
//! Every interval it sends one probe per TTL up to the end of the path, matches
//! replies to probes by sequence number, expires probes that stay unanswered, and
//! records everything in the shared session. Reverse DNS runs off this path.

use crate::probe::{IcmpResponseType, ProbeResponse, Prober};
use crate::session::{MtrSession, DEFAULT_REPORT_CYCLES};
use crate::Result;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{task::JoinSet, time::MissedTickBehavior};
use tracing::debug;

/// How long a probe may stay unanswered before it counts as lost.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often unanswered probes are checked against the timeout.
const SWEEP_INTERVAL: Duration = Duration::from_millis(50);

/// How long a finished trace waits for outstanding reverse lookups.
const DNS_GRACE: Duration = Duration::from_secs(3);

type Shared = Arc<Mutex<MtrSession>>;

/// A probe on the wire, waiting for its reply.
struct InFlight {
    hop: usize,
    ordinal: u64,
    sent_at: Instant,
}

/// Run the trace until `--count` rounds are done (10 in report mode, forever in the
/// TUI without `--count`), then wait for the last replies and reverse lookups.
pub async fn run(session: Shared, mut prober: Box<dyn Prober>) -> Result<()> {
    let (target, args, timeout) = {
        let s = session.lock().unwrap();
        (s.target_addr, s.args.clone(), s.probe_timeout)
    };
    let rounds = args.count.or(args.report.then_some(DEFAULT_REPORT_CYCLES));
    let mut replies = prober
        .take_replies()
        .ok_or_else(|| anyhow::anyhow!("prober has no reply stream"))?;

    let mut in_flight: HashMap<u16, InFlight> = HashMap::new();
    let mut dns = JoinSet::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(args.interval.max(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
    let mut round = 0;

    loop {
        let sending = rounds.is_none_or(|n| round < n);
        if !sending && in_flight.is_empty() {
            break;
        }
        tokio::select! {
            _ = ticker.tick(), if sending => {
                send_round(&session, prober.as_mut(), target, &mut in_flight);
                round += 1;
                debug!("Sent round {round}");
            }
            Some(reply) = replies.recv() => {
                let Some(probe) = in_flight.remove(&reply.seq) else {
                    continue; // late (already timed out) or not from this run
                };
                let rtt = reply.received_at.saturating_duration_since(probe.sent_at);
                debug!("Reply seq={} ttl={} from {} rtt={rtt:?}", reply.seq, probe.hop + 1, reply.source);
                let response = ProbeResponse {
                    hop: probe.hop,
                    ordinal: Some(probe.ordinal),
                    seq: reply.seq,
                    source_addr: reply.source,
                    icmp_type: reply.kind,
                    rtt,
                    send_time: probe.sent_at,
                    receive_time: reply.received_at,
                    precise_rtt_ns: rtt.as_nanos(),
                };
                let lookup = {
                    let mut s = session.lock().unwrap();
                    s.apply_response(&response);
                    s.notify();
                    (!s.args.numeric && s.want_lookup(reply.source)).then(|| s.resolver.clone())
                };
                if let Some(resolver) = lookup {
                    dns.spawn(reverse_lookup(Arc::clone(&session), resolver, reply.source));
                }
            }
            _ = sweep.tick() => expire(&session, &mut in_flight, timeout),
        }
    }

    // Report mode prints right after this; give hostnames a chance to arrive.
    let _ = tokio::time::timeout(DNS_GRACE, async {
        while dns.join_next().await.is_some() {}
    })
    .await;
    Ok(())
}

/// Send one probe to every TTL up to the end of the path. A probe that cannot be
/// sent counts as lost; the trace goes on.
fn send_round(
    session: &Shared,
    prober: &mut dyn Prober,
    target: IpAddr,
    in_flight: &mut HashMap<u16, InFlight>,
) {
    let mut s = session.lock().unwrap();
    for hop in 0..s.num_hosts {
        let ordinal = s.hops[hop].increment_sent();
        match prober.send(target, (hop + 1) as u8) {
            Ok((seq, sent_at)) => {
                let probe = InFlight {
                    hop,
                    ordinal,
                    sent_at,
                };
                if let Some(old) = in_flight.insert(seq, probe) {
                    // Sequence numbers wrapped onto a probe that never got an answer
                    s.hops[old.hop].record_timeout(Some(old.ordinal));
                }
            }
            Err(e) => {
                debug!("Sending to TTL {} failed: {e}", hop + 1);
                s.hops[hop].record_timeout(Some(ordinal));
            }
        }
    }
    s.notify();
}

/// Count probes that have waited longer than `timeout` as lost.
fn expire(session: &Shared, in_flight: &mut HashMap<u16, InFlight>, timeout: Duration) {
    let now = Instant::now();
    let expired: Vec<u16> = in_flight
        .iter()
        .filter(|(_, p)| now.duration_since(p.sent_at) >= timeout)
        .map(|(seq, _)| *seq)
        .collect();
    if expired.is_empty() {
        return;
    }
    let mut s = session.lock().unwrap();
    for seq in expired {
        if let Some(probe) = in_flight.remove(&seq) {
            s.apply_response(&ProbeResponse {
                hop: probe.hop,
                ordinal: Some(probe.ordinal),
                seq,
                source_addr: IpAddr::from([0, 0, 0, 0]),
                icmp_type: IcmpResponseType::Timeout,
                rtt: timeout,
                send_time: probe.sent_at,
                receive_time: now,
                precise_rtt_ns: 0,
            });
        }
    }
    s.notify();
}

async fn reverse_lookup(
    session: Shared,
    resolver: hickory_resolver::TokioAsyncResolver,
    addr: IpAddr,
) {
    let name = resolver
        .reverse_lookup(addr)
        .await
        .ok()
        .and_then(|names| names.iter().next().map(|n| n.to_utf8()))
        .map(|name| name.trim_end_matches('.').to_string())
        .filter(|name| *name != addr.to_string());
    let mut s = session.lock().unwrap();
    s.set_hostname(addr, name);
    s.notify();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::Reply;
    use crate::Args;
    use clap::Parser;
    use std::io;
    use tokio::sync::mpsc;

    /// Answers instantly with exact RTTs: hop `n` replies after `n` ms.
    struct FakeProber {
        /// Responder per TTL (index 0 = TTL 1); `None` is a silent hop.
        /// TTLs at or beyond `path.len()` reach the target.
        path: Vec<Option<IpAddr>>,
        fail_ttl: Option<u8>,
        next_seq: u16,
        tx: mpsc::UnboundedSender<Reply>,
        rx: Option<mpsc::UnboundedReceiver<Reply>>,
    }

    impl FakeProber {
        fn new(path: Vec<Option<IpAddr>>) -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            Self {
                path,
                fail_ttl: None,
                next_seq: 1,
                tx,
                rx: Some(rx),
            }
        }
    }

    impl Prober for FakeProber {
        fn send(&mut self, dst: IpAddr, ttl: u8) -> io::Result<(u16, Instant)> {
            if Some(ttl) == self.fail_ttl {
                return Err(io::Error::other("network is unreachable"));
            }
            let seq = self.next_seq;
            self.next_seq += 1;
            let sent_at = Instant::now();
            let index = usize::from(ttl) - 1;
            let answer = match self.path.get(index) {
                Some(router) => router.map(|r| (IcmpResponseType::TimeExceeded, r)),
                None => Some((IcmpResponseType::EchoReply, dst)),
            };
            if let Some((kind, source)) = answer {
                let rtt = Duration::from_millis(u64::from(ttl.min(self.path.len() as u8 + 1)));
                let _ = self.tx.send(Reply {
                    kind,
                    source,
                    seq,
                    received_at: sent_at + rtt,
                });
            }
            Ok((seq, sent_at))
        }

        fn take_replies(&mut self) -> Option<mpsc::UnboundedReceiver<Reply>> {
            self.rx.take()
        }
    }

    fn router(n: u8) -> Option<IpAddr> {
        Some(IpAddr::from([10, 0, 0, n]))
    }

    /// Run 3 rounds against a 12-hop path (target at TTL 12, TTL 4 silent).
    async fn trace(prober: FakeProber) -> MtrSession {
        let args = Args::parse_from(["mtr-ng", "-n", "-c", "3", "-i", "5", "192.0.2.1"]);
        let mut session = MtrSession::new(args).await.unwrap();
        session.probe_timeout = Duration::from_millis(100);
        let shared = Arc::new(Mutex::new(session));
        run(Arc::clone(&shared), Box::new(prober)).await.unwrap();
        let session = shared.lock().unwrap().clone();
        session
    }

    fn twelve_hop_path() -> Vec<Option<IpAddr>> {
        (1..=11)
            .map(|n| if n == 4 { None } else { router(n) })
            .collect()
    }

    #[tokio::test]
    async fn long_path_is_completed_and_measured_exactly() {
        let s = trace(FakeProber::new(twelve_hop_path())).await;

        assert_eq!(
            s.display_hop_count(),
            12,
            "target at TTL 12 (beyond the old 10-hop cap)"
        );
        assert_eq!(s.num_hosts, 12, "later rounds stop at the target");
        assert_eq!(s.hops[11].addr, Some(IpAddr::from([192, 0, 2, 1])));
        assert_eq!(s.hops[0].avg_rtt, Some(Duration::from_millis(1)));
        assert_eq!(s.hops[11].avg_rtt, Some(Duration::from_millis(12)));
        for (i, hop) in s.hops[..12].iter().enumerate() {
            assert_eq!(hop.sent, 3, "hop {}", i + 1);
            assert_eq!(hop.in_flight(), 0, "drained at the end");
            let expected_loss = if i == 3 { 100.0 } else { 0.0 };
            assert_eq!(hop.loss_percent, expected_loss, "hop {}", i + 1);
        }
        assert_eq!(
            s.hops[12].sent, 1,
            "TTLs past the target only in the first round"
        );
    }

    #[tokio::test]
    async fn send_failure_counts_as_loss_and_trace_continues() {
        let mut prober = FakeProber::new(twelve_hop_path());
        prober.fail_ttl = Some(2);
        let s = trace(prober).await;

        assert_eq!(s.hops[1].loss_percent, 100.0);
        assert_eq!(s.hops[1].sent, 3);
        assert_eq!(s.hops[2].loss_percent, 0.0);
        assert_eq!(s.display_hop_count(), 12);
    }
}
