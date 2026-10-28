//! The routes a trace saw, flow by flow (`--flows`). Every probe of a flow takes
//! the same path, so the addresses one flow met, TTL by TTL, form a route. Where
//! flows part and meet again sits a load balancer.

use crate::hop_stats::{FlowStats, HopStats};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    net::IpAddr,
};

/// Letters for the addresses that answered at a hop, most replies first.
pub fn letters(hop: &HopStats) -> Vec<(char, IpAddr)> {
    let mut responders: Vec<_> = hop.responders.values().collect();
    responders.sort_by_key(|r| (Reverse(r.replies), r.addr));
    ('A'..='Z').zip(responders.iter().map(|r| r.addr)).collect()
}

/// What one flow met at one hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cell {
    /// Always the responder with this letter
    One(char),
    /// Several responders: the route changed, or a router balances per packet
    Mixed,
    /// Probed, never answered
    Silent,
    /// Not probed in this flow (yet)
    Unprobed,
}

impl Cell {
    pub fn symbol(self) -> char {
        match self {
            Cell::One(letter) => letter,
            Cell::Mixed => '*',
            Cell::Silent => '·',
            Cell::Unprobed => ' ',
        }
    }
}

/// A hop's row of the flow matrix: one cell per flow, `0..flows`.
pub fn row(hop: &HopStats, flows: u16) -> Vec<Cell> {
    let letters = letters(hop);
    let letter_of = |addr| {
        letters
            .iter()
            .find(|&&(_, a)| a == addr)
            .map_or('?', |&(letter, _)| letter)
    };
    (0..flows)
        .map(|flow| match hop.flows.get(&flow) {
            None => Cell::Unprobed,
            Some(stats) => match (stats.responders.len(), stats.responder()) {
                (1, Some(addr)) => Cell::One(letter_of(addr)),
                (0, _) => Cell::Silent,
                _ => Cell::Mixed,
            },
        })
        .collect()
}

/// The flow matrix as text, for the report: a header, then per hop its cells and
/// which address each letter stands for.
pub fn matrix_lines(hops: &[HopStats], flows: u16, numeric: bool) -> Vec<String> {
    let width = usize::from(flows) * 2;
    let mut lines = vec![format!(
        "Flows  {:<width$} * several addresses, · no reply",
        flow_header(flows)
    )];
    for hop in hops.iter().filter(|hop| hop.sent > 0) {
        let cells: String = row(hop, flows)
            .into_iter()
            .map(|cell| format!("{} ", cell.symbol()))
            .collect();
        let legend: Vec<String> = letters(hop)
            .into_iter()
            .map(|(letter, addr)| format!("{letter} {}", name(hop, addr, numeric)))
            .collect();
        let line = format!("{:2}.|-- {cells:<width$} {}", hop.hop, legend.join("  "));
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// Flow numbers, one per two-character column (`0 1 2 ... 9 0 1`).
pub fn flow_header(flows: u16) -> String {
    (0..flows).map(|flow| format!("{} ", flow % 10)).collect()
}

/// Name of `addr` as answered at `hop`, or the address itself.
fn name(hop: &HopStats, addr: IpAddr, numeric: bool) -> String {
    hop.responders
        .get(&addr)
        .and_then(|r| r.hostname.clone())
        .filter(|_| !numeric)
        .unwrap_or_else(|| addr.to_string())
}

/// An address seen at a TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Node {
    pub ttl: u8,
    pub addr: IpAddr,
}

/// Consecutive responders of some flows. `from` is `None` for the first hop that
/// answered, which starts at the source host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: Option<Node>,
    pub to: Node,
    pub flows: BTreeSet<u16>,
}

impl Edge {
    /// Silent TTLs this edge skips.
    pub fn gap(&self) -> u8 {
        let from = self.from.map_or(0, |node| node.ttl);
        self.to.ttl.saturating_sub(from + 1)
    }
}

