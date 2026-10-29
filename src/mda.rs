//! MDA-Lite: find every branch of the load balancers on the path, then keep each
//! branch probed (`--flows auto`).
//!
//! Each round of discovery sends a new flow through the whole path. At a hop
//! where k addresses answered, n(k) flows must have answered before the chance
//! that a (k+1)th branch exists and no flow met it falls below 5%: 6 flows for
//! one address, 11 for two, 16 for three. These are the stopping points of MDA
//! and MDA-Lite (Vermeulen et al., "Multilevel MDA-Lite Paris Traceroute", IMC
//! 2018). Discovery stops once every hop that answers has enough, or when the
//! flows run out (`MAX_FLOWS`, enough for 11 branches at one hop). After that,
//! only the fewest flows that pass every address found take turns, so each
//! branch keeps its own loss and RTT. Every ten minutes discovery starts over,
//! to catch route changes.
//!
//! Like MDA-Lite, this assumes that balancers spread flows evenly and that
//! branches don't cross. Full MDA steers flows to each router where that fails;
//! this only says where crossing branches were seen, since one may be missing.

use crate::probe::MAX_FLOWS;
use crate::topology;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap},
    net::IpAddr,
    time::{Duration, Instant},
};

/// The chance of a missed branch that discovery accepts at each hop
const MISS: f64 = 0.05;

/// How long the branches one discovery found are trusted before the next
pub const REDISCOVER_AFTER: Duration = Duration::from_secs(600);

/// Flows that must answer at a hop where `k` addresses answered before the chance
/// that a (k+1)th branch went unmet falls to 5%, if balancers spread flows evenly.
pub fn needed(k: usize) -> usize {
    match k {
        0 => 0,
        _ => (k..)
            .find(|&n| missed(k + 1, n) <= MISS)
            .unwrap_or(usize::MAX),
    }
}

/// The chance that `n` flows spread evenly over `ways` branches miss at least
/// one: inclusion–exclusion over the branches missed.
fn missed(ways: usize, n: usize) -> f64 {
    let n = i32::try_from(n).unwrap_or(i32::MAX);
    (1..ways)
        .map(|j| {
            let sign = if j % 2 == 1 { 1.0 } else { -1.0 };
            let ways_left = 1.0 - j as f64 / ways as f64;
            sign * binomial(ways, j) * ways_left.powi(n)
        })
        .sum()
}

fn binomial(n: usize, k: usize) -> f64 {
    (0..k).fold(1.0, |acc, i| acc * (n - i) as f64 / (i + 1) as f64)
}

/// What discovery found at one hop
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branches {
    pub ttl: u8,
    /// The addresses that answered, one per branch
    pub addrs: BTreeSet<IpAddr>,
    /// Flows that got an answer here
    pub flows: usize,
}

impl Branches {
    /// Flows needed here to rule out another branch
    pub fn needed(&self) -> usize {
        needed(self.addrs.len())
    }

    /// Enough flows answered to be 95% sure every branch was found
    pub fn sure(&self) -> bool {
        self.flows >= self.needed()
    }

    fn describe(&self) -> String {
        let ways = self.addrs.len();
        let branches = if ways == 1 { "branch" } else { "branches" };
        match self.sure() {
            true => format!("{ways} {branches}, all found (95%)"),
            false => format!(
                "{ways} {branches} so far, {} of {} flows",
                self.flows,
                self.needed()
            ),
        }
    }
}

#[derive(Debug, Clone)]
enum Phase {
    Discovering,
    /// The flows of the cover take turns, since `since`
    Monitoring {
        turn: usize,
        since: Instant,
    },
}

/// The search for load-balanced branches and, once it is done, which flow each
/// round probes.
#[derive(Debug, Clone)]
pub struct Mda {
    /// Who each flow met in this discovery, TTL by TTL
    paths: BTreeMap<u16, BTreeMap<u8, IpAddr>>,
    /// Flows sent by this discovery: 0 up to here
    sent: u16,
    /// The most flows any discovery has sent
    widest: u16,
    phase: Phase,
    /// Which discovery this is, from 1
    pass: u32,
}

impl Default for Mda {
    fn default() -> Self {
        Self::new()
    }
}

impl Mda {
    pub fn new() -> Self {
        Self {
            paths: BTreeMap::new(),
            sent: 0,
            widest: 0,
            phase: Phase::Discovering,
            pass: 1,
        }
    }

