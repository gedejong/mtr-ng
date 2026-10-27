use crate::probe::{IcmpResponseType, ProbeEngine, ProbeResponse, Prober};
use crate::{tracer, utils, Args, HopStats, Result};
use anyhow::{anyhow, bail};
use hickory_resolver::{
    config::{LookupIpStrategy, ResolverConfig, CLOUDFLARE},
    net::runtime::TokioRuntimeProvider,
    TokioResolver,
};
use rand;

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time;
use tracing::{debug, info, warn};

/// Rounds in report mode when `--count` is not given (same as mtr).
pub const DEFAULT_REPORT_CYCLES: usize = 10;

/// Length of the fake path produced by `--simulate`.
const SIMULATED_PATH_LENGTH: usize = 8;

// Add callback type for real-time updates
pub type UpdateCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct MtrSession {
    pub target: String,
    pub target_addr: IpAddr,
    pub hops: Vec<HopStats>,
    pub args: Args,
    pub resolver: TokioResolver,
    pub num_hosts: usize, // TTLs probed each round (trimmed once the path end is known)
    pub update_callback: Option<UpdateCallback>, // callback for real-time updates
    /// How long a probe may stay unanswered before it counts as lost
    pub probe_timeout: Duration,
    /// Reverse DNS results per address; `None` while pending or when there is no name
    dns_names: HashMap<IpAddr, Option<String>>,
    /// Origin AS per address, with `--aslookup`; `None` while pending or unknown
    asns: HashMap<IpAddr, Option<u32>>,
}

impl MtrSession {
    pub async fn new(args: Args) -> Result<Self> {
        // The system resolver honours /etc/hosts, VPN and split-horizon DNS and keeps
        // private PTR lookups inside the network.
        let mut builder = TokioResolver::builder_tokio().unwrap_or_else(|e| {
            warn!("Cannot read the system DNS configuration ({e}); using public resolvers");
            TokioResolver::builder_with_config(
                ResolverConfig::udp_and_tcp(&CLOUDFLARE),
                TokioRuntimeProvider::default(),
            )
        });
        // Only IPv4 can be probed so far, and hickory 0.26 puts AAAA first by default
        builder.options_mut().ip_strategy = LookupIpStrategy::Ipv4thenIpv6;
        let resolver = builder.build()?;

        // Resolve target hostname to IP
        let target_addr = if let Ok(ip) = args.target.parse::<IpAddr>() {
            ip
        } else {
            let target = &args.target;
            let response = resolver.lookup_ip(target).await.map_err(|e| {
                if e.is_nx_domain() || e.is_no_records_found() {
                    anyhow!("{target}: no such host")
                } else {
                    anyhow!("cannot resolve {target}: {e}")
                }
            })?;
            response
                .iter()
                .next()
                .ok_or_else(|| anyhow!("{target}: no such host"))?
        };

        let mut hops: Vec<HopStats> = (1..=args.max_hops).map(HopStats::new).collect();

        // Configure EMA alpha for all hops from command line args
        for hop in &mut hops {
            hop.set_ema_alpha(args.ema_alpha);
        }
        let num_hosts = hops.len();

        Ok(Self {
            target: args.target.clone(),
            target_addr,
            hops,
            args,
            resolver,
            num_hosts, // probe every TTL until the end of the path is known
            update_callback: None,
            probe_timeout: tracer::PROBE_TIMEOUT,
            dns_names: HashMap::new(),
            asns: HashMap::new(),
        })
    }

    pub fn is_simulated(&self) -> bool {
        self.args.simulate
    }

    /// Open the prober for a real trace, or return `None` in simulation mode.
    /// Fails instead of falling back to simulated data when real probing is impossible.
    pub fn open_probe_engine(&self) -> Result<Option<Box<dyn Prober>>> {
        if self.is_simulated() {
            return Ok(None);
        }
        if self.target_addr.is_ipv6() {
            bail!(
                "IPv6 targets are not supported yet ({} resolved to {}). \
                 Use an IPv4 address, or --simulate for demo data.",
                self.target,
                self.target_addr
            );
        }
        Ok(Some(Box::new(ProbeEngine::new()?)))
    }

