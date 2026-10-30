//! The probing loop shared by report mode and the TUI.
//!
//! Every interval it sends one probe per TTL up to the end of the path, all in one
//! flow (`--flows` rotates through several, one per round; with `--flows auto`
//! the branch search in `mda` picks it), matches replies to
//! probes by sequence number, expires probes that stay unanswered, and records
//! everything in the shared session. With `--pmtu` a round also sends one large
//! probe for the path MTU search, whose answers go to that search instead of the
//! hop statistics. Reverse DNS runs off this path.

use crate::args::Flows;
use crate::mda::Mda;
use crate::probe::{self, IcmpResponseType, ProbeResponse, Prober, Reply};
use crate::record::Replay;
use crate::session::{MtrSession, DEFAULT_REPORT_CYCLES};
use crate::Result;
use hickory_resolver::proto::rr::RData;
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

/// Where a trace's replies come from: probes on the network, or a `--record`
/// log played back.
pub enum Trace {
    Live(Box<dyn Prober>),
    Replay(Box<Replay>),
}

impl Trace {
    /// Open the prober, or for a replay only the geolocation database.
    pub fn open(session: &mut MtrSession, replay: Option<Replay>) -> Result<Self> {
        match replay {
            Some(replay) => {
                session.open_geo()?;
                Ok(Self::Replay(Box::new(replay)))
            }
            None => Ok(Self::Live(session.open_probe_engine()?)),
        }
    }

    /// Run until done: see `run`, or to the end of the log, at once for a report.
    pub async fn run(self, session: Shared) -> Result<()> {
        match self {
            Self::Live(prober) => run(session, prober).await,
            Self::Replay(replay) => {
                let speed = {
                    let s = session.lock().unwrap();
                    (!s.args.report).then_some(s.args.replay_speed)
                };
                replay.run(session, speed).await
            }
        }
    }
}

/// A probe on the wire, waiting for its reply.
struct InFlight {
    hop: usize,
    flow: u16,
    ordinal: u64,
    sent_at: Instant,
}

/// A path MTU probe on the wire
struct MtuProbe {
    ttl: u8,
    size: u16,
    sent_at: Instant,
}

