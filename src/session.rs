use crate::args::{Flows, ProbeProtocol};
use crate::asn::AsInfo;
use crate::geo::{self, GeoFix, GeoSource, Inflation, Origin};
use crate::hop_stats::AddrInfo;
use crate::mda::Mda;
use crate::pmtu::PathMtu;
use crate::privacy::{self, StandIns};
use crate::probe::{self, IcmpResponseType, Nat, ProbeResponse, Prober};
use crate::record::{self, Event, Recorder};
use crate::simulate::{self, SimProber};
use crate::{tracer, Args, HopStats, Result};
use anyhow::{anyhow, bail};
use hickory_resolver::{
    config::{LookupIpStrategy, ResolverConfig, CLOUDFLARE},
    net::runtime::TokioRuntimeProvider,
    TokioResolver,
};

use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    hash::Hash,
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

/// Rounds in report mode when `--count` is not given (same as mtr).
pub const DEFAULT_REPORT_CYCLES: usize = 10;

/// Called when the session changes, so the TUI can redraw
pub type UpdateCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct MtrSession {
    pub target: String,
    pub target_addr: IpAddr,
    pub hops: Vec<HopStats>,
    pub args: Args,
    pub resolver: TokioResolver,
    pub num_hosts: usize, // TTLs probed each round (trimmed once the path end is known)
    update_callback: Option<UpdateCallback>,
    /// How long a probe may stay unanswered before it counts as lost
    pub probe_timeout: Duration,
    /// Reverse DNS results per address; `None` while pending or when there is no name
    dns_names: HashMap<IpAddr, Option<String>>,
    /// Origin AS per address, with `--aslookup`; `None` while pending or unknown
    asns: HashMap<IpAddr, Option<AsInfo>>,
    /// Addresses asked of IPmap, with `--ipmap`
    ipmap: HashSet<IpAddr>,
    /// AS names per AS number; `None` while pending or unknown
    as_names: HashMap<u32, Option<String>>,
    /// The geolocation database, if there is one
    geo_source: Option<Arc<dyn GeoSource>>,
    /// Locations per address; `None` when the database doesn't know it
    locations: HashMap<IpAddr, Option<GeoFix>>,
    /// The path MTU search, with `--pmtu`
    pub pmtu: Option<PathMtu>,
    /// How each round picks its flow
    pub flow_plan: FlowPlan,
    /// Where `--record` writes what the session is told
    pub recorder: Option<Arc<Recorder>>,
    /// With `--replay`, the first line of the log
    pub replayed: Option<record::Start>,
}

/// How each round picks its flow
#[derive(Debug, Clone)]
pub enum FlowPlan {
    /// Round mod n, with `--flows n`
    Fixed(u16),
    /// MDA-Lite chooses, with `--flows auto`
    Auto(Mda),
}

impl FlowPlan {
    /// The flow for round `round`, which probes up to `last_ttl`
    pub fn next(&mut self, round: usize, last_ttl: u8) -> u16 {
        match self {
            Self::Fixed(n) => (round % usize::from(*n)) as u16,
            Self::Auto(mda) => mda.next_flow(Instant::now(), last_ttl),
        }
    }

    /// The search for load-balanced branches, with `--flows auto`
    pub fn mda(&self) -> Option<&Mda> {
        match self {
            Self::Fixed(_) => None,
            Self::Auto(mda) => Some(mda),
        }
    }
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
        builder.options_mut().ip_strategy = match (args.ipv4, args.ipv6) {
            (true, _) => LookupIpStrategy::Ipv4Only,
            (_, true) => LookupIpStrategy::Ipv6Only,
            _ => LookupIpStrategy::Ipv4AndIpv6,
        };
        let resolver = builder.build()?;

        // Resolve target hostname to IP
        let target_addr = if let Ok(ip) = args.target.parse::<IpAddr>() {
            if args.ipv4 && ip.is_ipv6() || args.ipv6 && ip.is_ipv4() {
                bail!(
                    "{ip} is not an IPv{} address",
                    if args.ipv4 { 4 } else { 6 }
                );
            }
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
            choose_address(response.iter(), probe::routable)
                .ok_or_else(|| anyhow!("{target}: no such host"))?
        };

        let mut hops: Vec<HopStats> = (1..=args.max_hops).map(HopStats::new).collect();