    /// The flow to probe this round. `last_ttl` is the end of the path, as far
    /// as it is known.
    pub fn next_flow(&mut self, now: Instant, last_ttl: u8) -> u16 {
        if let Phase::Monitoring { since, .. } = self.phase {
            if now.saturating_duration_since(since) >= REDISCOVER_AFTER {
                self.paths.clear();
                self.sent = 0;
                self.phase = Phase::Discovering;
                self.pass += 1;
            }
        }
        if let Phase::Discovering = self.phase {
            if self.sent < MAX_FLOWS && !self.sure(last_ttl) {
                self.sent += 1;
                self.widest = self.widest.max(self.sent);
                return self.sent - 1;
            }
            self.phase = Phase::Monitoring {
                turn: 0,
                since: now,
            };
        }
        let cover = self.cover(last_ttl);
        let Phase::Monitoring { turn, .. } = &mut self.phase else {
            unreachable!("discovery ended above");
        };
        let flow = cover.get(*turn % cover.len().max(1)).copied().unwrap_or(0);
        *turn += 1;
        flow
    }

    /// Record that the probe of `flow` to `ttl` was answered by `from`.
    pub fn answered(&mut self, flow: u16, ttl: u8, from: IpAddr) {
        self.paths.entry(flow).or_default().insert(ttl, from);
    }

    pub fn discovering(&self) -> bool {
        matches!(self.phase, Phase::Discovering)
    }

    /// The flows probed so far, for the flow matrix: 0 up to here.
    pub fn width(&self) -> u16 {
        self.widest.max(1)
    }

    /// Every hop up to `last_ttl` that answered in this discovery
    pub fn hops(&self, last_ttl: u8) -> Vec<Branches> {
        let mut hops: BTreeMap<u8, Branches> = BTreeMap::new();
        for path in self.paths.values() {
            for (&ttl, &addr) in path.range(..=last_ttl) {
                let hop = hops.entry(ttl).or_insert_with(|| Branches {
                    ttl,
                    addrs: BTreeSet::new(),
                    flows: 0,
                });
                hop.addrs.insert(addr);
                hop.flows += 1;
            }
        }
        hops.into_values().collect()
    }

    /// Every hop that answered has had flows enough to rule out another branch.
    fn sure(&self, last_ttl: u8) -> bool {
        let hops = self.hops(last_ttl);
        !hops.is_empty() && hops.iter().all(Branches::sure)
    }

    /// The fewest flows that together pass every address found (greedily: the
    /// flow passing most addresses not yet passed, then the next), in order.
    pub fn cover(&self, last_ttl: u8) -> Vec<u16> {
        let mut left: BTreeSet<(u8, IpAddr)> = self
            .paths
            .values()
            .flat_map(|path| path.range(..=last_ttl).map(|(&ttl, &addr)| (ttl, addr)))
            .collect();
        let mut cover = Vec::new();
        while !left.is_empty() {
            let best = self
                .paths
                .iter()
                .map(|(&flow, path)| {
                    let new = path
                        .range(..=last_ttl)
                        .filter(|&(&ttl, &addr)| left.contains(&(ttl, addr)))
                        .count();
                    (new, Reverse(flow))
                })
                .max();
            let Some((_, Reverse(flow))) = best.filter(|&(new, _)| new > 0) else {
                break;
            };
            for (&ttl, &addr) in self.paths[&flow].range(..=last_ttl) {
                left.remove(&(ttl, addr));
            }
            cover.push(flow);
        }
        cover.sort_unstable();
        cover
    }

    /// The TTLs h where branches cross between h and h + 1: one address there
    /// leads to several, and one address at h + 1 is reached from several.
    pub fn crossings(&self, last_ttl: u8) -> Vec<u8> {
        (1..last_ttl)
            .filter(|&ttl| {
                let links: BTreeSet<(IpAddr, IpAddr)> = self
                    .paths
                    .values()
                    .filter_map(|path| Some((*path.get(&ttl)?, *path.get(&(ttl + 1))?)))
                    .collect();
                let mut out: HashMap<IpAddr, usize> = HashMap::new();
                let mut into: HashMap<IpAddr, usize> = HashMap::new();
                for &(from, to) in &links {
                    *out.entry(from).or_default() += 1;
                    *into.entry(to).or_default() += 1;
                }
                out.values().any(|&n| n > 1) && into.values().any(|&n| n > 1)
            })
            .collect()
    }

