//! The routes a trace saw, flow by flow (`--flows`). Every probe of a flow takes
//! the same path, so the addresses one flow met, TTL by TTL, form a route. Where
//! flows part and meet again sits a load balancer.

use crate::hop_stats::{FlowStats, HopStats};
use crate::privacy::show;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    net::IpAddr,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// Whether some TTL has more than one route through it.
pub fn branched(hops: &[HopStats]) -> bool {
    let nodes: BTreeSet<Node> = edges(hops).into_iter().map(|e| e.to).collect();
    nodes
        .iter()
        .zip(nodes.iter().skip(1))
        .any(|(a, b)| a.ttl == b.ttl)
}

/// Name of the responder `node`, from the hop it answered at.
pub fn node_name(hops: &[HopStats], node: Node, numeric: bool) -> String {
    hops.iter()
        .find(|hop| hop.hop == node.ttl)
        .map_or_else(|| show(node.addr), |hop| name(hop, node.addr, numeric))
}

/// Name of `addr` as answered at `hop`, or the address itself.
fn name(hop: &HopStats, addr: IpAddr, numeric: bool) -> String {
    hop.responders
        .get(&addr)
        .and_then(|r| r.hostname.clone())
        .filter(|_| !numeric)
        .unwrap_or_else(|| show(addr))
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
        let addr = show(node.addr);
        let name = hop.map_or_else(|| addr.clone(), |hop| name(hop, node.addr, numeric));
        let label = match name == addr {
            true => format!("{}. {addr}", node.ttl),
            false => format!("{}. {name}\n{addr}", node.ttl),
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

/// Mermaid source for the route graph, for Markdown that renders it (GitHub,
/// GitLab, many wikis). Labelled and dashed as `to_dot` does.
pub fn to_mermaid(hops: &[HopStats], source: &str, numeric: bool) -> String {
    // Mermaid wants plain identifiers and its own escape for quotes
    let quote = |text: &str| format!("\"{}\"", text.replace('"', "#quot;"));
    let edges = edges(hops);
    let nodes: BTreeSet<Node> = edges.iter().map(|e| e.to).collect();
    let ids: BTreeMap<Node, String> = nodes
        .iter()
        .enumerate()
        .map(|(i, &node)| (node, format!("n{}_{i}", node.ttl)))
        .collect();
    let all_flows: BTreeSet<u16> = edges.iter().flat_map(|e| e.flows.iter().copied()).collect();

    let mut out = String::from("flowchart TD\n");
    let _ = writeln!(out, "  source[{}]", quote(source));
    for node in &nodes {
        let hop = hops.iter().find(|hop| hop.hop == node.ttl);
        let addr = show(node.addr);
        let name = hop.map_or_else(|| addr.clone(), |hop| name(hop, node.addr, numeric));
        let label = match name == addr {
            true => format!("{}. {addr}", node.ttl),
            false => format!("{}. {name}<br/>{addr}", node.ttl),
        };
        let _ = writeln!(out, "  {}[{}]", ids[node], quote(&label));
    }
    for edge in &edges {
        let from = edge.from.map_or("source", |node| &ids[&node]);
        let mut label = Vec::new();
        if edge.flows != all_flows {
            label.push(format!("flows {}", ranges(&edge.flows)));
        }
        if edge.gap() > 0 {
            label.push(format!("+{} silent", edge.gap()));
        }
        let arrow = match edge.gap() {
            0 => "-->",
            _ => "-.->",
        };
        let label = match label.is_empty() {
            true => String::new(),
            false => format!("|{}|", quote(&label.join("<br/>"))),
        };
        let _ = writeln!(out, "  {from} {arrow}{label} {}", ids[&edge.to]);
    }
    out
}

/// One line of a drawn route graph, and the TTL whose responders it shows; `None`
/// for the lines that join them.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawnLine {
    pub ttl: Option<u8>,
    pub text: String,
}

/// Directions a box-drawing cell connects to.
const UP: u8 = 1;
const DOWN: u8 = 2;
const RIGHT: u8 = 4;
const LEFT: u8 = 8;

#[derive(Clone, Copy)]
enum Drawn {
    Blank,
    Text(char),
    /// Directions, and whether only edges over silent TTLs pass
    Lines(u8, bool),
}

impl Drawn {
    fn add(&mut self, directions: u8, dashed: bool) {
        *self = match *self {
            Drawn::Blank => Drawn::Lines(directions, dashed),
            Drawn::Lines(had, was_dashed) => Drawn::Lines(had | directions, was_dashed && dashed),
            text => text,
        };
    }

    fn symbol(self) -> char {
        let Drawn::Lines(d, dashed) = self else {
            return match self {
                Drawn::Text(c) => c,
                _ => ' ',
            };
        };
        match (d & UP != 0, d & DOWN != 0, d & RIGHT != 0, d & LEFT != 0) {
            (true, true, false, false) if dashed => '┆',
            (_, _, false, false) => '│',
            (false, false, _, _) => '─',
            (false, true, true, false) => '┌',
            (false, true, false, true) => '┐',
            (true, false, true, false) => '└',
            (true, false, false, true) => '┘',
            (true, true, true, false) => '├',
            (true, true, false, true) => '┤',
            (false, true, true, true) => '┬',
            (true, false, true, true) => '┴',
            (true, true, true, true) => '┼',
        }
    }
}

/// The route graph in box-drawing characters, top to bottom: a line per TTL with
/// its responders side by side, named by `name`, and between them lines for the
/// edges that join them. Edges over silent TTLs are dashed. Each responder goes
/// under the responders it follows, as far as it can; the lanes share `width`
/// columns, and names longer than their lane are cut. Where branches cross, one
/// line can stand for several edges.
pub fn draw(hops: &[HopStats], name: &dyn Fn(Node) -> String, width: usize) -> Vec<DrawnLine> {
    let edges = edges(hops);
    let Some(last) = edges.iter().map(|e| e.to.ttl).max() else {
        return Vec::new();
    };
    // Lanes, TTL by TTL: a responder goes to the mean lane of those it follows
    let mut lane: BTreeMap<Node, usize> = BTreeMap::new();
    for ttl in 1..=last {
        let Some(hop) = hops.iter().find(|hop| hop.hop == ttl) else {
            continue;
        };
        let order: Vec<IpAddr> = letters(hop).into_iter().map(|(_, addr)| addr).collect();
        let mut nodes: Vec<(f64, usize, Node)> = edges
            .iter()
            .filter(|e| e.to.ttl == ttl)
            .map(|e| e.to)
            .collect::<BTreeSet<Node>>()
            .into_iter()
            .map(|node| {
                let parents: Vec<usize> = edges
                    .iter()
                    .filter(|e| e.to == node)
                    .filter_map(|e| lane.get(&e.from?).copied())
                    .collect();
                let mean = match parents.len() {
                    0 => 0.0,
                    n => parents.iter().sum::<usize>() as f64 / n as f64,
                };
                let rank = order
                    .iter()
                    .position(|&a| a == node.addr)
                    .unwrap_or(usize::MAX);
                (mean, rank, node)
            })
            .collect();
        nodes.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        for (i, (_, _, node)) in nodes.into_iter().enumerate() {
            lane.insert(node, i);
        }
    }
    let lanes = lane.values().max().map_or(1, |&max| max + 1);
    let names: BTreeMap<Node, String> = lane.keys().map(|&node| (node, name(node))).collect();
    let longest = names.values().map(|n| n.width()).max().unwrap_or(0);
    let margin = last.to_string().len() + 2;
    let lane_width = (longest + 2)
        .min(width.saturating_sub(margin) / lanes)
        .max(4);
    let x = |node: &Node| margin + lane[node] * lane_width;

    // Line 2(t-1) shows TTL t, line 2(t-1)+1 joins it to what follows
    let width = margin + lanes * lane_width;
    let mut grid = vec![vec![Drawn::Blank; width]; usize::from(last) * 2 - 1];
    let row = |ttl: u8| usize::from(ttl - 1) * 2;
    for ttl in 1..=last {
        for (i, c) in format!("{ttl:>w$}", w = margin - 2).chars().enumerate() {
            grid[row(ttl)][i] = Drawn::Text(c);
        }
    }
    for (node, text) in &names {
        let mut used = 0;
        let chars = text.chars().take_while(|c| {
            used += c.width().unwrap_or(0);
            used < lane_width
        });
        for (i, c) in chars.enumerate() {
            grid[row(node.ttl)][x(node) + i] = Drawn::Text(c);
        }
    }
    for edge in &edges {
        let Some(from) = edge.from else { continue };
        let (x1, x2) = (x(&from), x(&edge.to));
        let below = row(from.ttl) + 1;
        let (toward, back) = match x2.cmp(&x1) {
            std::cmp::Ordering::Greater => (RIGHT, LEFT),
            std::cmp::Ordering::Less => (LEFT, RIGHT),
            std::cmp::Ordering::Equal => (DOWN, UP),
        };
        if x1 == x2 {
            grid[below][x1].add(UP | DOWN, false);
        } else {
            grid[below][x1].add(UP | toward, false);
            for cell in &mut grid[below][x1.min(x2) + 1..x1.max(x2)] {
                cell.add(LEFT | RIGHT, false);
            }
            grid[below][x2].add(DOWN | back, false);
        }
        for line in &mut grid[below + 1..row(edge.to.ttl)] {
            line[x2].add(UP | DOWN, edge.gap() > 0);
        }
    }
    grid.into_iter()
        .enumerate()
        .map(|(i, cells)| DrawnLine {
            ttl: (i % 2 == 0).then(|| (i / 2 + 1) as u8),
            text: cells
                .into_iter()
                .map(Drawn::symbol)
                .collect::<String>()
                .trim_end()
                .to_string(),
        })
        .collect()
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
    fn the_route_graph_is_drawn_lane_by_lane() {
        let hops = diamond();
        let name = |node: Node| node.addr.to_string();
        let lines = draw(&hops, &name, 30);
        let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            text,
            [
                "1  10.0.1.1",
                "   ├─────────┐",
                "2  10.0.2.1  10.0.2.2",
                "   │         │",
                "3  10.0.3.1  10.0.3.2",
                "   ├─────────┘",
                "4  ┆",
                "   ┆",
                "5  10.0.5.1",
            ]
        );
        let ttls: Vec<Option<u8>> = lines.iter().map(|l| l.ttl).collect();
        assert_eq!(ttls[..3], [Some(1), None, Some(2)]);

        let narrow = draw(&hops, &name, 15);
        assert_eq!(narrow[2].text, "2  10.0. 10.0.", "names cut to their lane");
        assert!(draw(&[], &name, 30).is_empty());
    }

    #[test]
    fn mermaid_labels_partial_flows_and_dashes_gaps() {
        let mermaid = to_mermaid(&diamond(), "say \"hi\"", true);
        assert!(
            mermaid.starts_with("flowchart TD\n  source[\"say #quot;hi#quot;\"]\n"),
            "{mermaid}"
        );
        assert!(mermaid.contains("  n1_0[\"1. 10.0.1.1\"]"), "{mermaid}");
        assert!(
            mermaid.contains("  source --> n1_0\n"),
            "every flow: {mermaid}"
        );
        assert!(
            mermaid.contains("  n1_0 -->|\"flows 1, 3\"| n2_2\n"),
            "{mermaid}"
        );
        assert!(
            mermaid.contains("  n3_3 -.->|\"flows 0, 2<br/>+1 silent\"| n5_5\n"),
            "{mermaid}"
        );
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
