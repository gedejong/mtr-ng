use crate::geo::{self, Assessment, GeoFix, Origin};
use crate::icmp_ext::MplsLabel;
use crate::utils;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    net::IpAddr,
    time::{Duration, Instant},
};

/// Probes remembered per hop: for the history graph, the RTT distribution and
/// matching late replies. Ten minutes at the default interval.
const HISTORY_LEN: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PacketOutcome {
    Received(Duration), // RTT
    Lost,               // Timeout/no response
    Pending,            // Sent but no response yet
}

/// One probe sent to this hop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeRecord {
    pub flow: u16,
    pub outcome: PacketOutcome,
    /// Who answered it
    pub from: Option<IpAddr>,
}

/// What the probes of one flow saw at this hop.
#[derive(Debug, Clone, Default)]
pub struct FlowStats {
    pub sent: usize,
    pub received: usize,
    /// Replies per answering address. More than one means the route changed
    /// during the trace, or a router balances per packet rather than per flow.
    pub responders: HashMap<IpAddr, usize>,
}

impl FlowStats {
    /// The address that answered this flow most often.
    pub fn responder(&self) -> Option<IpAddr> {
        self.responders
            .iter()
            .max_by_key(|&(addr, replies)| (replies, std::cmp::Reverse(addr)))
            .map(|(&addr, _)| addr)
    }
}

/// RTT percentiles over the probes still in the history window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RttSpread {
    pub samples: usize,
    pub min: Duration,
    pub p50: Duration,
    pub p90: Duration,
    pub p99: Duration,
    pub max: Duration,
}

/// Statistics for one address that answered at this hop. With load balancing a
/// hop can have several; the one with the most replies is the hop's primary.
#[derive(Debug, Clone)]
pub struct ResponderStats {
    pub addr: IpAddr,
    pub hostname: Option<String>,
    /// Origin AS number, with `--aslookup`
    pub asn: Option<u32>,
    pub replies: usize,
    pub last_seen: Instant,
    pub last_rtt: Option<Duration>,
    pub best_rtt: Option<Duration>,
    /// TTL of its latest reply on arrival, which hints at the return path's length
    pub reply_ttl: Option<u8>,
    /// MPLS labels in its latest reply
    pub mpls: Vec<MplsLabel>,
    /// Where the geolocation database puts it
    pub geo: Option<GeoFix>,
    rtt_sum_ns: u128,
}

impl ResponderStats {
    fn new(addr: IpAddr) -> Self {
        Self {
            addr,
            hostname: None,
            asn: None,
            replies: 0,
            last_seen: Instant::now(),
            last_rtt: None,
            best_rtt: None,
            reply_ttl: None,
            mpls: Vec::new(),
            geo: None,
            rtt_sum_ns: 0,
        }
    }

    fn record(&mut self, rtt: Duration) {
        self.replies += 1;
        self.last_seen = Instant::now();
        self.last_rtt = Some(rtt);
        self.best_rtt = Some(self.best_rtt.map_or(rtt, |best| best.min(rtt)));
        self.rtt_sum_ns += rtt.as_nanos();
    }

    pub fn avg_rtt(&self) -> Option<Duration> {
        mean_duration(self.rtt_sum_ns, self.replies)
    }

    /// Routers its latest reply crossed on the way back, if its TTL is known.
    pub fn routers_back(&self) -> Option<u8> {
        self.reply_ttl.map(routers_back)
    }
}

/// TTLs that systems commonly send packets with.
const INITIAL_TTLS: [u8; 4] = [32, 64, 128, 255];

/// Routers a packet crossed, judged by the TTL it arrived with: it most likely
/// left with the next common initial TTL up.
pub fn routers_back(arrival_ttl: u8) -> u8 {
    let initial = INITIAL_TTLS
        .into_iter()
        .find(|&ttl| ttl >= arrival_ttl)
        .unwrap_or(u8::MAX);
    initial - arrival_ttl
}

fn mean_duration(sum_ns: u128, count: usize) -> Option<Duration> {
    (count > 0).then(|| Duration::from_nanos((sum_ns / count as u128) as u64))
}

