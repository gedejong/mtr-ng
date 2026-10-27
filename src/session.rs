use crate::probe::{IcmpResponseType, ProbeEngine, ProbeResponse};
use crate::{utils, Args, HopStats, Result};
use anyhow::{anyhow, bail};
use hickory_resolver::{
    config::{ResolverConfig, ResolverOpts},
    TokioAsyncResolver,
};
use rand;

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, info};

const MIN_SEQUENCE: u16 = 33000;
const MAX_SEQUENCE: u16 = 65535;

/// Rounds in report mode when `--count` is not given (same as mtr).
pub const DEFAULT_REPORT_CYCLES: usize = 10;

/// Length of the fake path produced by `--simulate`.
const SIMULATED_PATH_LENGTH: usize = 8;

// Add callback type for real-time updates
pub type UpdateCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone)]
pub struct SequenceEntry {
    pub index: usize,       // hop index (like original mtr)
    pub transit: bool,      // is this sequence in transit?
    pub saved_seq: u32,     // saved sequence for this host
    pub send_time: Instant, // when packet was sent
}

#[derive(Clone)]
pub struct MtrSession {
    pub target: String,
    pub target_addr: IpAddr,
    pub hops: Vec<HopStats>,
    pub args: Args,
    pub resolver: TokioAsyncResolver,
    pub packet_id: u16,
    pub next_sequence: u16,
    pub sequence_table: HashMap<u16, SequenceEntry>, // sequence -> entry (like original mtr)
    pub batch_at: usize,  // current hop index being sent (like original mtr)
    pub num_hosts: usize, // number of active hops
    pub update_callback: Option<UpdateCallback>, // callback for real-time updates
}

impl MtrSession {
    pub async fn new(args: Args) -> Result<Self> {
        let resolver =
            TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default());

        // Resolve target hostname to IP
        let target_addr = if let Ok(ip) = args.target.parse::<IpAddr>() {
            ip
        } else {
            let response = resolver.lookup_ip(&args.target).await?;
            response
                .iter()
                .next()
                .ok_or_else(|| anyhow!("Failed to resolve hostname"))?
        };

        let mut hops: Vec<HopStats> = (1..=args.max_hops).map(HopStats::new).collect();

        // Configure EMA alpha for all hops from command line args
        for hop in &mut hops {
            hop.set_ema_alpha(args.ema_alpha);
        }
        let packet_id = std::process::id() as u16;
        let num_hosts = hops.len();