    /// Hops worth showing: up to the end of the path once it is known, otherwise
    /// up to the furthest hop that has answered.
    pub fn display_hop_count(&self) -> usize {
        match self.hops.iter().position(|hop| hop.is_target) {
            Some(end) => end + 1,
            None => self
                .hops
                .iter()
                .rposition(|hop| hop.addr.is_some())
                .map_or(0, |i| i + 1),
        }
    }

    /// Record a probe response in the hop table. An echo reply or Destination
    /// Unreachable marks the end of the path; higher TTLs are no longer probed.
    /// Returns false if the response refers to a hop outside the table.
    pub fn apply_response(&mut self, response: &ProbeResponse) -> bool {
        let index = response.hop;
        let Some(hop) = self.hops.get_mut(index) else {
            return false;
        };
        let addr = response.source_addr;
        match response.icmp_type {
            IcmpResponseType::TimeExceeded => {
                hop.record_reply(response.ordinal, Some(addr), response.rtt);
            }
            IcmpResponseType::EchoReply | IcmpResponseType::DestinationUnreachable => {
                hop.record_reply(response.ordinal, Some(addr), response.rtt);
                if response.icmp_type == IcmpResponseType::DestinationUnreachable {
                    hop.set_icmp_error();
                }
                hop.mark_as_target();
                if index + 1 < self.num_hosts {
                    info!("Path ends at hop {} ({})", index + 1, addr);
                    self.num_hosts = index + 1;
                }
            }
            IcmpResponseType::Timeout => {
                hop.record_timeout(response.ordinal);
                return true;
            }
        }
        if let Some(Some(name)) = self.dns_names.get(&addr) {
            self.hops[index].set_hostname_for_addr(addr, name.clone());
        }
        if let Some(Some(asn)) = self.asns.get(&addr) {
            self.hops[index].set_asn_for_addr(addr, *asn);
        }
        true
    }

    /// True the first time an address is seen: the caller should look it up.
    pub fn want_lookup(&mut self, addr: IpAddr) -> bool {
        if self.dns_names.contains_key(&addr) {
            return false;
        }
        self.dns_names.insert(addr, None);
        true
    }

    /// Store a reverse DNS result and show it on every hop where `addr` answered.
    pub fn set_hostname(&mut self, addr: IpAddr, name: Option<String>) {
        if let Some(name) = &name {
            debug!("Resolved {} to {}", addr, name);
            for hop in &mut self.hops {
                if hop.responders.contains_key(&addr) {
                    hop.set_hostname_for_addr(addr, name.clone());
                }
            }
        }
        self.dns_names.insert(addr, name);
    }

    /// Start the statistics over, keeping what is known about the path itself.
    pub fn reset_statistics(&mut self) {
        for hop in &mut self.hops {
            let mut fresh = HopStats::new(hop.hop);
            fresh.set_ema_alpha(hop.ema_alpha);
            fresh.is_target = hop.is_target;
            *hop = fresh;
        }
    }

    /// True the first time a public address is seen with `--aslookup`: the caller
    /// should look up its AS. Private and special addresses are never sent out.
    pub fn want_asn(&mut self, addr: IpAddr) -> bool {
        if !self.args.wants_asn() || !crate::asn::is_public(addr) || self.asns.contains_key(&addr) {
            return false;
        }
        self.asns.insert(addr, None);
        true
    }

    /// Store an AS lookup result and show it on every hop where `addr` answered.
    pub fn set_asn(&mut self, addr: IpAddr, asn: Option<u32>) {
        if let Some(asn) = asn {
            for hop in &mut self.hops {
                if hop.responders.contains_key(&addr) {
                    hop.set_asn_for_addr(addr, asn);
                }
            }
        }
        self.asns.insert(addr, asn);
    }

    /// Tell the UI that something changed.
    pub fn notify(&self) {
        if let Some(ref callback) = self.update_callback {
            callback();
        }
    }