    /// A line for under hop `ttl`: how many branches it has and how sure that is.
    /// Nothing for a hop with one address that answered enough flows, nor during
    /// discovery for one that may yet.
    pub fn note(&self, ttl: u8, last_ttl: u8) -> Option<String> {
        let hop = self.hops(last_ttl).into_iter().find(|hop| hop.ttl == ttl)?;
        let quiet = hop.addrs.len() == 1 && (hop.sure() || self.discovering());
        (!quiet).then(|| hop.describe())
    }

    /// For the status line
    pub fn status(&self, last_ttl: u8) -> String {
        if self.discovering() {
            let what = if self.pass == 1 {
                "finding"
            } else {
                "rechecking"
            };
            return format!("{what} branches: flow {}", self.sent);
        }
        let balanced = self
            .hops(last_ttl)
            .iter()
            .filter(|hop| hop.addrs.len() > 1)
            .count();
        let flows = self.cover(last_ttl).len();
        let s = |n: usize| if n == 1 { "" } else { "s" };
        match balanced {
            0 => "no load balancing".to_string(),
            n => format!("load balanced at {n} hop{}, {flows} flow{}", s(n), s(flows)),
        }
    }

    /// What discovery found, for the end of a report.
    pub fn summary(&self, last_ttl: u8) -> Vec<String> {
        let hops = self.hops(last_ttl);
        let mut lines = Vec::new();
        if self.discovering() {
            let furthest = hops
                .iter()
                .filter(|hop| !hop.sure())
                .max_by_key(|hop| (hop.needed() - hop.flows, Reverse(hop.ttl)));
            match furthest {
                Some(hop) => lines.push(format!(
                    "Branches not all found yet: after {} flows, hop {} has {}. \
                     Leave out -c, or raise it, to let discovery finish.",
                    self.sent,
                    hop.ttl,
                    hop.describe()
                )),
                None => lines.push(format!(
                    "Branches not all found yet: {} flows sent, no answers.",
                    self.sent
                )),
            }
        } else {
            let balanced: Vec<String> = hops
                .iter()
                .filter(|hop| hop.addrs.len() > 1)
                .map(|hop| format!("hop {} ({} branches)", hop.ttl, hop.addrs.len()))
                .collect();
            let cover = self.cover(last_ttl);
            lines.push(match balanced.len() {
                0 => format!(
                    "No load balancing found: {} flows met one address at every hop.",
                    self.sent
                ),
                _ => format!(
                    "Load balanced at {}. {} flows found the branches; then flow{} {}, \
                     which together pass every address, took turns.",
                    join(&balanced),
                    self.sent,
                    if cover.len() == 1 { "" } else { "s" },
                    topology::ranges(&cover.into_iter().collect())
                ),
            });
            for hop in hops.iter().filter(|hop| !hop.sure()) {
                lines.push(format!(
                    "Hop {}: {}, too few to rule out another branch.",
                    hop.ttl,
                    hop.describe()
                ));
            }
        }
        for ttl in self.crossings(last_ttl) {
            lines.push(format!(
                "Branches cross between hops {ttl} and {}, so one there may have been missed.",
                ttl + 1
            ));
        }
        lines.push(
            "Per hop 95% sure that no branch was missed, if balancers spread flows evenly \
             (MDA-Lite)."
                .to_string(),
        );
        lines
    }
}