        Ok(Self {
            target: args.target.clone(),
            target_addr,
            hops,
            args,
            resolver,
            packet_id,
            next_sequence: MIN_SEQUENCE,
            sequence_table: HashMap::new(),
            batch_at: 0, // Start at hop 1 (index 0)
            num_hosts,   // probe every TTL until the end of the path is known
            update_callback: None,
        })
    }

    pub fn is_simulated(&self) -> bool {
        self.args.simulate || self.args.force_simulate
    }

    /// Open the probe engine for a real trace, or return `None` in simulation mode.
    /// Fails instead of falling back to simulated data when real probing is impossible.
    pub fn open_probe_engine(&self) -> Result<Option<ProbeEngine>> {
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
        ProbeEngine::new().map(Some)
    }

    /// Run the trace to completion (report mode).
    pub async fn run_trace(&mut self, probe_engine: Option<ProbeEngine>) -> Result<()> {
        info!("Starting trace to {} ({})", self.target, self.target_addr);
        let Some(probe_engine) = probe_engine else {
            return self.run_simulated_trace().await;
        };
        let IpAddr::V4(target) = self.target_addr else {
            bail!("IPv6 targets are not supported yet");
        };
        self.run_mtr_algorithm_with_probe_engine(target, probe_engine)
            .await
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
    fn apply_response(&mut self, response: &ProbeResponse) -> bool {
        let index = response.hop;
        let Some(hop) = self.hops.get_mut(index) else {
            return false;
        };
        match response.icmp_type {
            IcmpResponseType::TimeExceeded => {
                hop.add_rtt_from_addr(response.source_addr, response.rtt);
            }
            IcmpResponseType::EchoReply | IcmpResponseType::DestinationUnreachable => {
                hop.add_rtt_from_addr(response.source_addr, response.rtt);
                if response.icmp_type == IcmpResponseType::DestinationUnreachable {
                    hop.set_icmp_error();
                }
                hop.mark_as_target();
                if index + 1 < self.num_hosts {
                    info!("Path ends at hop {} ({})", index + 1, response.source_addr);
                    self.num_hosts = index + 1;
                }
            }
            IcmpResponseType::Timeout => debug!("Timeout for hop {}", index + 1),
        }
        true
    }

    // Modern ProbeEngine implementation
    async fn run_mtr_algorithm_with_probe_engine(
        &mut self,
        target: Ipv4Addr,
        mut probe_engine: ProbeEngine,
    ) -> Result<()> {
        info!("Starting MTR algorithm with ProbeEngine");
        let rounds = self.args.count.unwrap_or(DEFAULT_REPORT_CYCLES);
        let mut round = 0;

        while round < rounds {
            let round_start = Instant::now();

            // Send probes for all active hops (like net_send_batch)
            let restart = self
                .net_send_batch_with_probe_engine(target, &mut probe_engine)
                .await?;

            // Collect responses efficiently
            let collect_duration = Duration::from_millis(self.args.interval);
            self.net_process_return_with_probe_engine(&mut probe_engine, collect_duration)
                .await;

            if restart {
                round += 1;
                debug!("Completed round {}/{}, restarting batch", round, rounds);

                // Only wait for remaining interval time if we're not done
                if round < rounds {
                    let elapsed = round_start.elapsed();
                    let target_interval = Duration::from_millis(self.args.interval);
                    if elapsed < target_interval {
                        tokio::time::sleep(target_interval - elapsed).await;
                    }
                }
            }
        }

        Ok(())
    }

    // ProbeEngine-based equivalent of net_send_batch - send to all hops in parallel
    async fn net_send_batch_with_probe_engine(
        &mut self,
        target: Ipv4Addr,
        probe_engine: &mut ProbeEngine,
    ) -> Result<bool> {
        // Send to every TTL up to the end of the path (all of them until it is known)
        for i in 0..self.num_hosts {
            self.net_send_query_with_probe_engine(target, probe_engine, i)?;
        }

        // Always restart after sending batch (that's how MTR works)
        Ok(true)
    }

    // ProbeEngine-based equivalent of net_send_query
    fn net_send_query_with_probe_engine(
        &mut self,
        target: Ipv4Addr,
        probe_engine: &mut ProbeEngine,
        index: usize,
    ) -> Result<()> {
        let time_to_live = (index + 1) as u8;
        let seq = self.prepare_sequence(index);
        let send_time = Instant::now();

        self.save_sequence_with_send_time(index, seq, send_time);

        let target_addr = std::net::SocketAddr::from((target, 0));
        let timeout = Duration::from_millis(200); // Short timeout per individual probe (like original MTR)

        probe_engine.send_probe(index, target_addr, time_to_live, timeout)?;

        debug!(
            "Sent probe to hop {} (TTL={}), seq={}",
            index + 1,
            time_to_live,
            seq
        );
        Ok(())
    }

    // Event-driven response collection (no polling!)
    async fn net_process_return_with_probe_engine(
        &mut self,
        probe_engine: &mut ProbeEngine,
        _collect_duration: Duration,
    ) {
        let start_collect = Instant::now();
        let max_wait = Duration::from_millis(50);
        let mut total_responses = 0;

        // Use tokio::select for event-driven I/O instead of polling
        loop {
            if start_collect.elapsed() >= max_wait {
                break;
            }

            tokio::select! {
                // Event-driven response collection
                result = probe_engine.collect_responses_async() => {
                    match result {
                        Ok(batch_responses) => {
                            if batch_responses.is_empty() {
                                // No responses available, yield briefly to other tasks
                                tokio::task::yield_now().await;
                            } else {
                                total_responses += batch_responses.len();
                                for response in batch_responses {
                                    self.process_probe_response(response).await;
                                }
                                // Continue immediately if we got responses
                                continue;
                            }
                        }
                        Err(_) => {
                            // Error in collection, yield and continue
                            tokio::task::yield_now().await;
                        }
                    }
                }
                // Timeout fallback to prevent infinite waiting
                _ = tokio::time::sleep(Duration::from_micros(500)) => {
                    // Cooperative yielding instead of busy polling
                    tokio::task::yield_now().await;
                }
            }
        }

        debug!(
            "Collected {} responses in {:?} (event-driven)",
            total_responses,
            start_collect.elapsed()
        );
    }

    // Process individual probe responses
    async fn process_probe_response(&mut self, response: ProbeResponse) {
        if !self.apply_response(&response) {
            return;
        }
        if !self.args.numeric && response.icmp_type != IcmpResponseType::Timeout {
            self.perform_dns_lookup(response.hop, response.source_addr)
                .await;
        }

        // Trigger real-time UI update when a response arrives
        if let Some(ref callback) = self.update_callback {
            callback();
        }
    }

    // DNS lookup functionality
    async fn perform_dns_lookup(&mut self, hop_index: usize, addr: IpAddr) {
        if hop_index >= self.hops.len() {
            return;
        }

        if let Ok(lookup_result) = self.resolver.reverse_lookup(addr).await {
            if let Some(hostname) = lookup_result.iter().next() {
                let hostname_str = hostname.to_string();
                if hostname_str != addr.to_string() {
                    debug!("Resolved {} to {}", addr, hostname_str);
                    self.hops[hop_index].set_hostname_for_addr(addr, hostname_str);
                }
            }
        }
    }

    // ProbeEngine-based sequence management
    fn prepare_sequence(&mut self, index: usize) -> u16 {
        let seq = self.next_sequence;
        self.next_sequence += 1;
        if self.next_sequence == MAX_SEQUENCE {
            self.next_sequence = MIN_SEQUENCE;
        }
        self.hops[index].increment_sent();

        // Trigger UI update immediately when packet is sent (shows waiting state)
        if let Some(ref callback) = self.update_callback {
            callback();
        }

        seq
    }

    fn save_sequence_with_send_time(&mut self, index: usize, seq: u16, send_time: Instant) {
        let entry = SequenceEntry {
            index,
            transit: true,
            saved_seq: self.hops[index].sent as u32,
            send_time,
        };
        self.sequence_table.insert(seq, entry);
    }

    async fn run_simulated_trace(&mut self) -> Result<()> {
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

    /// Run the trace with live updates for the TUI. `probe_engine` comes from
    /// `open_probe_engine`; `None` means simulation.
    pub async fn run_trace_with_realtime_updates(
        session_arc: std::sync::Arc<std::sync::Mutex<Self>>,
        probe_engine: Option<ProbeEngine>,
    ) -> Result<()> {
        let (target_addr, args) = {
            let session = session_arc.lock().unwrap();
            (session.target_addr, session.args.clone())
        };

        info!("Starting real-time trace to {}", target_addr);

        let Some(probe_engine) = probe_engine else {
            return Self::run_simulated_trace_realtime(session_arc, args).await;
        };
        let IpAddr::V4(ipv4) = target_addr else {
            bail!("IPv6 targets are not supported yet");
        };
        Self::run_real_trace_realtime(session_arc, ipv4, probe_engine, args).await
    }

    async fn run_real_trace_realtime(
        session_arc: std::sync::Arc<std::sync::Mutex<Self>>,
        target: Ipv4Addr,
        probe_engine: ProbeEngine,
        args: Args,
    ) -> Result<()> {
        info!("Starting real network trace with channels (real-time UI)");

        // Create channels for communication between probe task and UI
        let (response_tx, response_rx) = mpsc::unbounded_channel::<ProbeResponse>();
        let (sent_tx, sent_rx) = mpsc::unbounded_channel::<usize>(); // hop index when packet sent

        // Clone session for probe task
        let probe_session_arc = Arc::clone(&session_arc);
        let probe_args = args.clone();

        // Spawn probe task that runs independently
        let probe_handle = tokio::spawn(async move {
            Self::run_probe_task(
                probe_session_arc,
                target,
                probe_engine,
                probe_args,
                response_tx,
                sent_tx,
            )
            .await
        });

        // UI task processes both sent notifications and responses
        let ui_handle = tokio::spawn(async move {
            Self::run_ui_processor_with_sent_notifications(session_arc, response_rx, sent_rx).await
        });

        // Wait for both tasks
        let (probe_result, ui_result) = tokio::try_join!(probe_handle, ui_handle)?;
        probe_result?;
        ui_result?;

        Ok(())
    }

    // Probe task - continuously sends probes and async listens for responses
    #[allow(unused_mut)]
    async fn run_probe_task(
        session_arc: std::sync::Arc<std::sync::Mutex<Self>>,
        target: Ipv4Addr,
        mut probe_engine: ProbeEngine,
        args: Args,
        response_tx: mpsc::UnboundedSender<ProbeResponse>,
        sent_tx: mpsc::UnboundedSender<usize>,
    ) -> Result<()> {
        info!("Probe task starting with {} max hops", args.max_hops);

        // Spawn continuous response listener task
        #[allow(unused_mut)]
        let (probe_tx, probe_rx) = mpsc::unbounded_channel();
        let listener_response_tx = response_tx.clone();

        let listener_handle = tokio::spawn(async move {
            Self::run_response_listener(probe_engine, probe_rx, listener_response_tx).await
        });

        // Main probe sending loop
        let sender_handle = tokio::spawn(async move {
            let mut round = 0;

            loop {
                if let Some(count) = args.count {
                    if round >= count {
                        break;
                    }
                }

                // Send to every TTL up to the end of the path (all of them until it is known)
                let hops_this_round = session_arc.lock().unwrap().num_hosts;
                for i in 0..hops_this_round {
                    // Notify UI that packet is being sent (shows waiting state)
                    if sent_tx.send(i).is_err() {
                        return Ok::<(), anyhow::Error>(());
                    }

                    let dest = SocketAddr::new(target.into(), 0);
                    let ttl = (i + 1) as u8;
                    let timeout = Duration::from_millis(5000);

                    // Send probe request to listener task
                    if probe_tx.send((i, dest, ttl, timeout, round)).is_err() {
                        return Ok::<(), anyhow::Error>(());
                    }
                }

                debug!("Sent {} probes for round {}", hops_this_round, round + 1);
                round += 1;

                tokio::time::sleep(Duration::from_millis(args.interval)).await;
            }

            info!("Probe sender completed {} rounds", round);
            Ok(())
        });

        // Wait for both tasks
        let (listener_result, sender_result) = tokio::try_join!(listener_handle, sender_handle)?;
        listener_result?;
        sender_result?;

        Ok(())
    }

    // Pure event-driven async response listener (zero polling!)
    async fn run_response_listener(
        mut probe_engine: ProbeEngine,
        mut probe_rx: mpsc::UnboundedReceiver<(usize, SocketAddr, u8, Duration, usize)>, // (hop, dest, ttl, timeout, round)
        response_tx: mpsc::UnboundedSender<ProbeResponse>,
    ) -> Result<()> {
        use std::collections::HashMap;

        let mut sent_sequences: HashMap<u16, (usize, usize)> = HashMap::new(); // seq -> (hop, round)

        loop {
            tokio::select! {
                // Handle probe send requests (channel-driven)
                probe_request = probe_rx.recv() => {
                    if let Some((hop, dest, ttl, timeout, round)) = probe_request {
                        match probe_engine.send_probe(hop, dest, ttl, timeout) {
                            Ok(seq) => {
                                sent_sequences.insert(seq, (hop, round));
                                debug!("Sent probe: hop={}, round={}, seq={}", hop + 1, round + 1, seq);
                            }
                            Err(e) => debug!("Failed to send probe: {}", e),
                        }
                    } else {
                        // Sender dropped, time to exit
                        break;
                    }
                }

                // Event-driven response collection (no sleep!)
                result = probe_engine.collect_responses_async() => {
                    match result {
                        Ok(responses) => {
                            for response in responses {
                                // Check if this sequence belongs to a known round
                                if let Some((expected_hop, round)) = sent_sequences.remove(&response.seq) {
                                    if expected_hop == response.hop {
                                        debug!("Valid response: hop={}, round={}, seq={}, rtt={:?}",
                                              response.hop + 1, round + 1, response.seq, response.rtt);
                                    } else {
                                        debug!("WARNING: Hop mismatch - expected {}, got {}", expected_hop + 1, response.hop + 1);
                                    }

                                    if response_tx.send(response).is_err() {
                                        return Ok(());
                                    }
                                } else {
                                    debug!("OUT-OF-ORDER/LATE: seq={}, hop={}, rtt={:?} - no matching sent probe",
                                          response.seq, response.hop + 1, response.rtt);
                                }
                            }
                        }
                        Err(_) => {
                            // Brief yield on error to prevent tight error loops
                            tokio::task::yield_now().await;
                        }
                    }
                }
            }
        }

        info!("Response listener finished");
        Ok(())
    }

    // UI processor - handles both sent notifications and responses for real-time updates
    async fn run_ui_processor_with_sent_notifications(
        session_arc: std::sync::Arc<std::sync::Mutex<Self>>,
        mut response_rx: mpsc::UnboundedReceiver<ProbeResponse>,
        mut sent_rx: mpsc::UnboundedReceiver<usize>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                // Handle packet sent notifications (shows waiting state)
                sent_hop = sent_rx.recv() => {
                    match sent_hop {
                        Some(hop_index) => {
                            let should_update = {
                                let mut session = session_arc.lock().unwrap();
                                if hop_index < session.hops.len() {
                                    session.hops[hop_index].increment_sent();
                                    session.update_callback.is_some()
                                } else {
                                    false
                                }
                            };

                            // Trigger UI update immediately when packet is sent
                            if should_update {
                                let session = session_arc.lock().unwrap();
                                if let Some(ref callback) = session.update_callback {
                                    callback();
                                }
                            }
                        }
                        None => {
                            // Sender closed, but continue processing responses
                            break;
                        }
                    }
                }

                // Handle packet responses (shows actual RTT)
                response_result = response_rx.recv() => {
                    match response_result {
                        Some(response) => {
                            let (should_trigger_update, needs_dns_lookup) = {
                                let mut session = session_arc.lock().unwrap();
                                if session.apply_response(&response) {
                                    let needs_dns = !session.args.numeric
                                        && response.icmp_type != IcmpResponseType::Timeout;
                                    (session.update_callback.is_some(), needs_dns)
                                } else {
                                    (false, false)
                                }
                            };

                            // Perform DNS lookup if needed (outside the lock to avoid deadlock)
                            if needs_dns_lookup {
                                let session_clone = session_arc.clone();
                                let source_addr = response.source_addr;
                                let hop_index = response.hop;

                                // Extract resolver outside the async block to avoid Send issues
                                let resolver = {
                                    let session = session_arc.lock().unwrap();
                                    session.resolver.clone()
                                };

                                // Spawn DNS lookup task to avoid blocking the UI processor
                                tokio::spawn(async move {
                                    if let Ok(lookup_result) = resolver.reverse_lookup(source_addr).await {
                                        if let Some(hostname) = lookup_result.iter().next() {
                                            let hostname_str = hostname.to_string();
                                            if hostname_str != source_addr.to_string() {
                                                debug!("UI: Resolved {} to {}", source_addr, hostname_str);
                                                let mut session = session_clone.lock().unwrap();
                                                if hop_index < session.hops.len() {
                                                    session.hops[hop_index].set_hostname_for_addr(source_addr, hostname_str);
                                                    // Trigger UI update after DNS resolution
                                                    if let Some(ref callback) = session.update_callback {
                                                        callback();
                                                    }
                                                }
                                            }
                                        }
                                    }
                                });
                            }

                            // Trigger UI update after processing each response (moved outside lock)
                            if should_trigger_update {
                                let session = session_arc.lock().unwrap();
                                if let Some(ref callback) = session.update_callback {
                                    callback();
                                }
                            }

                            // Yield to allow other tasks to run - cooperative multitasking
                            tokio::task::yield_now().await;
                        }
                        None => {
                            // Response channel closed
                            break;
                        }
                    }
                }
            }
        }

        info!("UI processor finished");
        Ok(())
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
            force_simulate: false,
            timing: false,
            quiet: false,
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
            force_simulate: false,
            timing: false,
            quiet: false,
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
        let now = Instant::now();
        ProbeResponse {
            hop,
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
            force_simulate: false,
            timing: false,
            quiet: false,
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