        // Configure EMA alpha for all hops from command line args
        for hop in &mut hops {
            hop.set_ema_alpha(args.ema_alpha);
        }
        let num_hosts = hops.len();
        let flow_plan = match args.flows {
            Flows::Fixed(n) => FlowPlan::Fixed(n),
            Flows::Auto => FlowPlan::Auto(Mda::new()),
        };
        let recorder = match &args.record {
            Some(path) => {
                let start = record::start(&args, target_addr);
                Some(Recorder::create(path, start, args.privacy_hops)?)
            }
            None => None,
        };

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
            as_names: HashMap::new(),
            ipmap: HashSet::new(),
            geo_source: None,
            locations: HashMap::new(),
            pmtu: None,
            flow_plan,
            recorder,
            replayed: None,
        })
    }

    /// The flows probed so far, for the flow matrix: 0 up to this
    pub fn flows(&self) -> u16 {
        match &self.flow_plan {
            FlowPlan::Fixed(n) => *n,
            FlowPlan::Auto(mda) => mda.width(),
        }
    }

    /// The last TTL each round probes: the end of the path, once it is known
    pub fn last_ttl(&self) -> u8 {
        u8::try_from(self.num_hosts).unwrap_or(u8::MAX)
    }

    /// Lines for under `hop`: a NAT before it, where the path MTU drops past it,
    /// how many branches it has.
    pub fn notes_after(&self, hop: u8) -> Vec<String> {
        let mut notes: Vec<String> = self.nat_note(hop).into_iter().collect();
        if let Some(pmtu) = &self.pmtu {
            notes.extend(pmtu.notes_after(hop));
        }
        notes.extend(
            self.flow_plan
                .mda()
                .and_then(|mda| mda.note(hop, self.last_ttl())),
        );
        notes
    }

    /// Where the quotes start to show a NAT, or another one than before: under
    /// the first hop past it that answered with an error.
    fn nat_note(&self, hop: u8) -> Option<String> {
        let index = usize::from(hop).checked_sub(1)?;
        let nat = self.hops.get(index)?.nat()?;
        let before = self.hops[..index].iter().rev().find_map(HopStats::nat);
        if before == Some(nat) {
            return None;
        }
        Some(match nat {
            Nat::Source(addr) => format!(
                "NAT before this hop: probes reach it from {}",
                privacy::show(addr)
            ),
            Nat::Checksum => {
                "NAT before this hop: it puts the probes' identifier back, not their checksum"
                    .to_string()
            }
        })
    }

    pub fn is_simulated(&self) -> bool {
        self.args.simulate
    }

    /// Open the prober (simulated with `--simulate`) and the geolocation database.
    /// Fails instead of falling back to simulated data when real probing is impossible.
    pub fn open_probe_engine(&mut self) -> Result<Box<dyn Prober>> {
        self.open_geo()?;
        if self.is_simulated() {
            // Names come with the made-up path; a simulation sends no lookups.
            let target = (self.target != self.target_addr.to_string()).then(|| self.target.clone());
            self.set_hostname(self.target_addr, target);
            for (addr, name) in simulate::names() {
                self.set_hostname(addr, Some(name));
            }
            if self.args.pmtu {
                self.pmtu = Some(PathMtu::new(1500, self.target_addr.is_ipv6()));
            }
            return Ok(Box::new(SimProber::new()));
        }
        if !probe::routable(self.target_addr) {
            let family = if self.target_addr.is_ipv6() {
                "IPv6 "
            } else {
                ""
            };
            bail!("no {family}route to {}", self.target_addr);
        }
        // Windows: ICMP only, without --pmtu (see Args::validate)
        #[cfg(windows)]
        let engine = probe::IcmpApi::new(self.target_addr);
        #[cfg(not(windows))]
        let engine = {
            let engine = probe::ProbeEngine::new(self.target_addr, self.args.protocol)?;
            if self.args.pmtu {
                engine.set_dont_fragment()?;
                let start = probe::interface_mtu(self.target_addr).unwrap_or(1500);
                info!("Path MTU search starts at {start}, the interface's MTU");
                self.pmtu = Some(PathMtu::new(start, self.target_addr.is_ipv6()));
            }
            engine
        };
        Ok(Box::new(engine))
    }

    /// TTL and size of this round's path MTU probe, if one is due: to the end of
    /// the path, once the target answered or after a few rounds without it, and
    /// once a black hole is found, to the hops it may be near.
    pub fn mtu_probe(&mut self, round: usize) -> Option<(u8, u16)> {
        let end_known = self.hops.iter().any(|hop| hop.is_target);
        if !end_known && round < 3 {
            return None;
        }
        let end = u8::try_from(self.display_hop_count())
            .ok()
            .filter(|&ttl| ttl > 0)?;
        let hops = &self.hops;
        let answers = |ttl: u8| hops[usize::from(ttl) - 1].received > 0;
        self.pmtu.as_mut()?.next_probe(end, answers)
    }

    /// The hop `addr` answered at, if it answered any.
    pub fn hop_of(&self, addr: IpAddr) -> Option<u8> {
        self.hops
            .iter()
            .find(|hop| hop.responders.contains_key(&addr))
            .map(|hop| hop.hop)
    }

    /// The made-up locations with `--simulate`, else `--geoip-mmdb` or the first
    /// database found. Asking for locations when there is none is an error.
    pub fn open_geo(&mut self) -> Result<()> {
        self.geo_source = match self.is_simulated() {
            true => Some(Arc::new(simulate::SimGeo::new(self.target_addr))),
            false => geo::open(self.args.geoip_mmdb.as_deref())?,
        };
        // IPmap can do without a database
        if self.geo_source.is_none() && self.args.wants_geo() && !self.args.ipmap {
            bail!(
                "no geolocation database was found. Get DB-IP's free one with\n  {}\nor pass --geoip-mmdb PATH",
                geo::DOWNLOAD
            );
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_geo_source(&mut self, source: Arc<dyn GeoSource>) {
        self.geo_source = Some(source);
    }

    /// Whose data the locations are, as their licences ask to be credited: the
    /// database's, and IPmap's with `--ipmap`.
    pub fn geo_attribution(&self) -> Option<String> {
        let ipmap = self.args.ipmap && !self.is_simulated();
        let credits: Vec<&str> = [
            self.database_attribution(),
            ipmap.then_some(geo::ipmap::ATTRIBUTION),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!credits.is_empty()).then(|| credits.join("; "))
    }

    /// The database's credit alone
    pub fn database_attribution(&self) -> Option<&str> {
        self.geo_source.as_deref().map(GeoSource::attribution)
    }

    /// Whether hops are placed at all: by a database, or by IPmap
    fn locating(&self) -> bool {
        self.geo_source.is_some() || self.args.ipmap
    }

    /// Where `addr` is, looked up once. Private and special addresses are never
    /// looked up; the simulation's documentation addresses are.
    fn locate(&mut self, addr: IpAddr) -> Option<GeoFix> {
        if let Some(known) = self.locations.get(&addr) {
            return known.clone();
        }
        let source = self.geo_source.as_ref()?;
        let public = crate::asn::is_public(addr) || self.is_simulated();
        self.locations
            .entry(addr)
            .or_insert_with(|| public.then(|| source.locate(addr)).flatten())
            .clone()
    }

    /// Whether the best RTT `addr` answered with puts `fix` out of reach.
    fn ruled_out(&self, addr: IpAddr, fix: &GeoFix) -> bool {
        let best = self
            .hops
            .iter()
            .filter_map(|hop| hop.responders.get(&addr)?.best_rtt)
            .min();
        self.origin()
            .and_then(|origin| geo::assess(fix, best, &origin))
            .is_some_and(|a| a.impossible)
    }

    /// Where distances are measured from: `--origin`, or else the first hop the
    /// database places in a city. The probing host is somewhere within that hop's
    /// reach and accuracy, so both count as slack.
    pub fn origin(&self) -> Option<Origin> {
        if let Some((lat, lon)) = self.args.origin {
            return Some(Origin {
                lat,
                lon,
                slack_km: 0.0,
                hop: None,
                rtt: Duration::ZERO,
            });
        }
        self.hops[..self.display_hop_count()]
            .iter()
            .find_map(|hop| {
                let fix = hop.geo().filter(|fix| fix.is_city())?;
                Some(Origin {
                    lat: fix.lat,
                    lon: fix.lon,
                    slack_km: fix.accuracy() + hop.best_rtt.map_or(0.0, geo::reach_km),
                    hop: Some(hop.hop),
                    rtt: hop.best_rtt.unwrap_or_default(),
                })
            })
    }

    /// How far the path detours through the hops located past the origin, if
    /// the RTT allows their locations.
    pub fn inflation(&self) -> Option<Inflation> {
        let origin = self.origin()?;
        let stops: Vec<(u8, (f64, f64))> = self.hops[..self.display_hop_count()]
            .iter()
            .filter(|hop| origin.hop.is_none_or(|from| hop.hop > from))
            .filter_map(|hop| {
                let fix = hop.geo()?;
                let allowed = !hop.assess(Some(&origin))?.impossible;
                allowed.then_some((hop.hop, (fix.lat, fix.lon)))
            })
            .collect();
        geo::inflation(&origin, &stops)
    }

    /// Whether hop `ttl` is hidden by `--privacy-hops`: never looked up, and shown
    /// only in a redacted copy.
    pub fn is_hidden(&self, ttl: usize) -> bool {
        ttl <= usize::from(self.args.privacy_hops)
    }

    /// Replace everything that identifies the hidden hops, in a copy about to be
    /// shown: addresses by stand-ins (see `privacy`), and no names, ASes or
    /// locations. Load-balanced branches keep a stand-in each.
    pub fn redact(&mut self) {
        let hidden = self.args.privacy_hops;
        if hidden == 0 {
            return;
        }
        let mut stand_ins = StandIns::default();
        for hop in self.hops.iter_mut().filter(|hop| hop.hop <= hidden) {
            hop.redact(&mut stand_ins);
        }
        if let FlowPlan::Auto(mda) = &mut self.flow_plan {
            mda.redact(hidden, &mut stand_ins);
        }
        if let Some(pmtu) = self.pmtu.as_mut() {
            for drop in &mut pmtu.drops {
                let at_hidden = drop.hop.is_some_and(|hop| hop <= hidden);
                drop.router = drop.router.map(|router| match at_hidden {
                    true => stand_ins.get(router),
                    false => stand_ins.replace(router),
                });
            }
        }
        // The address a NAT gives probes is the hidden network's own.
        let responders = self
            .hops
            .iter_mut()
            .flat_map(|hop| hop.responders.values_mut());
        for nat in responders.filter_map(|responder| responder.nat.as_mut()) {
            if let Nat::Source(addr) = nat {
                *addr = stand_ins.get(*addr);
            }
        }
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

    /// Why no hop shows, once probes have timed out and none was answered.
    pub fn silence(&self) -> Option<String> {
        let timed_out = self.hops.iter().any(|hop| hop.loss_percent > 0.0);
        if self.display_hop_count() > 0 || !timed_out || self.is_simulated() {
            return None;
        }
        Some(
            if self.args.protocol == ProbeProtocol::Udp && probe::is_local(self.target_addr) {
                "No answer: UDP probes to this machine go unanswered, because its \
                 checksum offload leaves their checksums unfinished. ICMP works."
            } else {
                "No hop has answered: something drops the probes or the answers."
            }
            .to_string(),
        )
    }

    /// Record a probe response in the hop table. An echo reply, Port Unreachable
    /// (UDP's arrival) or Destination Unreachable marks the end of the path; higher
    /// TTLs are no longer probed.
    /// Returns false if the response refers to a hop outside the table.
    pub fn apply_response(&mut self, response: &ProbeResponse) -> bool {
        // Every field is named, so a new one has to be stored here too
        let &ProbeResponse {
            hop: index,
            ordinal,
            source_addr: addr,
            icmp_type,
            rtt,
            reply_ttl,
            ref ext,
            nat,
        } = response;
        if let Some(recorder) = self.recorder.as_ref().filter(|_| index < self.hops.len()) {
            recorder.response(response);
        }
        let Some(hop) = self.hops.get_mut(index) else {
            return false;
        };
        match icmp_type {
            IcmpResponseType::TimeExceeded => {
                hop.record_reply(ordinal, Some(addr), rtt);
            }
            IcmpResponseType::EchoReply
            | IcmpResponseType::PortUnreachable
            | IcmpResponseType::DestinationUnreachable => {
                hop.record_reply(ordinal, Some(addr), rtt);
                hop.mark_as_target();
                if index + 1 < self.num_hosts {
                    info!("Path ends at hop {} ({})", index + 1, addr);
                    self.num_hosts = index + 1;
                }
            }
            // Only path MTU probes are large enough; the tracer hands these on.
            IcmpResponseType::TooBig { .. } => return true,
        }
        self.hops[index].record_reply_details(addr, reply_ttl, ext.clone());
        // An echo reply quotes nothing, so it shows no NAT either way.
        if icmp_type != IcmpResponseType::EchoReply {
            self.hops[index].set_nat(addr, nat);
        }
        if !self.is_hidden(index + 1) {
            let info = self.info(addr);
            self.hops[index].set_info(addr, &info);
        }
        true
    }

    /// What lookups found out about `addr` so far.
    fn info(&mut self, addr: IpAddr) -> AddrInfo {
        AddrInfo {
            hostname: self.dns_names.get(&addr).cloned().flatten(),
            asn: self.asns.get(&addr).cloned().flatten(),
            geo: self.locate(addr),
        }
    }

    /// Show what is known about `addr` at every hop where it answered, except
    /// the hops `--privacy-hops` hides.
    fn refresh(&mut self, addr: IpAddr) {
        let info = self.info(addr);
        let hidden = usize::from(self.args.privacy_hops);
        for hop in self.hops.iter_mut().skip(hidden) {
            hop.set_info(addr, &info);
        }
    }

    /// True the first time an address is seen: the caller should look it up.
    pub fn want_lookup(&mut self, addr: IpAddr) -> bool {
        first_time(&mut self.dns_names, addr)
    }

    /// Store a reverse DNS result and show it on every hop where `addr` answered.
    /// A city code in the name goes before the database's location, unless the
    /// RTT so far rules the name's city out and not the database's.
    pub fn set_hostname(&mut self, addr: IpAddr, name: Option<String>) {
        if let Some(recorder) = &self.recorder {
            recorder.name(addr, &name);
        }
        if let Some(name) = &name {
            debug!("Resolved {} to {}", addr, name);
            let ruled_out = |fix: &GeoFix| self.ruled_out(addr, fix);
            let database = self.locations.get(&addr).cloned().flatten();
            let named = self
                .locating()
                .then(|| geo::names::locate(name))
                .flatten()
                .filter(|fix| !ruled_out(fix) || database.as_ref().is_none_or(ruled_out));
            if let Some(fix) = named {
                self.locations.insert(addr, Some(fix));
            }
        }
        self.dns_names.insert(addr, name);
        self.refresh(addr);
    }

    /// Start the statistics over, keeping what is known about the path itself.
    pub fn reset_statistics(&mut self) {
        if let Some(recorder) = &self.recorder {
            recorder.reset();
        }
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
        self.args.wants_asn()
            && !self.is_simulated()
            && crate::asn::is_public(addr)
            && first_time(&mut self.asns, addr)
    }

    /// Store an AS lookup result, with the AS's name if it is known already, and
    /// show it on every hop where `addr` answered.
    pub fn set_asn(&mut self, addr: IpAddr, asn: Option<AsInfo>) {
        if let Some(recorder) = &self.recorder {
            recorder.asn(addr, &asn);
        }
        let asn = asn.map(|mut info| {
            if let Some(Some(name)) = self.as_names.get(&info.number) {
                info.name = Some(name.clone());
            }
            info
        });
        self.asns.insert(addr, asn);
        self.refresh(addr);
    }

    /// True the first time AS `number` is seen: the caller should look up its name.
    pub fn want_as_name(&mut self, number: u32) -> bool {
        first_time(&mut self.as_names, number)
    }

    /// Store the name of AS `number` and show it wherever that AS answered.
    pub fn set_as_name(&mut self, number: u32, name: Option<String>) {
        if let Some(recorder) = &self.recorder {
            recorder.as_name(number, &name);
        }
        self.as_names.insert(number, name.clone());
        let Some(name) = name else { return };
        let mut named = Vec::new();
        for (&addr, info) in &mut self.asns {
            if let Some(info) = info.as_mut().filter(|i| i.number == number) {
                info.name = Some(name.clone());
                named.push(addr);
            }
        }
        for addr in named {
            self.refresh(addr);
        }
    }

    /// True the first time a public address is seen with `--ipmap`: the caller
    /// should ask IPmap where it is.
    pub fn want_ipmap(&mut self, addr: IpAddr) -> bool {
        self.args.ipmap
            && !self.is_simulated()
            && crate::asn::is_public(addr)
            && self.ipmap.insert(addr)
    }

    /// Store IPmap's answer for `addr` and place it there, unless a city code in
    /// its name does (names go first), or the RTT rules IPmap's place out and
    /// not the one it has.
    pub fn set_ipmap(&mut self, addr: IpAddr, fix: Option<GeoFix>) {
        if let Some(recorder) = &self.recorder {
            recorder.ipmap(addr, &fix);
        }
        let Some(fix) = fix else { return };
        let current = self.locations.get(&addr).cloned().flatten();
        let keep = current.as_ref().is_some_and(|current| {
            let named = current.from_name.is_some();
            !self.ruled_out(addr, current) && (named || self.ruled_out(addr, &fix))
        });
        if keep {
            return;
        }
        self.locations.insert(addr, Some(fix));
        self.refresh(addr);
    }

    /// Count a probe sent to hop `index` (from 0) in `flow`. Returns its number
    /// at the hop, which its reply or timeout names.
    pub fn sent(&mut self, index: usize, flow: u16) -> u64 {
        let probe = self.hops[index].increment_sent(flow);
        if let Some(recorder) = &self.recorder {
            recorder.sent(index, flow, probe);
        }
        probe
    }

    /// Count probe `probe` to hop `index` as lost: it timed out, could not be
    /// sent, or its sequence number came round again.
    pub fn lost(&mut self, index: usize, probe: u64) {
        if let Some(recorder) = &self.recorder {
            recorder.timeout(index, probe);
        }
        self.hops[index].record_timeout(Some(probe));
    }

    /// Apply an event from a `--record` log.
    pub fn replay(&mut self, event: Event) {
        let hops = self.hops.len();
        let index = |hop: u8| usize::from(hop).checked_sub(1).filter(|&i| i < hops);
        match event {
            Event::Sent { hop, flow, .. } => {
                if let Some(index) = index(hop) {
                    if let FlowPlan::Fixed(n) = &mut self.flow_plan {
                        *n = record::flows_with(*n, flow);
                    }
                    self.sent(index, flow);
                }
            }
            Event::Reply {
                hop,
                probe,
                from,
                kind,
                rtt_ms,
                reply_ttl,
                ext,
                nat,
            } => {
                let Some(index) = index(hop) else { return };
                // A negative or absurd RTT becomes 0
                let rtt = Duration::from_nanos((rtt_ms * 1e6).round() as u64);
                self.apply_response(&ProbeResponse {
                    hop: index,
                    ordinal: Some(probe),
                    source_addr: from,
                    icmp_type: kind.into(),
                    rtt,
                    reply_ttl,
                    ext,
                    nat,
                });
            }
            Event::Timeout { hop, probe } => {
                if let Some(index) = index(hop) {
                    self.lost(index, probe);
                }
            }
            Event::Name { addr, name } => self.set_hostname(addr, name),
            Event::Asn { addr, asn, prefix } => {
                let info = asn.map(|number| AsInfo {
                    number,
                    prefix,
                    name: None,
                });
                self.set_asn(addr, info);
            }
            Event::AsName { asn, name } => self.set_as_name(asn, name),
            Event::Ipmap { addr, location } => self.set_ipmap(addr, location),
            Event::Reset => self.reset_statistics(),
            Event::Start(_) => {}
        }
    }

    /// Tell the UI that something changed.
    pub fn notify(&self) {
        if let Some(ref callback) = self.update_callback {
            callback();
        }
    }

    pub fn set_update_callback(&mut self, callback: UpdateCallback) {
        self.update_callback = Some(callback);
    }
}

/// Which of a name's addresses to trace: its first IPv6 address when this machine
/// has a route there, as a connection would prefer it (RFC 6724), else its first
/// IPv4 address.
fn choose_address(
    addrs: impl IntoIterator<Item = IpAddr>,
    routable: impl Fn(IpAddr) -> bool,
) -> Option<IpAddr> {
    let addrs: Vec<IpAddr> = addrs.into_iter().collect();
    let v6 = addrs.iter().copied().find(IpAddr::is_ipv6);
    let v4 = addrs.iter().copied().find(IpAddr::is_ipv4);
    match (v6, v4) {
        (Some(v6), Some(v4)) => Some(if routable(v6) { v6 } else { v4 }),
        (v6, v4) => v6.or(v4),
    }
}

/// True if `key` is new to `map`, which then holds it with no answer yet.
fn first_time<K: Hash + Eq, V>(map: &mut HashMap<K, Option<V>>, key: K) -> bool {
    match map.entry(key) {
        Entry::Vacant(entry) => {
            entry.insert(None);
            true
        }
        Entry::Occupied(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icmp_ext::Extensions;
    use clap::Parser;

    #[tokio::test]
    async fn test_mtr_session_new_with_ip() {
        let session = session_for(&["-n", "-c", "5", "-i", "500", "-M", "20", "192.168.1.1"]).await;
        assert_eq!(session.target, "192.168.1.1");
        assert_eq!(session.target_addr.to_string(), "192.168.1.1");
        assert_eq!(session.hops.len(), 20);
        assert_eq!(session.args.count, Some(5));
        assert_eq!(session.args.interval, 500);
    }

    #[tokio::test]
    async fn test_mtr_session_new_with_localhost() {
        let session = session_for(&["-r", "-c", "3", "-M", "15", "localhost"]).await;
        assert_eq!(session.target, "localhost");
        assert_eq!(session.hops.len(), 15);
        assert!(session.args.report);
        assert!(!session.args.numeric);
    }

    fn response(hop: usize, kind: IcmpResponseType, from: [u8; 4]) -> ProbeResponse {
        ProbeResponse {
            hop,
            ordinal: None,
            source_addr: IpAddr::V4(std::net::Ipv4Addr::from(from)),
            icmp_type: kind,
            rtt: Duration::from_millis(10),
            reply_ttl: None,
            ext: Extensions::default(),
            nat: None,
        }
    }

    async fn session_for(cli: &[&str]) -> MtrSession {
        let args = Args::parse_from(std::iter::once("mtr-ng").chain(cli.iter().copied()));
        MtrSession::new(args).await.unwrap()
    }

    #[tokio::test]
    async fn silence_is_explained_once_probes_time_out() {
        for (cli, local) in [
            (&["-n", "192.0.2.1"][..], false),
            (&["-n", "-P", "udp", "192.0.2.1"], false),
            (&["-n", "-P", "udp", "127.0.0.1"], true),
        ] {
            let mut session = session_for(cli).await;
            session.hops[0].increment_sent(0);
            assert_eq!(session.silence(), None, "{cli:?}: still in flight");
            session.hops[0].add_timeout();
            let why = session.silence().expect("explained");
            assert_eq!(why.contains("UDP probes to this machine"), local, "{cli:?}");

            session.apply_response(&response(1, IcmpResponseType::TimeExceeded, [10, 0, 0, 1]));
            assert_eq!(session.silence(), None, "{cli:?}: a hop answered");
        }
    }

    #[tokio::test]
    async fn a_note_marks_where_quotes_start_to_show_a_nat() {
        let mut session = session_for(&["-n", "198.51.100.200"]).await;
        let cgn = IpAddr::from([100, 64, 0, 9]);
        let public = IpAddr::from([203, 0, 113, 9]);
        let quoting = |hop, nat, from| ProbeResponse {
            nat,
            ..response(hop, IcmpResponseType::TimeExceeded, from)
        };
        session.apply_response(&quoting(0, None, [192, 168, 1, 1]));
        session.apply_response(&quoting(1, Some(Nat::Source(cgn)), [100, 64, 0, 1]));
        session.apply_response(&quoting(3, Some(Nat::Source(cgn)), [198, 51, 100, 1]));
        session.apply_response(&quoting(4, Some(Nat::Source(public)), [198, 51, 100, 5]));
        let target = [198, 51, 100, 200];
        session.apply_response(&response(5, IcmpResponseType::EchoReply, target));
        assert!(session.notes_after(1).is_empty());
        assert_eq!(
            session.notes_after(2),
            ["NAT before this hop: probes reach it from 100.64.0.9"]
        );
        assert!(session.notes_after(4).is_empty(), "the same NAT");
        assert_eq!(
            session.notes_after(5),
            ["NAT before this hop: probes reach it from 203.0.113.9"],
            "a second NAT"
        );
        assert!(
            session.notes_after(6).is_empty(),
            "an echo reply quotes nothing"
        );

        session.args.privacy_hops = 1;
        session.redact();
        assert_eq!(
            session.notes_after(2),
            ["NAT before this hop: probes reach it from (hidden)"],
            "the hidden network's public address"
        );
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
        assert!(session.hops[4].is_target);
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
    async fn real_targets_are_probed_or_refused_never_simulated() {
        for target in ["127.0.0.1", "::1"] {
            let mut session = session_for(&["-n", target]).await;
            match session.open_probe_engine() {
                // Without privileges or IPv6 the engine can't open; that's fine.
                Err(e) => assert!(!e.to_string().is_empty()),
                Ok(_) => assert!(session.dns_names.is_empty(), "{target}: made-up names"),
            }
        }
    }

    #[test]
    fn a_routable_ipv6_address_goes_first() {
        let v4: IpAddr = "192.0.2.1".parse().unwrap();
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        let v6b: IpAddr = "2001:db8::2".parse().unwrap();
        assert_eq!(choose_address([v4, v6, v6b], |_| true), Some(v6));
        assert_eq!(
            choose_address([v4, v6], |_| false),
            Some(v4),
            "no IPv6 route"
        );
        assert_eq!(choose_address([v6], |_| false), Some(v6), "nothing else");
        assert_eq!(choose_address([v4], |_| true), Some(v4));
        assert_eq!(choose_address([], |_| true), None);
    }

    #[tokio::test]
    async fn an_address_of_the_wrong_family_is_refused() {
        for (flag, target) in [("-4", "::1"), ("-6", "127.0.0.1")] {
            let args = Args::try_parse_from(["mtr-ng", flag, target]).unwrap();
            let err = MtrSession::new(args).await.err().expect(flag);
            assert!(err.to_string().contains("is not an IPv"), "{err}");
        }
    }

    #[tokio::test]
    async fn locations_skip_private_addresses_and_the_first_city_is_the_origin() {
        use crate::geo::tests::{city, FakeGeo};
        let mut session = session_for(&["-n", "1.1.1.1"]).await;
        let country_only = GeoFix {
            city: None,
            ..city("-", "US", 37.75, -97.82)
        };
        let known = [
            ([192, 168, 1, 1], city("Home", "NL", 52.0, 5.0)),
            ([9, 9, 9, 9], country_only),
            ([8, 8, 8, 8], city("Amsterdam", "NL", 52.37, 4.89)),
            ([1, 1, 1, 1], city("Sydney", "AU", -33.87, 151.21)),
        ];
        let known = known.into_iter().map(|(a, fix)| (IpAddr::from(a), fix));
        session.set_geo_source(Arc::new(FakeGeo(known.collect())));
        let path = [[192, 168, 1, 1], [9, 9, 9, 9], [8, 8, 8, 8], [1, 1, 1, 1]];
        for (ttl, from) in path.into_iter().enumerate() {
            session.apply_response(&response(ttl, IcmpResponseType::TimeExceeded, from));
        }

        assert_eq!(session.hops[0].geo(), None, "private: never looked up");
        assert_eq!(session.hops[1].geo().map(GeoFix::place), Some("US".into()));
        let origin = session.origin().unwrap();
        assert_eq!(
            (origin.hop, origin.lat),
            (Some(3), 52.37),
            "first city, not a country"
        );
        assert_eq!(
            origin.slack_km,
            20.0 + 1000.0,
            "its accuracy and 10 ms of reach"
        );
        let sydney = session.hops[3].assess(Some(&origin)).unwrap();
        assert!(sydney.impossible, "16,600 km in 10 ms");

        // The router's name says Amsterdam, and outlasts the next reply
        let one = IpAddr::from([1, 1, 1, 1]);
        session.set_hostname(one, Some("adm-b3-link.ip.twelve99.net".into()));
        session.apply_response(&response(3, IcmpResponseType::TimeExceeded, [1, 1, 1, 1]));
        let fix = session.hops[3].geo().cloned().unwrap();
        assert_eq!(
            (fix.place(), fix.from_name.as_deref()),
            ("Amsterdam, NL".into(), Some("adm"))
        );
        assert_eq!(session.hops[3].responders[&one].info.geo, Some(fix));
        assert!(!session.hops[3].assess(Some(&origin)).unwrap().impossible);
        session.set_hostname(one, Some("dns.example".into()));
        assert_eq!(
            session.hops[3].geo().and_then(|f| f.from_name.as_deref()),
            Some("adm"),
            "a name without a code changes nothing"
        );
        // A name 10 ms rules out doesn't replace a database location it allows
        let eight = IpAddr::from([8, 8, 8, 8]);
        session.set_hostname(eight, Some("sng-b7-link.ip.twelve99.net".into()));
        assert_eq!(
            session.hops[2].geo().map(GeoFix::place),
            Some("Amsterdam, NL".into())
        );

        session.args.origin = Some((48.85, 2.35));
        assert_eq!(session.origin().unwrap().hop, None, "--origin wins");
    }

    #[tokio::test]
    async fn ipmap_places_what_the_database_has_wrong_but_not_what_names_place() {
        use crate::geo::tests::{city, FakeGeo};
        let mut session = session_for(&["-n", "--ipmap", "1.1.1.1"]).await;
        assert!(session.want_ipmap(IpAddr::from([8, 8, 8, 8])));
        assert!(
            !session.want_ipmap(IpAddr::from([8, 8, 8, 8])),
            "asked once"
        );
        assert!(!session.want_ipmap(IpAddr::from([10, 0, 0, 1])), "private");
        let known = [
            ([8, 8, 8, 8], city("Amsterdam", "NL", 52.37, 4.89)),
            ([9, 9, 9, 9], city("Mountain View", "US", 37.39, -122.08)),
            ([1, 1, 1, 1], city("Sydney", "AU", -33.87, 151.21)),
        ];
        let known = known.into_iter().map(|(a, fix)| (IpAddr::from(a), fix));
        session.set_geo_source(Arc::new(FakeGeo(known.collect())));
        let path = [[8, 8, 8, 8], [9, 9, 9, 9], [1, 1, 1, 1]];
        for (ttl, from) in path.into_iter().enumerate() {
            session.apply_response(&response(ttl, IcmpResponseType::TimeExceeded, from));
        }
        let measured = |place: &str, lat, lon| GeoFix {
            from_ipmap: Some(45),
            ..city(place, "NL", lat, lon)
        };

        // The database put a router 10 ms away in California; IPmap measured it
        let nine = IpAddr::from([9, 9, 9, 9]);
        session.set_ipmap(nine, Some(measured("Rotterdam", 51.92, 4.48)));
        let fix = session.hops[1].geo().cloned().unwrap();
        assert_eq!(
            (fix.place(), fix.from_ipmap),
            ("Rotterdam, NL".into(), Some(45))
        );
        assert_eq!(session.hops[1].responders[&nine].info.geo, Some(fix));

        // A city code in the name goes first, and stays first
        let one = IpAddr::from([1, 1, 1, 1]);
        session.set_hostname(one, Some("adm-b3-link.ip.twelve99.net".into()));
        session.set_ipmap(one, Some(measured("Utrecht", 52.09, 5.12)));
        assert_eq!(
            session.hops[2].geo().unwrap().from_name.as_deref(),
            Some("adm")
        );

        // IPmap's place 10 ms rules out doesn't replace one it allows
        let eight = IpAddr::from([8, 8, 8, 8]);
        let far = GeoFix {
            from_ipmap: Some(40),
            ..city("Tokyo", "JP", 35.68, 139.69)
        };
        session.set_ipmap(eight, Some(far));
        assert_eq!(
            session.hops[0].geo().map(GeoFix::place),
            Some("Amsterdam, NL".into())
        );
        assert_eq!(
            session.geo_attribution().unwrap(),
            "test locations; RIPE IPmap, by the RIPE NCC"
        );
    }

    #[tokio::test]
    async fn a_named_database_that_cannot_be_read_is_an_error() {
        let mut session =
            session_for(&["-g", "--geoip-mmdb", "/nonexistent/x.mmdb", "1.1.1.1"]).await;
        let err = session
            .open_probe_engine()
            .err()
            .expect("must not run without it");
        assert!(
            format!("{err:#}").contains("/nonexistent/x.mmdb"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn simulation_only_when_asked() {
        let mut session = session_for(&["--simulate", "::1"]).await;
        assert!(session.is_simulated());
        assert!(
            session.open_probe_engine().is_ok(),
            "simulated, even for IPv6"
        );
    }
}