#[derive(Debug, Clone)]
pub struct HopStats {
    pub hop: u8,
    /// Primary responder (the address with the most replies)
    pub addr: Option<IpAddr>,
    pub hostname: Option<String>,
    pub asn: Option<u32>,
    /// Where the geolocation database puts the primary responder
    pub geo: Option<GeoFix>,
    pub sent: usize,
    pub received: usize,
    pub last_rtt: Option<Duration>,
    pub best_rtt: Option<Duration>,
    pub worst_rtt: Option<Duration>,
    /// Mean over all replies since start (or reset)
    pub avg_rtt: Option<Duration>,
    pub ema_rtt: Option<Duration>, // Exponentially smoothed average RTT
    pub jitter_avg: Option<Duration>, // Mean of all jitter values
    pub last_jitter: Option<Duration>, // Last calculated jitter value
    /// The most recent probes, oldest first
    pub history: VecDeque<ProbeRecord>,
    /// Loss among probes that have been answered or timed out (in-flight excluded)
    pub loss_percent: f64,
    // Exponential smoothing factor (0.0 to 1.0)
    // Higher values = more responsive to recent changes
    // Lower values = more stable, less sensitive to spikes
    // Typical values: 0.1-0.3 for network monitoring
    pub ema_alpha: f64,

    /// Every address that answered at this hop
    pub responders: HashMap<IpAddr, ResponderStats>,
    /// Per flow (see `Prober::send`), which addresses answered it
    pub flows: BTreeMap<u16, FlowStats>,

    // Target tracking
    pub is_target: bool,

    // ICMP error tracking (for MTR algorithm compatibility)
    pub icmp_error: bool,

    // All-time accumulators
    rtt_sum_ns: u128,
    rtt_mean_ns: f64, // Welford running mean and sum of squared deviations
    rtt_m2: f64,
    jitter_sum_ns: u128,
    jitter_count: usize,
}

impl HopStats {
    pub fn new(hop: u8) -> Self {
        Self {
            hop,
            addr: None,
            hostname: None,
            asn: None,
            geo: None,
            sent: 0,
            received: 0,
            last_rtt: None,
            best_rtt: None,
            worst_rtt: None,
            avg_rtt: None,
            ema_rtt: None,
            jitter_avg: None,
            last_jitter: None,
            history: VecDeque::with_capacity(HISTORY_LEN),
            loss_percent: 0.0,
            ema_alpha: 0.1,
            responders: HashMap::new(),
            flows: BTreeMap::new(),
            is_target: false,
            icmp_error: false,
            rtt_sum_ns: 0,
            rtt_mean_ns: 0.0,
            rtt_m2: 0.0,
            jitter_sum_ns: 0,
            jitter_count: 0,
        }
    }

    /// Count a probe sent to this hop in `flow`. Returns its ordinal, which
    /// identifies the probe when its reply or timeout is recorded.
    pub fn increment_sent(&mut self, flow: u16) -> u64 {
        let ordinal = self.sent as u64;
        self.sent += 1;
        self.flows.entry(flow).or_default().sent += 1;
        self.history.push_back(ProbeRecord {
            flow,
            outcome: PacketOutcome::Pending,
            from: None,
        });
        if self.history.len() > HISTORY_LEN {
            self.history.pop_front();
        }
        self.update_loss_percent();
        ordinal
    }

    /// Record a reply. `ordinal` identifies the probe (from `increment_sent`); without
    /// one the most recent pending probe is assumed, which suits lockstep callers.
    /// Replies to unknown, already answered or timed-out probes are ignored.
    pub fn record_reply(&mut self, ordinal: Option<u64>, addr: Option<IpAddr>, rtt: Duration) {
        let slot = match ordinal {
            Some(ordinal) => match self.pending_slot(ordinal) {
                Some(slot) => Some(slot),
                None => return,
            },
            None => self
                .history
                .iter()
                .rposition(|r| r.outcome == PacketOutcome::Pending),
        };
        let flow = slot.map(|slot| {
            let record = &mut self.history[slot];
            record.outcome = PacketOutcome::Received(rtt);
            record.from = addr;
            record.flow
        });

        self.received += 1;
        if let Some(flow) = flow.and_then(|flow| self.flows.get_mut(&flow)) {
            flow.received += 1;
            if let Some(addr) = addr {
                *flow.responders.entry(addr).or_default() += 1;
            }
        }
        if let Some(addr) = addr {
            self.responders
                .entry(addr)
                .or_insert_with(|| ResponderStats::new(addr))
                .record(rtt);
            self.update_primary(addr);
        }
        self.update_rtt_stats(rtt);
        self.update_loss_percent();
    }