/// "a", "a and b", "a, b and c"
fn join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(ttl: u8, n: u8) -> IpAddr {
        IpAddr::from([10, 0, ttl, n])
    }

    /// Discovery against a path where `ways[ttl - 1]` branches answer at each TTL,
    /// flow f meeting branch f mod ways, until it stops sending new flows.
    fn discover(ways: &[u8]) -> (Mda, Instant) {
        let mut mda = Mda::new();
        let now = Instant::now();
        let last = ways.len() as u8;
        while mda.discovering() {
            let flow = mda.next_flow(now, last);
            if !mda.discovering() {
                break;
            }
            for (ttl, &n) in (1..).zip(ways) {
                mda.answered(flow, ttl, addr(ttl, (flow % u16::from(n)) as u8));
            }
        }
        (mda, now)
    }

    #[test]
    fn the_stopping_points_are_mda_lites() {
        let points: Vec<usize> = (1..=11).map(needed).collect();
        assert_eq!(points, [6, 11, 16, 21, 27, 33, 38, 44, 51, 57, 63]);
        assert!(
            needed(12) > usize::from(MAX_FLOWS),
            "12 branches can't be confirmed"
        );
    }

    #[test]
    fn discovery_sends_flows_until_every_hop_is_sure() {
        let (mda, _) = discover(&[1, 1]);
        assert_eq!(mda.sent, 6, "6 flows rule out a second branch");

        let (mda, _) = discover(&[1, 4, 2, 1]);
        assert_eq!(mda.sent, 21, "4 branches need 21 flows");
        let hops = mda.hops(4);
        assert_eq!(hops[1].addrs.len(), 4);
        assert!(hops.iter().all(Branches::sure));
        assert_eq!(mda.cover(4), [0, 1, 2, 3], "four flows pass every address");
    }

    #[test]
    fn monitoring_rotates_the_cover_and_rediscovers_later() {
        let (mut mda, now) = discover(&[1, 3, 1]);
        // discover() already took flow 0's turn, when discovery ended
        let flows: Vec<u16> = (0..6).map(|_| mda.next_flow(now, 3)).collect();
        assert_eq!(flows, [1, 2, 0, 1, 2, 0]);
        assert_eq!(mda.status(3), "load balanced at 1 hop, 3 flows");

        let later = now + REDISCOVER_AFTER;
        assert_eq!(mda.next_flow(later, 3), 0, "discovery starts over");
        assert!(mda.discovering());
        assert_eq!(mda.status(3), "rechecking branches: flow 1");
        assert_eq!(
            mda.width(),
            16,
            "the matrix keeps the flows of the first pass"
        );
    }

    #[test]
    fn a_quiet_hop_ends_discovery_unsure_when_the_flows_run_out() {
        let mut mda = Mda::new();
        let now = Instant::now();
        while mda.discovering() {
            let flow = mda.next_flow(now, 2);
            mda.answered(flow, 1, addr(1, 0));
            if flow.is_multiple_of(20) {
                mda.answered(flow, 2, addr(2, 0)); // rate-limited: 1 in 20
            }
        }
        assert_eq!(mda.sent, MAX_FLOWS);
        assert_eq!(mda.note(1, 2), None, "hop 1 is sure");
        assert_eq!(mda.note(2, 2).unwrap(), "1 branch so far, 4 of 6 flows");
        assert!(mda.summary(2)[1].starts_with("Hop 2: 1 branch so far, 4 of 6 flows"));
    }

    #[test]
    fn notes_say_how_many_branches_and_how_sure() {
        let mut mda = Mda::new();
        let now = Instant::now();
        for flow in 0..5 {
            assert_eq!(mda.next_flow(now, 2), flow);
            mda.answered(flow, 1, addr(1, 0));
            mda.answered(flow, 2, addr(2, (flow % 2) as u8));
        }
        assert_eq!(mda.note(1, 2), None, "one address, discovery not done");
        assert_eq!(mda.note(2, 2).unwrap(), "2 branches so far, 5 of 11 flows");
        assert_eq!(mda.status(2), "finding branches: flow 5");

        let (mda, _) = discover(&[1, 2, 1]);
        assert_eq!(mda.note(2, 3).unwrap(), "2 branches, all found (95%)");
        assert_eq!(mda.note(3, 3), None);
        let summary = mda.summary(3);
        assert_eq!(
            summary[0],
            "Load balanced at hop 2 (2 branches). 11 flows found the branches; \
             then flows 0-1, which together pass every address, took turns."
        );
    }

    #[test]
    fn crossing_branches_are_noticed() {
        // Two branches at hop 2, three at hop 3, chosen independently: flow 0
        // meets a then a, flow 3 b then a, so a at hop 3 is reached from both.
        let (mda, _) = discover(&[1, 2, 3, 1]);
        assert_eq!(mda.crossings(4), [2]);
        // The same branch all the way: a then a, b then b
        let (mda, _) = discover(&[1, 2, 2, 1]);
        assert!(mda.crossings(4).is_empty());
        assert_eq!(mda.cover(4), [0, 1]);
    }

    #[test]
    fn hops_past_the_end_of_the_path_are_left_out() {
        let mut mda = Mda::new();
        let now = Instant::now();
        let flow = mda.next_flow(now, 30);
        // The first round reaches the target from TTL 3 on
        for ttl in 1..=5 {
            mda.answered(flow, ttl, addr(ttl.min(3), 0));
        }
        assert_eq!(mda.hops(3).len(), 3);
        assert_eq!(mda.hops(30).len(), 5);
    }
}