/// The route graph: per flow, its most frequent responder at each TTL, linked in
/// order. Silent TTLs are skipped, so an edge can span several TTLs.
pub fn edges(hops: &[HopStats]) -> Vec<Edge> {
    let flows: BTreeSet<u16> = hops
        .iter()
        .flat_map(|hop| hop.flows.keys().copied())
        .collect();
    let mut edges: BTreeMap<(Option<Node>, Node), BTreeSet<u16>> = BTreeMap::new();
    for &flow in &flows {
        let mut previous = None;
        for hop in hops {
            let Some(addr) = hop.flows.get(&flow).and_then(FlowStats::responder) else {
                continue;
            };
            let node = Node { ttl: hop.hop, addr };
            edges.entry((previous, node)).or_default().insert(flow);
            previous = Some(node);
        }
    }
    edges
        .into_iter()
        .map(|((from, to), flows)| Edge { from, to, flows })
        .collect()
}

/// `0-3, 6, 9-10`
pub fn ranges(flows: &BTreeSet<u16>) -> String {
    let mut parts: Vec<(u16, u16)> = Vec::new();
    for &flow in flows {
        match parts.last_mut() {
            Some((_, end)) if *end + 1 == flow => *end = flow,
            _ => parts.push((flow, flow)),
        }
    }
    let part = |&(start, end): &(u16, u16)| match start == end {
        true => start.to_string(),
        false => format!("{start}-{end}"),
    };
    parts.iter().map(part).collect::<Vec<_>>().join(", ")
}