    /// Record a probe that got no answer in time. Without an ordinal the oldest
    /// pending probe is assumed.
    pub fn record_timeout(&mut self, ordinal: Option<u64>) {
        let slot = match ordinal {
            Some(ordinal) => self.pending_slot(ordinal),
            None => self
                .history
                .iter()
                .position(|r| r.outcome == PacketOutcome::Pending),
        };
        if let Some(slot) = slot {
            self.history[slot].outcome = PacketOutcome::Lost;
        }
        self.update_loss_percent();
    }

    /// Remember what `addr` put in its latest reply besides the timing.
    pub fn record_reply_details(&mut self, addr: IpAddr, ttl: Option<u8>, mpls: &[MplsLabel]) {
        if let Some(responder) = self.responders.get_mut(&addr) {
            responder.reply_ttl = ttl;
            responder.mpls = mpls.to_vec();
        }
    }

    /// Reply for the most recent probe, no address (tests).
    pub fn add_rtt(&mut self, rtt: Duration) {
        self.record_reply(None, None, rtt);
    }

    /// Reply from `addr` for the most recent probe.
    pub fn add_rtt_from_addr(&mut self, addr: IpAddr, rtt: Duration) {
        self.record_reply(None, Some(addr), rtt);
    }

    /// Timeout for the oldest pending probe.
    pub fn add_timeout(&mut self) {
        self.record_timeout(None);
    }

    /// History index of a still-pending probe, if it is within the window.
    fn pending_slot(&self, ordinal: u64) -> Option<usize> {
        let first = (self.sent - self.history.len()) as u64;
        let slot = usize::try_from(ordinal.checked_sub(first)?).ok()?;
        (self.history.get(slot)?.outcome == PacketOutcome::Pending).then_some(slot)
    }

    fn update_primary(&mut self, addr: IpAddr) {
        let replies = |a: &IpAddr| self.responders.get(a).map_or(0, |r| r.replies);
        let switch = match self.addr {
            None => true,
            Some(primary) => primary != addr && replies(&addr) > replies(&primary),
        };
        if switch {
            self.addr = Some(addr);
            self.hostname = self.responders[&addr].hostname.clone();
            self.asn = self.responders[&addr].asn;
            self.geo = self.responders[&addr].geo.clone();
        }
    }

    fn update_rtt_stats(&mut self, rtt: Duration) {
        if let Some(prev_rtt) = self.last_rtt {
            let jitter = utils::time::calculate_timing_jitter(rtt, prev_rtt);
            self.last_jitter = Some(jitter);
            self.jitter_sum_ns += jitter.as_nanos();
            self.jitter_count += 1;
            self.jitter_avg = mean_duration(self.jitter_sum_ns, self.jitter_count);
        }

        self.last_rtt = Some(rtt);
        self.best_rtt = Some(self.best_rtt.map_or(rtt, |best| best.min(rtt)));
        self.worst_rtt = Some(self.worst_rtt.map_or(rtt, |worst| worst.max(rtt)));

        // received counts this reply already
        self.rtt_sum_ns += rtt.as_nanos();
        self.avg_rtt = mean_duration(self.rtt_sum_ns, self.received);
        let x = rtt.as_nanos() as f64;
        let delta = x - self.rtt_mean_ns;
        self.rtt_mean_ns += delta / self.received as f64;
        self.rtt_m2 += delta * (x - self.rtt_mean_ns);

        self.ema_rtt = Some(utils::time::calculate_timing_ema(
            rtt,
            self.ema_rtt,
            self.ema_alpha,
        ));
    }