    /// Simulated trace for report mode: lockstep rounds, no real sockets.
    pub async fn run_simulated_trace(&mut self) -> Result<()> {
        info!("Running simulated traceroute (use sudo for real network tracing)");

        if let Some(end) = self.hops.get_mut(SIMULATED_PATH_LENGTH - 1) {
            end.mark_as_target();
        }
        self.num_hosts = self.num_hosts.min(SIMULATED_PATH_LENGTH);

        for round in 0..self.args.count.unwrap_or(DEFAULT_REPORT_CYCLES) {
            debug!("Simulation Round {}", round + 1);

            for hop in self.hops.iter_mut().take(SIMULATED_PATH_LENGTH) {
                hop.increment_sent();

                // Simulate realistic network behavior
                let base_latency = hop.hop as u64 * 10 + 20; // Base latency increases with hops
                let jitter = rand::random::<u64>() % 50; // Random jitter
                let packet_loss_chance = utils::math::clamp_f64(hop.hop as f64 * 0.05, 0.0, 0.25); // Higher loss chance for testing

                if rand::random::<f64>() > packet_loss_chance {
                    let rtt = Duration::from_millis(base_latency + jitter);
                    hop.add_rtt(rtt);

                    // Simulate realistic IP addresses and hostnames
                    if hop.addr.is_none() {
                        // Generate realistic-looking IP addresses
                        match hop.hop {
                            1 => {
                                hop.addr = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
                                hop.hostname = if !self.args.numeric {
                                    Some("gateway.local".to_string())
                                } else {
                                    None
                                };
                            }
                            2..=3 => {
                                hop.addr = Some(IpAddr::V4(Ipv4Addr::new(10, 0, hop.hop, 1)));
                                hop.hostname = if !self.args.numeric {
                                    Some(format!("core-{}.isp.net", hop.hop))
                                } else {
                                    None
                                };
                            }
                            _ => {
                                let final_octet = if hop.hop >= 8 { 8 } else { hop.hop };
                                hop.addr = Some(IpAddr::V4(Ipv4Addr::new(8, 8, 8, final_octet)));
                                hop.hostname = if !self.args.numeric {
                                    Some("dns.google".to_string())
                                } else {
                                    None
                                };
                            }
                        }
                    }
                } else {
                    hop.add_timeout();
                }
            }

            time::sleep(Duration::from_millis(self.args.interval)).await;
        }

        Ok(())
    }

    pub fn set_update_callback(&mut self, callback: UpdateCallback) {
        self.update_callback = Some(callback);
    }

    /// Run the trace with live updates for the TUI. `prober` comes from
    /// `open_probe_engine`; `None` means simulation.
    pub async fn run_trace_with_realtime_updates(
        session_arc: Arc<Mutex<Self>>,
        prober: Option<Box<dyn Prober>>,
    ) -> Result<()> {
        let args = session_arc.lock().unwrap().args.clone();
        match prober {
            Some(prober) => tracer::run(session_arc, prober).await,
            None => Self::run_simulated_trace_realtime(session_arc, args).await,
        }
    }