/// Run the trace until `--count` rounds are done (10 in report mode, forever in the
/// TUI without `--count`), then wait for the last replies and reverse lookups.
pub async fn run(session: Shared, mut prober: Box<dyn Prober>) -> Result<()> {
    let (target, args, timeout) = {
        let s = session.lock().unwrap();
        (s.target_addr, s.args.clone(), s.probe_timeout)
    };
    // An exporter (--metrics) runs until stopped, report or not
    let rounds = args
        .count
        .or((args.report && args.metrics.is_none()).then_some(DEFAULT_REPORT_CYCLES));
    // A report without -c waits for --flows auto to find every branch
    let finish_discovery = args.report && args.count.is_none();
    let discovering = || {
        let s = session.lock().unwrap();
        s.mda.as_ref().is_some_and(Mda::discovering)
    };
    let mut replies = prober
        .take_replies()
        .ok_or_else(|| anyhow::anyhow!("prober has no reply stream"))?;

    let mut in_flight: HashMap<u16, InFlight> = HashMap::new();
    let mut mtu_in_flight: HashMap<u16, MtuProbe> = HashMap::new();
    let mut dns = JoinSet::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(args.interval.max(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
    let mut round = 0;

    loop {
        let sending = rounds.is_none_or(|n| round < n) || finish_discovery && discovering();
        if !sending && in_flight.is_empty() && mtu_in_flight.is_empty() {
            break;
        }
        tokio::select! {
            // Replies first, so that the next round's choices (the flow, the path
            // MTU probe's size) see every answer that has arrived.
            biased;
            Some(reply) = replies.recv() => {
                if let Some(probe) = mtu_in_flight.remove(&reply.seq) {
                    mtu_answered(&session, &probe, &reply);
                    continue;
                }
                if matches!(reply.kind, IcmpResponseType::TooBig { .. }) {
                    continue; // a small probe can't be too big; let it time out
                }
                let Some(probe) = in_flight.remove(&reply.seq) else {
                    continue; // late (already timed out) or not from this run
                };
                let rtt = reply.received_at.saturating_duration_since(probe.sent_at);
                debug!("Reply seq={} ttl={} from {} rtt={rtt:?} reply_ttl={:?} mpls={:?}", reply.seq, probe.hop + 1, reply.source, reply.ttl, reply.mpls);
                let response = ProbeResponse {
                    hop: probe.hop,
                    ordinal: Some(probe.ordinal),
                    source_addr: reply.source,
                    icmp_type: reply.kind,
                    rtt,
                    reply_ttl: reply.ttl,
                    mpls: reply.mpls,
                    interfaces: reply.interfaces,
                    nat: reply.nat,
                };
                let (name, asn, ipmap, resolver) = {
                    let mut s = session.lock().unwrap();
                    s.apply_response(&response);
                    if let Some(mda) = s.mda.as_mut() {
                        mda.answered(probe.flow, (probe.hop + 1) as u8, reply.source);
                    }
                    s.notify();
                    // Hidden hops (--privacy-hops) are never looked up
                    let shown = !s.is_hidden(probe.hop + 1);
                    let name = shown && !s.args.numeric && s.want_lookup(reply.source);
                    let asn = shown && s.want_asn(reply.source);
                    let ipmap = shown && s.want_ipmap(reply.source);
                    (name, asn, ipmap, s.resolver.clone())
                };
                if ipmap {
                    dns.spawn(ipmap_lookup(Arc::clone(&session), reply.source));
                }
                if name {
                    dns.spawn(reverse_lookup(Arc::clone(&session), resolver.clone(), reply.source));
                }
                if asn {
                    dns.spawn(asn_lookup(Arc::clone(&session), resolver, reply.source));
                }
            }
            _ = ticker.tick(), if sending => {
                let flow = {
                    let mut s = session.lock().unwrap();
                    let last_ttl = s.last_ttl();
                    match (s.args.flows, s.mda.as_mut()) {
                        (_, Some(mda)) => mda.next_flow(Instant::now(), last_ttl),
                        (Flows::Fixed(n), None) => (round % usize::from(n)) as u16,
                        (Flows::Auto, None) => 0,
                    }
                };
                send_round(&session, prober.as_mut(), target, flow, &mut in_flight);
                send_mtu_probe(&session, prober.as_mut(), target, flow, round, &mut mtu_in_flight);
                round += 1;
                debug!("Sent round {round}");
            }
            _ = sweep.tick() => {
                expire(&session, &mut in_flight, timeout);
                expire_mtu_probes(&session, &mut mtu_in_flight, timeout);
            }
        }
    }

    // Report mode prints right after this; give hostnames a chance to arrive.
    let _ = tokio::time::timeout(DNS_GRACE, async {
        while dns.join_next().await.is_some() {}
    })
    .await;
    if let Some(recorder) = &session.lock().unwrap().recorder {
        recorder.flush();
    }
    Ok(())
}

/// Send one probe in `flow` to every TTL up to the end of the path. A probe that
/// cannot be sent counts as lost; the trace goes on.
fn send_round(
    session: &Shared,
    prober: &mut dyn Prober,
    target: IpAddr,
    flow: u16,
    in_flight: &mut HashMap<u16, InFlight>,
) {
    let mut s = session.lock().unwrap();
    for hop in 0..s.num_hosts {
        let ordinal = s.sent(hop, flow);
        match prober.send(target, (hop + 1) as u8, flow, None) {
            Ok((seq, sent_at)) => {
                let probe = InFlight {
                    hop,
                    flow,
                    ordinal,
                    sent_at,
                };
                if let Some(old) = in_flight.insert(seq, probe) {
                    // Sequence numbers wrapped onto a probe that never got an answer
                    s.lost(old.hop, old.ordinal);
                }
            }
            Err(e) => {
                debug!("Sending to TTL {} failed: {e}", hop + 1);
                s.lost(hop, ordinal);
            }
        }
    }
    s.notify();
}

/// Send this round's path MTU probe, if the search wants one.
fn send_mtu_probe(
    session: &Shared,
    prober: &mut dyn Prober,
    target: IpAddr,
    flow: u16,
    round: usize,
    in_flight: &mut HashMap<u16, MtuProbe>,
) {
    let mut s = session.lock().unwrap();
    let Some((ttl, size)) = s.mtu_probe(round) else {
        return;
    };
    match prober.send(target, ttl, flow, Some(size)) {
        Ok((seq, sent_at)) => {
            debug!("Sent a {size}-byte path MTU probe to TTL {ttl}");
            in_flight.insert(seq, MtuProbe { ttl, size, sent_at });
        }
        Err(e) if probe::is_too_big(&e) => {
            debug!("This machine won't send {size} bytes to the target: {e}");
            if let Some(pmtu) = s.pmtu.as_mut() {
                pmtu.refused(size);
            }
            s.notify();
        }
        Err(e) => debug!("Sending a path MTU probe failed: {e}"),
    }
}

/// Hand the answer to a path MTU probe to the search.
fn mtu_answered(session: &Shared, probe: &MtuProbe, reply: &Reply) {
    debug!(
        "Path MTU probe of {} bytes to TTL {}: {:?} from {}",
        probe.size, probe.ttl, reply.kind, reply.source
    );
    let mut s = session.lock().unwrap();
    let hop = s.hop_of(reply.source);
    let Some(pmtu) = s.pmtu.as_mut() else {
        return;
    };
    match reply.kind {
        IcmpResponseType::TooBig { mtu } => pmtu.too_big(probe.size, reply.source, hop, mtu),
        IcmpResponseType::TimeExceeded
        | IcmpResponseType::EchoReply
        | IcmpResponseType::PortUnreachable => pmtu.arrived(probe.size, probe.ttl),
        IcmpResponseType::DestinationUnreachable => {}
    }
    s.notify();
}

/// Path MTU probes that waited longer than `timeout` vanished, if the hop they
/// went to answers small probes.
fn expire_mtu_probes(session: &Shared, in_flight: &mut HashMap<u16, MtuProbe>, timeout: Duration) {
    let now = Instant::now();
    let expired: Vec<MtuProbe> = in_flight
        .extract_if(|_, p| now.duration_since(p.sent_at) >= timeout)
        .map(|(_, probe)| probe)
        .collect();
    if expired.is_empty() {
        return;
    }
    let mut s = session.lock().unwrap();
    for probe in expired {
        let answers = s
            .hops
            .get(usize::from(probe.ttl) - 1)
            .is_some_and(|hop| hop.received > 0);
        debug!("Path MTU probe of {} bytes vanished", probe.size);
        if let (true, Some(pmtu)) = (answers, s.pmtu.as_mut()) {
            pmtu.lost(probe.size, probe.ttl);
        }
    }
    s.notify();
}

/// Count probes that have waited longer than `timeout` as lost.
fn expire(session: &Shared, in_flight: &mut HashMap<u16, InFlight>, timeout: Duration) {
    let now = Instant::now();
    let expired: Vec<InFlight> = in_flight
        .extract_if(|_, p| now.duration_since(p.sent_at) >= timeout)
        .map(|(_, probe)| probe)
        .collect();
    if expired.is_empty() {
        return;
    }
    let mut s = session.lock().unwrap();
    for probe in expired {
        s.lost(probe.hop, probe.ordinal);
    }
    s.notify();
}

/// The origin AS of `addr`, then the AS's name if no other address asked for it.
async fn asn_lookup(session: Shared, resolver: hickory_resolver::TokioResolver, addr: IpAddr) {
    let asn = crate::asn::lookup(&resolver, addr).await;
    let number = asn.as_ref().map(|info| info.number);
    let name_wanted = {
        let mut s = session.lock().unwrap();
        s.set_asn(addr, asn);
        s.notify();
        number.filter(|&n| s.want_as_name(n))
    };
    if let Some(number) = name_wanted {
        let name = crate::asn::lookup_name(&resolver, number).await;
        let mut s = session.lock().unwrap();
        s.set_as_name(number, name);
        s.notify();
    }
}

/// Where RIPE IPmap puts `addr`. The HTTP client blocks, so on a thread of
/// tokio's blocking pool, which starts long after privileges are given up.
async fn ipmap_lookup(session: Shared, addr: IpAddr) {
    let fix = tokio::task::spawn_blocking(move || crate::geo::ipmap::locate(addr))
        .await
        .ok()
        .flatten();
    let mut s = session.lock().unwrap();
    s.set_ipmap(addr, fix);
    s.notify();
}

async fn reverse_lookup(session: Shared, resolver: hickory_resolver::TokioResolver, addr: IpAddr) {
    let name = resolver
        .reverse_lookup(addr)
        .await
        .ok()
        .and_then(|lookup| {
            lookup
                .answers()
                .iter()
                .find_map(|record| match &record.data {
                    RData::PTR(ptr) => Some(ptr.0.to_utf8()),
                    _ => None,
                })
        })
        .map(|name| name.trim_end_matches('.').to_string())
        .filter(|name| *name != addr.to_string());
    let mut s = session.lock().unwrap();
    s.set_hostname(addr, name);
    s.notify();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icmp_ext::MplsLabel;
    use crate::probe::{Nat, Reply};
    use crate::Args;
    use clap::Parser;
    use std::io;
    use tokio::sync::mpsc;

    /// Answers instantly with exact RTTs: hop `n` replies after `n` ms, with reply
    /// TTL `64 - n`.
    struct FakeProber {
        /// Responder per TTL (index 0 = TTL 1); `None` is a silent hop.
        /// TTLs at or beyond `path.len()` reach the target.
        path: Vec<Option<IpAddr>>,
        /// Load-balanced TTLs: flow `f` gets `routers[f % routers.len()]`.
        branches: HashMap<u8, Vec<IpAddr>>,
        /// TTL whose router reports this MPLS label
        mpls: Option<(u8, MplsLabel)>,
        /// From this TTL on, routers quote the probes as a NAT left them
        nat: Option<(u8, Nat)>,
        fail_ttl: Option<u8>,
        /// Past this TTL's router links take this many bytes, and larger probes get
        /// Fragmentation Needed from it
        narrow: Option<(u8, u16)>,
        /// Past this TTL's router larger probes vanish without a word
        black_hole: Option<(u8, u16)>,
        next_seq: u16,
        tx: mpsc::UnboundedSender<Reply>,
        rx: Option<mpsc::UnboundedReceiver<Reply>>,
    }

    impl FakeProber {
        fn new(path: Vec<Option<IpAddr>>) -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            Self {
                path,
                branches: HashMap::new(),
                mpls: None,
                nat: None,
                fail_ttl: None,
                narrow: None,
                black_hole: None,
                next_seq: 1,
                tx,
                rx: Some(rx),
            }
        }
    }

    impl Prober for FakeProber {
        fn send(
            &mut self,
            dst: IpAddr,
            ttl: u8,
            flow: u16,
            size: Option<u16>,
        ) -> io::Result<(u16, Instant)> {
            if Some(ttl) == self.fail_ttl {
                return Err(io::Error::other("network is unreachable"));
            }
            let seq = self.next_seq;
            self.next_seq += 1;
            let sent_at = Instant::now();
            let index = usize::from(ttl) - 1;
            let beyond = |limit: Option<(u8, u16)>| {
                limit.filter(|&(at, mtu)| ttl > at && size.is_some_and(|size| size > mtu))
            };
            if beyond(self.black_hole).is_some() {
                return Ok((seq, sent_at));
            }
            let answer = match (self.branches.get(&ttl), self.path.get(index)) {
                _ if beyond(self.narrow).is_some() => {
                    let (at, mtu) = self.narrow.unwrap();
                    self.path[usize::from(at) - 1]
                        .map(|router| (IcmpResponseType::TooBig { mtu }, router))
                }
                (Some(routers), _) => Some(routers[usize::from(flow) % routers.len()])
                    .map(|source| (IcmpResponseType::TimeExceeded, source)),
                (None, Some(router)) => {
                    router.map(|source| (IcmpResponseType::TimeExceeded, source))
                }
                (None, None) => Some((IcmpResponseType::EchoReply, dst)),
            };
            if let Some((kind, source)) = answer {
                let hops = ttl.min(self.path.len() as u8 + 1);
                let mpls = self
                    .mpls
                    .filter(|&(at, _)| at == ttl)
                    .map(|(_, label)| label);
                let _ = self.tx.send(Reply {
                    kind,
                    source,
                    seq,
                    received_at: sent_at + Duration::from_millis(u64::from(hops)),
                    ttl: Some(64 - hops),
                    mpls: mpls.into_iter().collect(),
                    interfaces: Vec::new(),
                    nat: self
                        .nat
                        .filter(|&(at, _)| ttl >= at && kind == IcmpResponseType::TimeExceeded)
                        .map(|(_, nat)| nat),
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
        trace_with(prober, &["-c", "3"]).await
    }

    async fn trace_with(prober: FakeProber, cli: &[&str]) -> MtrSession {
        let cli = ["mtr-ng", "-n"].iter().chain(cli).chain(&["192.0.2.1"]);
        let mut args = Args::parse_from(cli);
        args.interval = 5; // below the CLI minimum, to keep the test fast
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

    #[tokio::test]
    async fn each_flow_keeps_to_its_own_branch() {
        let mut prober = FakeProber::new(twelve_hop_path());
        let pair = |ttl: u8| vec![IpAddr::from([10, 0, ttl, 1]), IpAddr::from([10, 0, ttl, 2])];
        prober.branches = HashMap::from([(5, pair(5)), (6, pair(6))]);
        let s = trace_with(prober, &["-c", "8", "--flows", "4"]).await;

        let hop5 = &s.hops[4];
        assert_eq!(hop5.flow_count(), 4);
        for (&flow, stats) in &hop5.flows {
            assert_eq!(stats.sent, 2, "flow {flow}");
            assert_eq!(stats.responders.len(), 1, "flow {flow} stays on one path");
            assert_eq!(stats.responder(), Some(pair(5)[usize::from(flow) % 2]));
        }
        assert_eq!(hop5.get_primary_path_percentage(), 50.0);
        assert!(
            s.hops[3].flows.values().all(|f| f.received == 0),
            "silent hop"
        );
        assert!(s.hops[6].flows.values().all(|f| f.responders.len() == 1));
    }

    /// A path with four branches at TTL 5 and two at TTL 6
    fn balanced() -> FakeProber {
        let mut prober = FakeProber::new(twelve_hop_path());
        let ways = |ttl: u8, n: u8| (1..=n).map(|i| IpAddr::from([10, 0, ttl, i])).collect();
        prober.branches = HashMap::from([(5, ways(5, 4)), (6, ways(6, 2))]);
        prober
    }

    #[tokio::test]
    async fn flows_auto_finds_every_branch_then_probes_one_flow_per_branch() {
        let s = trace_with(balanced(), &["-c", "41", "--flows", "auto"]).await;

        let mda = s.mda.as_ref().unwrap();
        assert!(!mda.discovering());
        assert_eq!(s.flows(), 21, "four branches take 21 flows to be sure of");
        let hop5 = &s.hops[4];
        assert_eq!(hop5.responders.len(), 4);
        assert_eq!(mda.cover(s.last_ttl()), [0, 1, 2, 3]);
        // The other 20 rounds went to the four flows that pass every branch
        for flow in 0..21 {
            let expected = if flow < 4 { 6 } else { 1 };
            assert_eq!(hop5.flows[&flow].sent, expected, "flow {flow}");
        }
        assert_eq!(
            s.notes_after(5),
            ["4 branches, all found (95%)"],
            "and none for hops with one address"
        );
        assert!(s.notes_after(7).is_empty());
    }

    #[tokio::test]
    async fn a_report_without_a_count_lets_discovery_finish() {
        let s = trace_with(balanced(), &["-r", "--flows", "auto"]).await;

        assert!(!s.mda.as_ref().unwrap().discovering());
        assert_eq!(s.hops[0].sent, 22, "21 flows, then one round of the cover");
    }

    #[tokio::test]
    async fn reply_ttl_and_mpls_labels_reach_the_responder() {
        let label = MplsLabel {
            label: 24005,
            tc: 0,
            bottom: true,
            ttl: 1,
        };
        let mut prober = FakeProber::new(twelve_hop_path());
        prober.mpls = Some((3, label));
        let s = trace(prober).await;

        let hop3 = &s.hops[2].responders[&router(3).unwrap()];
        assert_eq!(hop3.mpls, [label]);
        assert_eq!(hop3.reply_ttl, Some(61));
        assert!(s.hops[1].responders[&router(2).unwrap()].mpls.is_empty());
    }

    /// A log path of its own for each test, removed first
    fn log_path(name: &str) -> std::path::PathBuf {
        let file = format!("mtr-ng-{}-{name}.ndjson", std::process::id());
        let path = std::env::temp_dir().join(file);
        let _ = std::fs::remove_file(&path);
        path
    }

    #[tokio::test]
    async fn a_recorded_trace_replays_to_the_same_statistics() {
        let path = log_path("same");
        let mut prober = balanced();
        prober.mpls = Some((
            3,
            MplsLabel {
                label: 24005,
                tc: 0,
                bottom: true,
                ttl: 1,
            },
        ));
        prober.fail_ttl = Some(9); // lost without being sent
        prober.nat = Some((2, Nat::Source(IpAddr::from([203, 0, 113, 9]))));
        let record = [
            "-c",
            "4",
            "--flows",
            "2",
            "--record",
            path.to_str().unwrap(),
        ];
        let live = trace_with(prober, &record).await;

        let replay = Replay::open(&path).await.unwrap();
        let args = Args::parse_from(["mtr-ng", "--replay", path.to_str().unwrap()]);
        let shared = Arc::new(Mutex::new(replay.session(args).await.unwrap()));
        replay.run(Arc::clone(&shared), None).await.unwrap();
        let replayed = shared.lock().unwrap().clone();
        std::fs::remove_file(&path).unwrap();

        assert_eq!(replayed.target, "192.0.2.1");
        assert_eq!(replayed.flows(), 2);
        assert_eq!(replayed.display_hop_count(), 12);
        let responders = |hop: &crate::HopStats| {
            let mut responders: Vec<_> = hop
                .responders
                .values()
                .map(|r| (r.addr, r.replies, r.reply_ttl, r.mpls.clone(), r.nat))
                .collect();
            responders.sort_by_key(|r| r.0);
            responders
        };
        let flows = |hop: &crate::HopStats| -> Vec<(u16, usize, usize)> {
            hop.flows
                .iter()
                .map(|(&f, s)| (f, s.sent, s.received))
                .collect()
        };
        for (a, b) in live.hops.iter().zip(&replayed.hops) {
            let hop = a.hop;
            assert_eq!((a.sent, a.received), (b.sent, b.received), "hop {hop}");
            assert_eq!(a.loss_percent, b.loss_percent, "hop {hop}");
            assert_eq!(a.history, b.history, "hop {hop}");
            assert_eq!(
                (a.best_rtt, a.worst_rtt, a.avg_rtt, a.last_rtt, a.jitter_avg),
                (b.best_rtt, b.worst_rtt, b.avg_rtt, b.last_rtt, b.jitter_avg),
                "hop {hop}"
            );
            assert_eq!(a.is_target, b.is_target, "hop {hop}");
            assert_eq!(responders(a), responders(b), "hop {hop}");
            assert_eq!(flows(a), flows(b), "hop {hop}");
            assert_eq!(
                live.notes_after(hop),
                replayed.notes_after(hop),
                "hop {hop}"
            );
        }
        assert_eq!(
            replayed.notes_after(2),
            ["NAT before this hop: probes reach it from 203.0.113.9"]
        );
        assert_eq!(
            replayed.hops[8].loss_percent, 100.0,
            "the probes that failed"
        );
        assert_eq!(
            replayed.hops[4].responders.len(),
            2,
            "two flows, two branches"
        );
    }

    /// A trace with `--pmtu` and the search starting at 1500 bytes
    async fn pmtu_trace(prober: FakeProber, rounds: &str) -> MtrSession {
        let cli = ["mtr-ng", "-n", "--pmtu", "-c", rounds, "192.0.2.1"];
        let mut args = Args::parse_from(cli);
        args.interval = 5;
        let mut session = MtrSession::new(args).await.unwrap();
        session.probe_timeout = Duration::from_millis(20);
        session.pmtu = Some(crate::pmtu::PathMtu::new(1500, false));
        let shared = Arc::new(Mutex::new(session));
        run(Arc::clone(&shared), Box::new(prober)).await.unwrap();
        let session = shared.lock().unwrap().clone();
        session
    }

    #[tokio::test]
    async fn the_path_mtu_is_where_a_router_says_it_narrows() {
        let mut prober = FakeProber::new(twelve_hop_path());
        prober.narrow = Some((3, 1400));
        let s = pmtu_trace(prober, "5").await;

        let pmtu = s.pmtu.as_ref().unwrap();
        assert_eq!(pmtu.mtu(), Some(1400));
        assert_eq!(pmtu.drops.len(), 1);
        assert_eq!(
            (pmtu.drops[0].hop, pmtu.drops[0].router),
            (Some(3), router(3))
        );
        for hop in &s.hops[..12] {
            assert_eq!(
                hop.sent, 5,
                "MTU probes don't count as probes to hop {}",
                hop.hop
            );
            assert_eq!(hop.loss_percent, if hop.hop == 4 { 100.0 } else { 0.0 });
        }
    }

    #[tokio::test]
    async fn a_black_hole_is_found_by_halving() {
        let mut prober = FakeProber::new(twelve_hop_path());
        prober.black_hole = Some((5, 1300));
        // A size counts as vanished after two timeouts, so this takes a while.
        let s = pmtu_trace(prober, "200").await;

        let pmtu = s.pmtu.as_ref().unwrap();
        assert_eq!((pmtu.mtu(), pmtu.black_hole()), (Some(1300), Some(1301)));
        assert!(pmtu.drops.is_empty(), "no router said a word");
        assert_eq!(s.hops[11].loss_percent, 0.0, "small probes unaffected");
        let hole = pmtu.hole.as_ref().unwrap();
        assert!(hole.found, "located too");
        assert_eq!((hole.passed, hole.lost_at), (5, 6), "past hop 5");
        assert_eq!(
            s.notes_after(5),
            ["probes over 1300 bytes vanish past this hop"]
        );
    }
}