/// Graphviz source for the route graph. Edges are labelled with their flows
/// unless every flow takes them; dashed edges skip silent TTLs.
pub fn to_dot(hops: &[HopStats], source: &str, numeric: bool) -> String {
    let quote = |text: &str| {
        let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{}\"", escaped.replace('\n', "\\n"))
    };
    let id = |node: Option<Node>| match node {
        Some(node) => quote(&format!("{}:{}", node.ttl, node.addr)),
        None => "source".to_string(),
    };
    let edges = edges(hops);
    let all_flows: BTreeSet<u16> = edges.iter().flat_map(|e| e.flows.iter().copied()).collect();

    let mut dot = String::from("digraph mtr_ng {\n  rankdir=LR;\n  node [shape=box];\n");
    let _ = writeln!(dot, "  source [label={}];", quote(source));
    let nodes: BTreeSet<Node> = edges.iter().map(|e| e.to).collect();
    for node in nodes {
        let hop = hops.iter().find(|hop| hop.hop == node.ttl);
        let name = hop.map_or_else(
            || node.addr.to_string(),
            |hop| name(hop, node.addr, numeric),
        );
        let label = match name == node.addr.to_string() {
            true => format!("{}. {}", node.ttl, node.addr),
            false => format!("{}. {name}\n{}", node.ttl, node.addr),
        };
        let _ = writeln!(dot, "  {} [label={}];", id(Some(node)), quote(&label));
    }
    for edge in &edges {
        let mut attributes = Vec::new();
        let mut label = Vec::new();
        if edge.flows != all_flows {
            label.push(format!("flows {}", ranges(&edge.flows)));
        }
        if edge.gap() > 0 {
            label.push(format!("+{} silent", edge.gap()));
            attributes.push("style=dashed".to_string());
        }
        if !label.is_empty() {
            attributes.push(format!("label={}", quote(&label.join("\n"))));
        }
        let attributes = match attributes.is_empty() {
            true => String::new(),
            false => format!(" [{}]", attributes.join(", ")),
        };
        let _ = writeln!(
            dot,
            "  {} -> {}{attributes};",
            id(edge.from),
            id(Some(edge.to))
        );
    }
    dot.push_str("}\n");
    dot
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn addr(ttl: u8, n: u8) -> IpAddr {
        IpAddr::from([10, 0, ttl, n])
    }

    /// Four flows over five TTLs: a diamond at TTLs 2-3 (even flows on .1, odd on
    /// .2), TTL 4 silent, and flow 3 mixed at TTL 5.
    fn diamond() -> Vec<HopStats> {
        (1..=5)
            .map(|ttl| {
                let mut hop = HopStats::new(ttl);
                for flow in 0..4u16 {
                    for round in 0..2u8 {
                        let ordinal = hop.increment_sent(flow);
                        let n = match ttl {
                            2 | 3 => 1 + (flow % 2) as u8,
                            5 if flow == 3 => 1 + round,
                            _ => 1,
                        };
                        if ttl == 4 {
                            hop.record_timeout(Some(ordinal));
                        } else {
                            let rtt = Duration::from_millis(u64::from(ttl));
                            hop.record_reply(Some(ordinal), Some(addr(ttl, n)), rtt);
                        }
                    }
                }
                hop
            })
            .collect()
    }

    #[test]
    fn the_matrix_shows_branches_silence_and_mixed_flows() {
        let hops = diamond();
        let symbols =
            |ttl: usize| -> String { row(&hops[ttl - 1], 4).iter().map(|c| c.symbol()).collect() };
        assert_eq!(symbols(1), "AAAA");
        assert_eq!(symbols(2), "ABAB");
        assert_eq!(symbols(4), "····");
        assert_eq!(symbols(5), "AAA*");
        assert_eq!(row(&hops[0], 6)[5], Cell::Unprobed);
    }

    #[test]
    fn edges_follow_each_flow_and_skip_silent_hops() {
        let edges = edges(&diamond());
        let find = |from: Option<(u8, u8)>, to: (u8, u8)| {
            edges
                .iter()
                .find(|e| {
                    e.from
                        == from.map(|(t, n)| Node {
                            ttl: t,
                            addr: addr(t, n),
                        })
                        && e.to
                            == Node {
                                ttl: to.0,
                                addr: addr(to.0, to.1),
                            }
                })
                .map(|e| e.flows.iter().copied().collect::<Vec<_>>())
        };
        assert_eq!(find(None, (1, 1)), Some(vec![0, 1, 2, 3]));
        assert_eq!(find(Some((1, 1)), (2, 1)), Some(vec![0, 2]));
        assert_eq!(find(Some((1, 1)), (2, 2)), Some(vec![1, 3]));
        assert_eq!(find(Some((2, 1)), (3, 2)), None, "flows don't cross over");
        let over_silence = edges.iter().find(|e| e.to.ttl == 5).unwrap();
        assert_eq!(over_silence.gap(), 1);
    }

    #[test]
    fn dot_labels_partial_flows_and_dashes_gaps() {
        let dot = to_dot(&diamond(), "laptop", true);
        assert!(dot.starts_with("digraph mtr_ng {"), "{dot}");
        assert!(
            dot.contains("source -> \"1:10.0.1.1\";"),
            "shared by all flows: {dot}"
        );
        assert!(
            dot.contains("\"1:10.0.1.1\" -> \"2:10.0.2.2\" [label=\"flows 1, 3\"];"),
            "{dot}"
        );
        assert!(dot.contains("style=dashed"), "{dot}");
        assert!(dot.contains("+1 silent"), "{dot}");
        assert!(dot.contains("[label=\"1. 10.0.1.1\"]"), "{dot}");
        let named = to_dot(&diamond(), "say \"hi\"", true).replace("10.0.1.1", "x");
        assert!(
            named.contains("source [label=\"say \\\"hi\\\"\"];"),
            "{named}"
        );
        assert!(!dot.lines().any(|l| l.ends_with('\n') || l.contains("\n\n")));
    }

    #[test]
    fn flow_ranges_are_compact() {
        let set = |v: &[u16]| v.iter().copied().collect::<BTreeSet<u16>>();
        assert_eq!(ranges(&set(&[0, 1, 2, 3, 6, 9, 10])), "0-3, 6, 9-10");
        assert_eq!(ranges(&set(&[5])), "5");
    }

    #[test]
    fn matrix_lines_have_a_legend() {
        let lines = matrix_lines(&diamond(), 4, true);
        assert!(lines[0].starts_with("Flows  0 1 2 3"), "{lines:?}");
        assert!(lines[1].starts_with(" 1.|-- A A A A"), "{lines:?}");
        assert!(
            lines[2].contains("A B A B") && lines[2].contains("A 10.0.2.1  B 10.0.2.2"),
            "{lines:?}"
        );
    }
}