    async fn run_simulated_trace_realtime(
        session_arc: std::sync::Arc<std::sync::Mutex<Self>>,
        args: Args,
    ) -> Result<()> {
        info!("Running simulated traceroute (real-time individual packet responses)");

        // Extract the numeric flag once to avoid borrow conflicts
        let numeric = args.numeric;
        let max_hops = {
            let mut session = session_arc.lock().unwrap();
            if let Some(end) = session.hops.get_mut(SIMULATED_PATH_LENGTH - 1) {
                end.mark_as_target();
            }
            session.num_hosts = session.num_hosts.min(SIMULATED_PATH_LENGTH);
            session.num_hosts
        };

        for round in 0..args.count.unwrap_or(1000) {
            debug!(
                "Simulation Round {} (interval: {}ms)",
                round + 1,
                args.interval
            );
            let round_start = tokio::time::Instant::now();

            // PHASE 1: Send all packets immediately (shows waiting state)
            {
                let mut session = session_arc.lock().unwrap();
                for hop_index in 0..max_hops {
                    session.hops[hop_index].increment_sent();
                }

                // Trigger UI update to show all hops in waiting state
                if let Some(ref callback) = session.update_callback {
                    callback();
                }
            }

            // PHASE 2: Simulate responses arriving individually with realistic delays
            for hop_index in 0..max_hops {
                // Simulate network transit time for this hop
                let base_transit_time = (hop_index + 1) as u64 * 15 + 10; // 25ms, 40ms, 55ms, etc.
                let jitter = rand::random::<u64>() % 30; // 0-30ms jitter
                let transit_time = Duration::from_millis(base_transit_time + jitter);

                // Wait for the simulated transit time
                tokio::time::sleep(transit_time).await;

                let should_update_ui = {
                    let mut session = session_arc.lock().unwrap();
                    let hop = &mut session.hops[hop_index];

                    // Don't increment sent again - already done in phase 1

                    let base_latency = (hop_index + 1) as u64 * 15 + 20; // Realistic latency progression
                    let rtt_jitter = rand::random::<u64>() % 20;
                    let packet_loss_chance =
                        utils::math::clamp_f64(hop_index as f64 * 0.03, 0.0, 0.20);

                    if rand::random::<f64>() > packet_loss_chance {
                        let rtt = Duration::from_millis(base_latency + rtt_jitter);
                        hop.add_rtt(rtt);

                        if hop.addr.is_none() {
                            match hop.hop {
                                1 => {
                                    hop.addr = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
                                    hop.hostname = if !numeric {
                                        Some("gateway.local".to_string())
                                    } else {
                                        None
                                    };
                                }
                                2..=3 => {
                                    hop.addr = Some(IpAddr::V4(Ipv4Addr::new(10, 0, hop.hop, 1)));
                                    hop.hostname = if !numeric {
                                        Some(format!("core-{}.isp.net", hop.hop))
                                    } else {
                                        None
                                    };
                                }
                                _ => {
                                    let final_octet = if hop.hop >= 8 { 8 } else { hop.hop };
                                    hop.addr =
                                        Some(IpAddr::V4(Ipv4Addr::new(8, 8, 8, final_octet)));
                                    hop.hostname = if !numeric {
                                        Some("dns.google".to_string())
                                    } else {
                                        None
                                    };
                                }
                            }
                        }
                    } else {
                        hop.add_timeout();
                    }

                    session.update_callback.is_some()
                };

                // Trigger UI update immediately when this packet response "arrives"
                if should_update_ui {
                    let session = session_arc.lock().unwrap();
                    if let Some(ref callback) = session.update_callback {
                        callback();
                    }
                }
            }

            // Wait for the remainder of the interval before starting the next round
            // This maintains the specified interval timing while showing individual responses
            let elapsed = round_start.elapsed();
            let interval_duration = Duration::from_millis(args.interval);
            if elapsed < interval_duration {
                let remaining = interval_duration - elapsed;
                debug!(
                    "Round {} completed in {:?}, waiting {:?} more",
                    round + 1,
                    elapsed,
                    remaining
                );
                tokio::time::sleep(remaining).await;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[tokio::test]
    async fn test_mtr_session_new_with_ip() {
        let args = Args {
            target: "192.168.1.1".to_string(),
            count: Some(5),
            interval: 500,
            max_hops: 20,
            report: false,
            numeric: true,
            sparkline_scale: crate::SparklineScale::Logarithmic,
            ema_alpha: 0.1,
            fields: None,
            show_all: false,
            simulate: false,
            protocol: crate::args::ProbeProtocol::Icmp,
            quiet: false,
            aslookup: false,
            json: false,
            csv: false,
            log_file: None,
        };

        let session = MtrSession::new(args).await;
        assert!(session.is_ok());

        let session = session.unwrap();
        assert_eq!(session.target, "192.168.1.1");
        assert_eq!(session.target_addr.to_string(), "192.168.1.1");
        assert_eq!(session.hops.len(), 20);
        assert_eq!(session.args.count, Some(5));
        assert_eq!(session.args.interval, 500);
    }

    #[tokio::test]
    async fn test_mtr_session_new_with_localhost() {
        let args = Args {
            target: "localhost".to_string(),
            count: Some(3),
            interval: 1000,
            max_hops: 15,
            report: true,
            numeric: false,
            sparkline_scale: crate::SparklineScale::Logarithmic,
            ema_alpha: 0.1,
            fields: None,
            show_all: false,
            simulate: false,
            protocol: crate::args::ProbeProtocol::Icmp,
            quiet: false,
            aslookup: false,
            json: false,
            csv: false,
            log_file: None,
        };

        let session = MtrSession::new(args).await;
        assert!(session.is_ok());

        let session = session.unwrap();
        assert_eq!(session.target, "localhost");
        assert_eq!(session.hops.len(), 15);
        assert!(session.args.report);
        assert!(!session.args.numeric);
    }

    fn response(hop: usize, kind: IcmpResponseType, from: [u8; 4]) -> ProbeResponse {
        let now = std::time::Instant::now();
        ProbeResponse {
            hop,
            ordinal: None,
            seq: 0,
            source_addr: IpAddr::V4(Ipv4Addr::from(from)),
            icmp_type: kind,
            rtt: Duration::from_millis(10),
            send_time: now,
            receive_time: now,
            precise_rtt_ns: 10_000_000,
        }
    }

    async fn session_for(cli: &[&str]) -> MtrSession {
        let args = Args::parse_from(std::iter::once("mtr-ng").chain(cli.iter().copied()));
        MtrSession::new(args).await.unwrap()
    }

    #[tokio::test]
    async fn path_ends_at_first_echo_reply() {
        let mut session = session_for(&["-n", "1.1.1.1"]).await;
        assert_eq!(
            session.num_hosts, 30,
            "probe every TTL until the path end is known"
        );

        session.apply_response(&response(
            0,
            IcmpResponseType::TimeExceeded,
            [192, 168, 1, 1],
        ));
        session.apply_response(&response(2, IcmpResponseType::TimeExceeded, [10, 0, 0, 3]));
        assert_eq!(session.display_hop_count(), 3, "furthest hop that answered");

        // Higher TTLs also reach the target; the lowest one is the real distance
        session.apply_response(&response(9, IcmpResponseType::EchoReply, [1, 1, 1, 1]));
        session.apply_response(&response(6, IcmpResponseType::EchoReply, [1, 1, 1, 1]));
        assert_eq!(session.num_hosts, 7);
        assert_eq!(session.display_hop_count(), 7);
    }

    #[tokio::test]
    async fn destination_unreachable_ends_path() {
        let mut session = session_for(&["-n", "1.1.1.1"]).await;
        session.apply_response(&response(
            4,
            IcmpResponseType::DestinationUnreachable,
            [10, 0, 0, 5],
        ));
        assert_eq!(session.num_hosts, 5);
        assert_eq!(session.display_hop_count(), 5);
        assert!(session.hops[4].has_icmp_error());
        assert_eq!(
            session.hops[4].received, 1,
            "an unreachable reply is still a reply"
        );
    }

    #[tokio::test]
    async fn out_of_range_response_is_ignored() {
        let mut session = session_for(&["-n", "-M", "5", "1.1.1.1"]).await;
        assert!(!session.apply_response(&response(7, IcmpResponseType::EchoReply, [1, 1, 1, 1])));
        assert_eq!(session.num_hosts, 5);
    }

    #[tokio::test]
    async fn ipv6_target_fails_instead_of_simulating() {
        let session = session_for(&["-n", "::1"]).await;
        let err = session
            .open_probe_engine()
            .err()
            .expect("IPv6 must not fall back to simulation");
        assert!(err.to_string().contains("IPv6"));
    }

    #[tokio::test]
    async fn simulation_only_when_asked() {
        let session = session_for(&["--simulate", "::1"]).await;
        assert!(session.is_simulated());
        assert!(session.open_probe_engine().unwrap().is_none());
    }

    #[test]
    fn test_mtr_session_clone() {
        let args = Args {
            target: "example.com".to_string(),
            count: Some(10),
            interval: 1000,
            max_hops: 30,
            report: false,
            numeric: false,
            sparkline_scale: crate::SparklineScale::Logarithmic,
            ema_alpha: 0.1,
            fields: None,
            show_all: false,
            simulate: false,
            protocol: crate::args::ProbeProtocol::Icmp,
            quiet: false,
            aslookup: false,
            json: false,
            csv: false,
            log_file: None,
        };

        // We can't easily test MtrSession::new in sync context due to async resolver,
        // but we can test that the struct supports Clone
        // This is mainly a compilation test
        let args_clone = args.clone();
        assert_eq!(args.target, args_clone.target);
        assert_eq!(args.count, args_clone.count);
    }
}