    /// RTTs of the answered probes in the history window, oldest first.
    pub fn recent_rtts(&self) -> impl Iterator<Item = Duration> + '_ {
        self.history.iter().filter_map(|r| match r.outcome {
            PacketOutcome::Received(rtt) => Some(rtt),
            _ => None,
        })
    }

    /// Percentiles of the RTTs in the history window (nearest rank).
    pub fn rtt_spread(&self) -> Option<RttSpread> {
        let mut rtts: Vec<Duration> = self.recent_rtts().collect();
        if rtts.is_empty() {
            return None;
        }
        rtts.sort_unstable();
        let at = |p: f64| rtts[((p * rtts.len() as f64).ceil() as usize).clamp(1, rtts.len()) - 1];
        Some(RttSpread {
            samples: rtts.len(),
            min: rtts[0],
            p50: at(0.5),
            p90: at(0.9),
            p99: at(0.99),
            max: rtts[rtts.len() - 1],
        })
    }

    /// MPLS labels in the primary responder's latest reply.
    pub fn mpls(&self) -> &[MplsLabel] {
        self.addr
            .and_then(|addr| self.responders.get(&addr))
            .map_or(&[], |r| &r.mpls)
    }

    /// How the primary responder's location squares with its best RTT, seen
    /// from `origin`.
    pub fn assess(&self, origin: Option<&Origin>) -> Option<Assessment> {
        geo::assess(self.geo.as_ref()?, self.best_rtt, origin?)
    }

    /// Flows probed at this hop.
    pub fn flow_count(&self) -> usize {
        self.flows.len()
    }

    /// Sample standard deviation of all RTTs in milliseconds (as mtr reports it).
    pub fn stddev_ms(&self) -> f64 {
        if self.received < 2 {
            return 0.0;
        }
        (self.rtt_m2 / (self.received - 1) as f64).sqrt() / 1e6
    }

    /// Probes sent but neither answered nor timed out yet.
    pub fn in_flight(&self) -> usize {
        self.history
            .iter()
            .filter(|r| r.outcome == PacketOutcome::Pending)
            .count()
    }

    pub fn update_loss_percent(&mut self) {
        let settled = self.sent.saturating_sub(self.in_flight());
        self.loss_percent = if settled > 0 {
            settled.saturating_sub(self.received) as f64 / settled as f64 * 100.0
        } else {
            0.0
        };
    }

    /// Addresses other than the primary, most frequent first
    pub fn get_alternate_paths(&self) -> Vec<&ResponderStats> {
        let mut paths: Vec<_> = self
            .responders
            .values()
            .filter(|r| Some(r.addr) != self.addr)
            .collect();
        paths.sort_by_key(|path| std::cmp::Reverse(path.replies));
        paths
    }

    /// Check if this hop has multiple paths
    pub fn has_multiple_paths(&self) -> bool {
        self.responders.len() > 1
    }

    /// Replies across all responders
    pub fn get_total_frequency(&self) -> usize {
        self.responders.values().map(|r| r.replies).sum()
    }

    /// Share of replies that came from `path`
    pub fn get_path_percentage(&self, path: &ResponderStats) -> f64 {
        let total = self.get_total_frequency();
        if total > 0 {
            (path.replies as f64 / total as f64) * 100.0
        } else {
            0.0
        }
    }

    /// Share of replies that came from the primary responder
    pub fn get_primary_path_percentage(&self) -> f64 {
        match self.addr.and_then(|addr| self.responders.get(&addr)) {
            Some(primary) => self.get_path_percentage(primary),
            None => 100.0, // If no frequency data, assume 100%
        }
    }

    /// Set hostname for a specific address
    pub fn set_hostname_for_addr(&mut self, addr: IpAddr, hostname: String) {
        if Some(addr) == self.addr {
            self.hostname = Some(hostname.clone());
        }
        if let Some(responder) = self.responders.get_mut(&addr) {
            responder.hostname = Some(hostname);
        }
    }

    pub fn set_geo_for_addr(&mut self, addr: IpAddr, fix: GeoFix) {
        if Some(addr) == self.addr {
            self.geo = Some(fix.clone());
        }
        if let Some(responder) = self.responders.get_mut(&addr) {
            responder.geo = Some(fix);
        }
    }

    pub fn set_asn_for_addr(&mut self, addr: IpAddr, asn: u32) {
        if Some(addr) == self.addr {
            self.asn = Some(asn);
        }
        if let Some(responder) = self.responders.get_mut(&addr) {
            responder.asn = Some(asn);
        }
    }

    /// Set the exponential smoothing factor (alpha)
    /// Values closer to 1.0 make the average more responsive to recent changes
    /// Values closer to 0.0 make the average more stable and less sensitive to spikes
    /// Typical values for network monitoring: 0.1-0.3
    pub fn set_ema_alpha(&mut self, alpha: f64) {
        self.ema_alpha = utils::math::clamp_ratio(alpha);
    }

    /// Mark this hop as containing the target destination
    pub fn mark_as_target(&mut self) {
        self.is_target = true;
    }

    /// Check if this hop has returned an ICMP error (like original MTR)
    pub fn has_icmp_error(&self) -> bool {
        self.icmp_error
    }

    /// Mark this hop as having an ICMP error
    pub fn set_icmp_error(&mut self) {
        self.icmp_error = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hop_stats_new() {
        let hop = HopStats::new(5);
        assert_eq!(hop.hop, 5);
        assert_eq!(hop.sent, 0);
        assert_eq!(hop.received, 0);
        assert_eq!(hop.loss_percent, 0.0);
        assert!(hop.addr.is_none());
        assert!(hop.hostname.is_none());
        assert!(hop.last_rtt.is_none());
        assert!(hop.best_rtt.is_none());
        assert!(hop.worst_rtt.is_none());
        assert!(hop.avg_rtt.is_none());
        assert!(hop.ema_rtt.is_none());
        assert!(hop.jitter_avg.is_none());
        assert!(hop.last_jitter.is_none());
        assert!(hop.history.is_empty());
    }

    #[test]
    fn test_hop_stats_add_rtt() {
        let mut hop = HopStats::new(1);

        // Add first RTT
        let rtt1 = Duration::from_millis(100);
        hop.increment_sent(0);
        hop.add_rtt(rtt1);

        assert_eq!(hop.received, 1);
        assert_eq!(hop.last_rtt, Some(rtt1));
        assert_eq!(hop.best_rtt, Some(rtt1));
        assert_eq!(hop.worst_rtt, Some(rtt1));
        assert_eq!(hop.avg_rtt, Some(rtt1));
        assert_eq!(hop.ema_rtt, Some(rtt1)); // First value initializes EMA
        assert!(hop.jitter_avg.is_none()); // No jitter yet (need 2+ samples)
        assert!(hop.last_jitter.is_none());
        assert_eq!(hop.recent_rtts().count(), 1);

        // Add second RTT (better)
        let rtt2 = Duration::from_millis(50);
        hop.increment_sent(0);
        hop.add_rtt(rtt2);

        assert_eq!(hop.received, 2);
        assert_eq!(hop.last_rtt, Some(rtt2));
        assert_eq!(hop.best_rtt, Some(rtt2));
        assert_eq!(hop.worst_rtt, Some(rtt1));
        assert_eq!(hop.avg_rtt, Some(Duration::from_millis(75))); // Average of 100 and 50
        assert_eq!(hop.recent_rtts().count(), 2);

        // Add third RTT (worse)
        let rtt3 = Duration::from_millis(200);
        hop.increment_sent(0);
        hop.add_rtt(rtt3);

        assert_eq!(hop.received, 3);
        assert_eq!(hop.last_rtt, Some(rtt3));
        assert_eq!(hop.best_rtt, Some(rtt2));
        assert_eq!(hop.worst_rtt, Some(rtt3));
        assert_eq!(hop.recent_rtts().count(), 3);
    }

    #[test]
    fn test_hop_stats_loss_calculation() {
        let mut hop = HopStats::new(1);

        // No packets sent yet
        assert_eq!(hop.loss_percent, 0.0);

        // Send 10 packets, receive 8
        for _ in 0..8 {
            hop.increment_sent(0);
            hop.add_rtt(Duration::from_millis(100));
        }
        for _ in 0..2 {
            hop.increment_sent(0);
            hop.add_timeout();
        }

        assert_eq!(hop.sent, 10);
        assert_eq!(hop.received, 8);
        assert_eq!(hop.loss_percent, 20.0); // 2 lost out of 10 = 20%
    }

    #[test]
    fn routers_back_assumes_the_next_common_initial_ttl() {
        assert_eq!(routers_back(252), 3, "a router starting at 255");
        assert_eq!(routers_back(56), 8, "a host starting at 64");
        assert_eq!(routers_back(64), 0);
        assert_eq!(routers_back(120), 8, "Windows starts at 128");
        assert_eq!(routers_back(30), 2);
    }

    #[test]
    fn history_keeps_the_latest_probes() {
        let mut hop = HopStats::new(1);
        let total = HISTORY_LEN as u64 + 50;
        for i in 0..total {
            hop.increment_sent(0);
            hop.add_rtt(Duration::from_millis(i));
        }
        assert_eq!(hop.history.len(), HISTORY_LEN);
        assert_eq!(hop.received, total as usize);
        let rtts: Vec<Duration> = hop.recent_rtts().collect();
        assert_eq!(rtts.first(), Some(&Duration::from_millis(50)));
        assert_eq!(rtts.last(), Some(&Duration::from_millis(total - 1)));
    }

    #[test]
    fn test_hop_stats_clone() {
        let mut original = HopStats::new(3);
        original.add_rtt(Duration::from_millis(150));
        original.increment_sent(0);
        original.addr = Some("192.168.1.3".parse().unwrap());
        original.hostname = Some("gateway.local".to_string());

        let cloned = original.clone();

        assert_eq!(original.hop, cloned.hop);
        assert_eq!(original.sent, cloned.sent);
        assert_eq!(original.received, cloned.received);
        assert_eq!(original.addr, cloned.addr);
        assert_eq!(original.hostname, cloned.hostname);
        assert_eq!(original.last_rtt, cloned.last_rtt);
        assert_eq!(original.history, cloned.history);
    }

    #[test]
    fn test_hop_stats_update_loss_percent_edge_cases() {
        let mut hop = HopStats::new(1);

        // Test with no packets sent
        hop.update_loss_percent();
        assert_eq!(hop.loss_percent, 0.0);

        // Test with 100% loss
        hop.sent = 5;
        hop.received = 0;
        hop.update_loss_percent();
        assert_eq!(hop.loss_percent, 100.0);

        // Test with 0% loss
        hop.received = 5;
        hop.update_loss_percent();
        assert_eq!(hop.loss_percent, 0.0);

        // Test with partial loss
        hop.sent = 10;
        hop.received = 7;
        hop.update_loss_percent();
        assert_eq!(hop.loss_percent, 30.0);
    }

    #[test]
    fn test_hop_stats_increment_sent() {
        let mut hop = HopStats::new(1);

        assert_eq!(hop.sent, 0);
        assert_eq!(hop.loss_percent, 0.0);

        hop.increment_sent(0);
        assert_eq!(hop.sent, 1);
        assert_eq!(hop.loss_percent, 0.0); // still in flight, so not lost (as in mtr)
        assert_eq!(hop.in_flight(), 1);

        hop.add_rtt(Duration::from_millis(100)); // This also calls increment_sent internally
        assert_eq!(hop.sent, 1); // Should still be 1 since add_rtt doesn't increment sent
        assert_eq!(hop.received, 1);
        assert_eq!(hop.loss_percent, 0.0); // 1 sent, 1 received = 0% loss
    }

    #[test]
    fn test_large_rtt_values() {
        let mut hop = HopStats::new(1);

        // Test with very large RTT values
        let large_rtt = Duration::from_secs(5); // 5 seconds
        hop.add_rtt(large_rtt);

        assert_eq!(hop.best_rtt, Some(large_rtt));
        assert_eq!(hop.worst_rtt, Some(large_rtt));
        assert_eq!(hop.avg_rtt, Some(large_rtt));

        // Add a smaller RTT
        let small_rtt = Duration::from_millis(10);
        hop.add_rtt(small_rtt);

        assert_eq!(hop.best_rtt, Some(small_rtt));
        assert_eq!(hop.worst_rtt, Some(large_rtt));
    }

    #[test]
    fn test_hop_stats_add_timeout() {
        let mut hop = HopStats::new(1);

        // Add some successful RTTs first
        hop.increment_sent(0);
        hop.add_rtt(Duration::from_millis(100));
        hop.increment_sent(0);
        hop.add_rtt(Duration::from_millis(150));

        assert_eq!(hop.sent, 2);
        assert_eq!(hop.received, 2);
        assert_eq!(hop.loss_percent, 0.0);

        // Add timeout
        hop.increment_sent(0);
        hop.add_timeout();

        assert_eq!(hop.sent, 3);
        assert_eq!(hop.received, 2);
        assert!((hop.loss_percent - 33.333333333333336).abs() < 1e-10); // 1 lost out of 3
    }

    #[test]
    fn test_rtt_statistics_accuracy() {
        let mut hop = HopStats::new(1);

        let rtts = vec![50, 100, 75, 200, 25]; // in milliseconds

        for &rtt_ms in &rtts {
            hop.add_rtt(Duration::from_millis(rtt_ms));
        }

        assert_eq!(hop.best_rtt, Some(Duration::from_millis(25)));
        assert_eq!(hop.worst_rtt, Some(Duration::from_millis(200)));
        assert_eq!(hop.last_rtt, Some(Duration::from_millis(25))); // Last added

        // Average should be (50 + 100 + 75 + 200 + 25) / 5 = 90
        assert_eq!(hop.avg_rtt, Some(Duration::from_millis(90)));
    }

    #[test]
    fn test_exponential_moving_average() {
        let mut hop = HopStats::new(1);
        hop.set_ema_alpha(0.5); // Use 0.5 for easier testing (50% weight to new values)

        // First RTT should initialize EMA
        hop.add_rtt(Duration::from_millis(100));
        assert_eq!(hop.ema_rtt, Some(Duration::from_millis(100)));

        // Second RTT: EMA = 0.5 * 200 + 0.5 * 100 = 150
        hop.add_rtt(Duration::from_millis(200));
        let ema_ms = hop.ema_rtt.unwrap().as_millis() as u64;
        assert_eq!(ema_ms, 150);

        // Third RTT: EMA = 0.5 * 100 + 0.5 * 150 = 125
        hop.add_rtt(Duration::from_millis(100));
        let ema_ms = hop.ema_rtt.unwrap().as_millis() as u64;
        assert_eq!(ema_ms, 125);
    }

    #[test]
    fn test_ema_alpha_clamping() {
        let mut hop = HopStats::new(1);

        // Test values outside valid range are clamped
        hop.set_ema_alpha(-0.5);
        assert_eq!(hop.ema_alpha, 0.0);

        hop.set_ema_alpha(1.5);
        assert_eq!(hop.ema_alpha, 1.0);

        hop.set_ema_alpha(0.3);
        assert_eq!(hop.ema_alpha, 0.3);
    }

    #[test]
    fn test_jitter_calculation() {
        let mut hop = HopStats::new(1);

        // First RTT - no jitter yet
        hop.add_rtt(Duration::from_millis(100));
        assert!(hop.jitter_avg.is_none());
        assert!(hop.last_jitter.is_none());

        // Second RTT - first jitter calculation
        hop.add_rtt(Duration::from_millis(120));
        assert_eq!(hop.last_jitter, Some(Duration::from_millis(20))); // |120 - 100| = 20
        assert_eq!(hop.jitter_avg, Some(Duration::from_millis(20))); // Only one jitter value

        // Third RTT - jitter decreases
        hop.add_rtt(Duration::from_millis(110));
        assert_eq!(hop.last_jitter, Some(Duration::from_millis(10))); // |110 - 120| = 10
        let expected_avg = (20 + 10) / 2; // Average of 20ms and 10ms = 15ms
        assert_eq!(hop.jitter_avg, Some(Duration::from_millis(expected_avg)));

        // Fourth RTT - larger jitter spike
        hop.add_rtt(Duration::from_millis(150));
        assert_eq!(hop.last_jitter, Some(Duration::from_millis(40))); // |150 - 110| = 40
                                                                      // Average of jitter values: (20 + 10 + 40) / 3 = 23.333... ms
        let expected_avg_ms = hop.jitter_avg.unwrap().as_millis() as u64;
        assert_eq!(expected_avg_ms, 23); // Rounded to nearest ms
    }

    fn addr(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, last])
    }

    #[test]
    fn replies_land_in_their_own_slot() {
        let mut hop = HopStats::new(1);
        let first = hop.increment_sent(0);
        let second = hop.increment_sent(0);
        // Reply to the older probe arrives while the newer one is in flight
        hop.record_reply(Some(first), Some(addr(1)), Duration::from_millis(10));
        assert!(matches!(hop.history[0].outcome, PacketOutcome::Received(_)));
        assert_eq!(hop.history[0].from, Some(addr(1)));
        assert_eq!(hop.history[1].outcome, PacketOutcome::Pending);
        assert_eq!(hop.loss_percent, 0.0);

        hop.record_timeout(Some(second));
        assert_eq!(hop.history[1].outcome, PacketOutcome::Lost);
        assert_eq!(hop.loss_percent, 50.0);

        // A late reply after the timeout and a duplicate are both ignored
        hop.record_reply(Some(second), Some(addr(1)), Duration::from_millis(900));
        hop.record_reply(Some(first), Some(addr(1)), Duration::from_millis(10));
        assert_eq!(hop.received, 1);
        assert_eq!(hop.worst_rtt, Some(Duration::from_millis(10)));
    }

    #[test]
    fn stale_ordinal_after_reset_is_ignored() {
        let mut hop = HopStats::new(1);
        hop.record_reply(Some(57), Some(addr(1)), Duration::from_millis(10));
        assert_eq!(hop.received, 0);
        assert!(hop.addr.is_none());
    }

    #[test]
    fn averages_cover_all_replies_not_just_the_window() {
        let mut hop = HopStats::new(1);
        let (slow, fast) = (50, HISTORY_LEN);
        for _ in 0..slow {
            hop.increment_sent(0);
            hop.add_rtt(Duration::from_millis(1000));
        }
        for _ in 0..fast {
            hop.increment_sent(0);
            hop.add_rtt(Duration::from_millis(10));
        }
        assert!(
            hop.recent_rtts()
                .all(|rtt| rtt == Duration::from_millis(10)),
            "the window only holds the latest probes"
        );
        let n = (slow + fast) as f64;
        let mean = (slow as f64 * 1000.0 + fast as f64 * 10.0) / n;
        let avg_ms = hop.avg_rtt.unwrap().as_secs_f64() * 1000.0;
        assert!((avg_ms - mean).abs() < 1e-3, "{avg_ms} vs {mean}");
        let expected_sd = {
            let ss =
                slow as f64 * (1000.0f64 - mean).powi(2) + fast as f64 * (10.0f64 - mean).powi(2);
            (ss / (n - 1.0)).sqrt()
        };
        assert!(
            (hop.stddev_ms() - expected_sd).abs() < 1e-6,
            "{}",
            hop.stddev_ms()
        );
    }

    #[test]
    fn even_split_between_two_responders_is_50_50() {
        let mut hop = HopStats::new(3);
        for i in 0..6 {
            hop.increment_sent(0);
            hop.add_rtt_from_addr(
                addr(if i % 2 == 0 { 1 } else { 2 }),
                Duration::from_millis(10),
            );
        }
        let alternates = hop.get_alternate_paths();
        assert_eq!(alternates.len(), 1);
        assert_eq!(hop.get_primary_path_percentage(), 50.0);
        assert_eq!(hop.get_path_percentage(alternates[0]), 50.0);
        assert_eq!(hop.recent_rtts().count(), 6);
    }

    #[test]
    fn primary_switch_keeps_old_primary_as_alternate() {
        let mut hop = HopStats::new(3);
        hop.add_rtt_from_addr(addr(1), Duration::from_millis(10));
        hop.set_hostname_for_addr(addr(1), "router-a.example".into());
        hop.add_rtt_from_addr(addr(2), Duration::from_millis(20));
        hop.add_rtt_from_addr(addr(2), Duration::from_millis(20));

        assert_eq!(hop.addr, Some(addr(2)));
        assert_eq!(hop.hostname, None, "B must not show A's name");
        let alternates = hop.get_alternate_paths();
        assert_eq!(alternates.len(), 1);
        assert_eq!(alternates[0].addr, addr(1));
        assert_eq!(alternates[0].hostname.as_deref(), Some("router-a.example"));
        let primary = hop.get_primary_path_percentage();
        assert!((primary - 200.0 / 3.0).abs() < 1e-9, "{primary}");
    }
}
