//! Draws one frame: status line, hop table or flow matrix, RTT scale, and the help,
//! column or hop detail overlay.

use super::events::{Binding, COLUMN_KEYS, DETAIL_KEYS, TABLE_KEYS};
use super::map::render_map;
use super::state::{Mode, UiState, View};
use super::visualization::{
    gradient, histogram, histogram_spans, history_spans, letter_color, loss_color, symbol,
    ColorSupport, RttScale,
};
use crate::args::Column;
use crate::columns::{self, format_ms};
use crate::geo::{self, Origin};
use crate::hop_stats::ResponderStats;
use crate::icmp_ext::MplsLabel;
use crate::topology;
use crate::utils::network::truncate_hostname;
use crate::utils::text::pack;
use crate::{HopStats, MtrSession, SparklineScale};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Cell, Clear, List, ListItem, ListState, Padding, Paragraph, Row, Table, Wrap,
    },
    Frame,
};
use std::collections::BTreeSet;
use std::net::IpAddr;
use unicode_width::UnicodeWidthStr;

/// Smallest terminal the table makes sense in.
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 6;

/// While a graph shares the row, the host column gets at least this much (a full
/// IPv4 address) and the graph at least its header's width.
const HOST_MIN: usize = 15;

/// The flow matrix spaces its cells out while this much is left for the legend.
const LEGEND_MIN: usize = 24;

/// Widest the hop details get, and the widest RTT histogram in them.
const DETAIL_WIDTH: u16 = 100;
const HISTOGRAM_WIDTH: usize = 60;

pub fn render(f: &mut Frame, session: &MtrSession, ui: &mut UiState) {
    let area = f.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let text = format!(
            "Terminal too small ({}x{}, need {MIN_WIDTH}x{MIN_HEIGHT}). q quits.",
            area.width, area.height
        );
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
        return;
    }
    let [status, table, scale_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(area);

    let hops = &session.hops[..session.display_hop_count()];
    f.render_widget(Paragraph::new(status_line(session, hops, ui)), status);
    match ui.view {
        View::Hops => {
            let scale = RttScale::from_hops(hops, ui.scale);
            render_table(f, table, session, hops, scale.as_ref(), ui);
            render_scale(f, scale_area, scale.as_ref(), ui);
            if let Some(why) = session.silence() {
                let below_header = Rect {
                    y: table.y + 1,
                    height: table.height.saturating_sub(1),
                    ..table
                };
                let dim = Style::default().add_modifier(Modifier::DIM);
                let text = Paragraph::new(Line::styled(why, dim)).wrap(Wrap { trim: true });
                f.render_widget(text, below_header);
            }
        }
        View::Flows => {
            render_flows(f, table, session, hops, ui);
            render_flow_legend(f, scale_area, session);
        }
        View::Map => {
            let below = Rect {
                height: table.height + scale_area.height,
                ..table
            };
            render_map(f, below, session, hops, ui);
        }
    }
    match ui.mode {
        Mode::Table => {}
        Mode::Help => render_help(f, area),
        Mode::Columns => render_chooser(f, area, ui),
        Mode::Detail => render_detail(f, area, session, hops, ui),
    }
}

/// Remember which hop each row shows, and select the row `ui.follow` asks for.
pub(super) fn track_rows(ui: &mut UiState, row_hops: Vec<u8>) {
    if let Some(hop) = ui.follow.take() {
        if let Some(row) = row_hops.iter().position(|&h| h == hop) {
            ui.table.select(Some(row));
        }
    }
    ui.row_hops = row_hops;
}

fn status_line(session: &MtrSession, hops: &[HopStats], ui: &UiState) -> Line<'static> {
    let mut spans = Vec::new();
    if session.is_simulated() {
        let badge = Style::default()
            .fg(Color::Black)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD);
        spans.push(Span::styled(" SIMULATED ", badge));
        spans.push(Span::raw(" "));
    }
    let addr = session.target_addr.to_string();
    let target = if session.target == addr {
        addr
    } else {
        format!("{} ({addr})", session.target)
    };
    spans.push(Span::styled(
        target,
        Style::default().add_modifier(Modifier::BOLD),
    ));

    let rounds = session.hops.first().map_or(0, |hop| hop.sent);
    let mut facts = vec![format!("{} hops", hops.len()), format!("round {rounds}")];
    if session.args.flows > 1 {
        facts.push(format!("{} flows", session.args.flows));
    }
    if let Some(end) = hops.last().filter(|hop| hop.is_target) {
        facts.push(format!("{:.1}% loss at target", end.loss_percent));
    }
    if let Some(pmtu) = &session.pmtu {
        facts.push(pmtu.status());
    }
    let scale = match ui.scale {
        SparklineScale::Linear => "linear",
        SparklineScale::Logarithmic => "log",
    };
    let names = if shows_names(session, ui) {
        "names"
    } else {
        "IPs"
    };
    facts.push(format!("{scale} scale, {names}"));
    if ui.finished {
        facts.push("done".to_string());
    }
    facts.push("? help".to_string());
    spans.push(Span::raw(format!("  {}", facts.join(" · "))));
    Line::from(spans)
}

fn shows_names(session: &MtrSession, ui: &UiState) -> bool {
    ui.show_hostnames && !session.args.numeric
}

/// Host name, or the address when names are off or unknown.
fn host_label(name: Option<&String>, addr: Option<IpAddr>, names: bool) -> String {
    match (name, addr) {
        (Some(name), _) if names => name.clone(),
        (_, Some(addr)) => addr.to_string(),
        _ => "???".to_string(),
    }
}

/// What a table row stands for.
enum RowKind<'a> {
    Hop,
    /// One more address that answered for the hop
    Alternate(&'a ResponderStats),
    /// A line about the hop or alternate above it: an MPLS label (`-e`), or where
    /// the path MTU drops (`--pmtu`)
    Note,
}

struct HostRow<'a> {
    hop: &'a HopStats,
    host: String,
    kind: RowKind<'a>,
}

fn host_rows<'a>(session: &MtrSession, hops: &'a [HopStats], ui: &UiState) -> Vec<HostRow<'a>> {
    let names = shows_names(session, ui);
    let mut rows = Vec::new();
    for hop in hops.iter().filter(|hop| hop.sent > 0) {
        let mut host = host_label(hop.hostname.as_ref(), hop.addr, names);
        if hop.has_multiple_paths() && hop.addr.is_some() {
            host = format!("{host} ({:.0}%)", hop.get_primary_path_percentage());
        }
        rows.push(HostRow {
            hop,
            host,
            kind: RowKind::Hop,
        });
        if ui.show_mpls {
            rows.extend(mpls_rows(hop, hop.mpls()));
        }
        for alt in hop.get_alternate_paths() {
            let host = format!(
                "↳ {} ({:.0}%)",
                host_label(alt.hostname.as_ref(), Some(alt.addr), names),
                hop.get_path_percentage(alt)
            );
            rows.push(HostRow {
                hop,
                host,
                kind: RowKind::Alternate(alt),
            });
            if ui.show_mpls {
                rows.extend(mpls_rows(hop, &alt.mpls));
            }
        }
        if let Some(pmtu) = &session.pmtu {
            rows.extend(pmtu.notes_after(hop.hop).into_iter().map(|note| HostRow {
                hop,
                host: format!("  {note}"),
                kind: RowKind::Note,
            }));
        }
    }
    rows
}

/// A row per label, under the hop or alternate that reported them.
fn mpls_rows<'a>(hop: &'a HopStats, labels: &[MplsLabel]) -> Vec<HostRow<'a>> {
    labels
        .iter()
        .map(|label| HostRow {
            hop,
            host: format!("  {label}"),
            kind: RowKind::Note,
        })
        .collect()
}

fn cell_text(column: Column, row: &HostRow, origin: Option<&Origin>) -> String {
    match (column, &row.kind) {
        (Column::Host, _) => row.host.clone(),
        (Column::Graph, _) => String::new(),
        (Column::Hop, RowKind::Hop) => format!("{}.", row.hop.hop),
        (_, RowKind::Hop) => column.value(row.hop, origin),
        (Column::Geo, RowKind::Alternate(alt)) => alt.geo.as_ref().map_or(String::new(), |fix| {
            let assessment = origin.and_then(|o| geo::assess(fix, alt.best_rtt, o));
            let mark = if assessment.is_some_and(|a| a.impossible) {
                "!"
            } else {
                ""
            };
            format!("{}{mark}", fix.place())
        }),
        (Column::Last, RowKind::Alternate(alt)) => format_ms(alt.last_rtt),
        (Column::Avg, RowKind::Alternate(alt)) => format_ms(alt.avg_rtt()),
        (Column::Best, RowKind::Alternate(alt)) => format_ms(alt.best_rtt),
        (Column::Back, RowKind::Alternate(alt)) => {
            columns::routers_back(alt.routers_back(), row.hop.hop)
        }
        (Column::Asn, RowKind::Alternate(alt)) => {
            alt.asn.map_or("AS???".to_string(), |n| format!("AS{n}"))
        }
        _ => String::new(),
    }
}

/// Widths for the leading columns that fit in a table `total` cells wide. Columns
/// that don't fit are left off the end rather than squeezed, since a squeezed
/// number loses its leading digits. Numeric columns get their natural width, the
/// host column fits the longest name when it can, and the graph takes the rest.
fn layout(columns: &[Column], natural: &[usize], total: u16) -> Vec<u16> {
    let total = usize::from(total);
    let floor = |c: Column, w: usize| match c {
        Column::Host => w.min(HOST_MIN),
        _ => w,
    };
    let (mut used, mut shown) = (0, 0);
    for (&c, &w) in columns.iter().zip(natural) {
        let next = used + floor(c, w) + usize::from(shown > 0);
        if next > total {
            break;
        }
        (used, shown) = (next, shown + 1);
    }
    let columns = &columns[..shown];

    let fixed: usize = columns
        .iter()
        .zip(natural)
        .filter(|(c, _)| !matches!(c, Column::Host | Column::Graph))
        .map(|(_, w)| w)
        .sum();
    let free = total.saturating_sub(shown.saturating_sub(1) + fixed);
    let graph_min = columns
        .iter()
        .position(|&c| c == Column::Graph)
        .map(|i| natural[i]);
    let host = columns
        .iter()
        .position(|&c| c == Column::Host)
        .map_or(0, |i| match graph_min {
            Some(graph_min) => natural[i].min(free.saturating_sub(graph_min)),
            None => free,
        });
    let graph = free.saturating_sub(host);
    columns
        .iter()
        .zip(natural)
        .map(|(&c, &w)| match c {
            Column::Host => host,
            Column::Graph => graph,
            _ => w,
        })
        .map(|w| u16::try_from(w).unwrap_or(u16::MAX))
        .collect()
}

fn right(text: impl Into<String>) -> Line<'static> {
    Line::from(text.into()).alignment(Alignment::Right)
}

fn render_table(
    f: &mut Frame,
    area: Rect,
    session: &MtrSession,
    hops: &[HopStats],
    scale: Option<&RttScale>,
    ui: &mut UiState,
) {
    let rows = host_rows(session, hops, ui);
    track_rows(ui, rows.iter().map(|row| row.hop.hop).collect());
    let columns = &ui.columns;
    let origin = session.origin();
    let texts: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|&c| cell_text(c, row, origin.as_ref()))
                .collect()
        })
        .collect();
    let natural: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            texts
                .iter()
                .map(|cells| cells[i].width())
                .chain([c.header().width(), c.min_width()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let widths = layout(columns, &natural, area.width);
    let columns = &columns[..widths.len()];

    let header = Row::new(columns.iter().map(|&c| match c {
        Column::Host | Column::Asn | Column::Geo | Column::Graph => Line::from(c.header()),
        _ => right(c.header()),
    }))
    .style(Style::default().add_modifier(Modifier::BOLD));

    let body = rows.iter().zip(texts).map(|(row, cells)| {
        let cells = columns
            .iter()
            .zip(cells)
            .zip(&widths)
            .map(|((&c, text), &width)| match c {
                Column::Graph if matches!(row.kind, RowKind::Hop) => {
                    Cell::from(Line::from(history_spans(
                        row.hop,
                        scale,
                        ui.visualization,
                        ui.colors,
                        usize::from(width),
                    )))
                }
                Column::Host => Cell::from(truncate_hostname(&text, usize::from(width))),
                Column::Geo | Column::Dist if text.ends_with('!') => {
                    let line = Line::styled(text, Style::default().fg(loss_color(ui.colors)));
                    Cell::from(match c {
                        Column::Geo => line,
                        _ => line.alignment(Alignment::Right),
                    })
                }
                Column::Asn | Column::Geo => Cell::from(text),
                _ => Cell::from(right(text)),
            });
        let style = match row.kind {
            RowKind::Hop => Style::default(),
            RowKind::Alternate(_) | RowKind::Note => Style::default().add_modifier(Modifier::DIM),
        };
        Row::new(cells).style(style)
    });

    let table = Table::new(body, widths.iter().map(|&w| Constraint::Length(w)))
        .header(header)
        .column_spacing(1)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(table, area, &mut ui.table);
}

fn format_rtt(ms: f64) -> String {
    match ms {
        ms if ms < 1.0 => format!("{ms:.2}ms"),
        ms if ms < 10.0 => format!("{ms:.1}ms"),
        ms if ms < 1000.0 => format!("{ms:.0}ms"),
        ms => format!("{:.1}s", ms / 1000.0),
    }
}

/// The colour and bar ramp with RTT labels underneath, drawn from the same scale as
/// the table's history cells.
fn render_scale(f: &mut Frame, area: Rect, scale: Option<&RttScale>, ui: &UiState) {
    let Some(scale) = scale else {
        f.render_widget(Paragraph::new("Waiting for replies…"), area);
        return;
    };
    let width = usize::from(area.width).min(60);
    let ratio_at = |i: usize| i as f64 / (width - 1) as f64;
    let ramp: Vec<Span> = (0..width)
        .map(|i| {
            let ratio = ratio_at(i);
            Span::styled(
                symbol(ratio, ui.visualization).to_string(),
                Style::default().fg(gradient(ratio, ui.colors)),
            )
        })
        .collect();

    let mut labels = vec![' '; width];
    let mut free_from = 0;
    for step in 0..5 {
        let ratio = f64::from(step) / 4.0;
        let label: Vec<char> = format_rtt(scale.at(ratio)).chars().collect();
        let center = (ratio * (width - 1) as f64).round() as usize;
        let start = center
            .saturating_sub(label.len() / 2)
            .min(width.saturating_sub(label.len()));
        if start < free_from || start + label.len() > width {
            continue; // would overlap its neighbour
        }
        labels[start..start + label.len()].copy_from_slice(&label);
        free_from = start + label.len() + 1;
    }
    let labels: String = labels.into_iter().collect();
    f.render_widget(
        Paragraph::new(vec![Line::from(ramp), Line::from(labels)]),
        area,
    );
}

/// Which address each flow met at each hop: a letter per flow, the letters
/// spelled out on the right.
fn render_flows(
    f: &mut Frame,
    area: Rect,
    session: &MtrSession,
    hops: &[HopStats],
    ui: &mut UiState,
) {
    let flows = session.args.flows.max(1);
    let shown: Vec<&HopStats> = hops.iter().filter(|hop| hop.sent > 0).collect();
    track_rows(ui, shown.iter().map(|hop| hop.hop).collect());
    let names = shows_names(session, ui);

    let numbers: Vec<String> = shown.iter().map(|hop| format!("{}.", hop.hop)).collect();
    let hop_width = numbers.iter().map(|n| n.width()).max().unwrap_or(0).max(3);
    let spaced = hop_width + usize::from(flows) * 2 + LEGEND_MIN <= usize::from(area.width);
    let header = match spaced {
        true => topology::flow_header(flows).trim_end().to_string(),
        false => (0..flows).map(|flow| (flow % 10).to_string()).collect(),
    };

    let bold = Style::default().add_modifier(Modifier::BOLD);
    let header_row = Row::new([
        right("Hop"),
        Line::from(header.clone()),
        Line::from("Responders"),
    ])
    .style(bold);
    let body = shown.iter().zip(numbers).map(|(hop, number)| {
        let mut cells = Vec::new();
        for cell in topology::row(hop, flows) {
            if spaced && !cells.is_empty() {
                cells.push(Span::raw(" "));
            }
            cells.push(flow_cell(cell, ui.colors));
        }
        Row::new([
            Cell::from(right(number)),
            Cell::from(Line::from(cells)),
            Cell::from(Line::from(legend_spans(hop, names, ui.colors))),
        ])
    });
    let widths = [
        Constraint::Length(u16::try_from(hop_width).unwrap_or(u16::MAX)),
        Constraint::Length(u16::try_from(header.width()).unwrap_or(u16::MAX)),
        Constraint::Fill(1),
    ];
    let table = Table::new(body, widths)
        .header(header_row)
        .column_spacing(1)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(table, area, &mut ui.table);
}

fn flow_cell(cell: topology::Cell, colors: ColorSupport) -> Span<'static> {
    let style = Style::default();
    let style = match cell {
        topology::Cell::One(letter) => style
            .fg(letter_color(letter_index(letter), colors))
            .add_modifier(Modifier::BOLD),
        topology::Cell::Mixed => style.add_modifier(Modifier::BOLD),
        topology::Cell::Silent => style.fg(loss_color(colors)),
        topology::Cell::Unprobed => style,
    };
    Span::styled(cell.symbol().to_string(), style)
}

fn letter_index(letter: char) -> usize {
    (letter as usize).saturating_sub('A' as usize)
}

fn letter_span(letter: char, colors: ColorSupport) -> Span<'static> {
    let style = Style::default()
        .fg(letter_color(letter_index(letter), colors))
        .add_modifier(Modifier::BOLD);
    Span::styled(letter.to_string(), style)
}

/// `A name (60%)  B name (40%)`: who the letters of a hop stand for.
fn legend_spans(hop: &HopStats, names: bool, colors: ColorSupport) -> Vec<Span<'static>> {
    let letters = topology::letters(hop);
    if letters.is_empty() {
        let dim = Style::default().add_modifier(Modifier::DIM);
        return vec![Span::styled("no reply", dim)];
    }
    let mut spans = Vec::new();
    for &(letter, addr) in &letters {
        let responder = &hop.responders[&addr];
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(letter_span(letter, colors));
        let host = host_label(responder.hostname.as_ref(), Some(addr), names);
        spans.push(Span::raw(match letters.len() {
            1 => format!(" {host}"),
            _ => format!(" {host} ({:.0}%)", hop.get_path_percentage(responder)),
        }));
    }
    spans
}

fn render_flow_legend(f: &mut Frame, area: Rect, session: &MtrSession) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines = vec![Line::styled(
        "A, B, …: who answered, most replies first · * several in one flow · · no reply",
        dim,
    )];
    if session.args.flows <= 1 {
        lines.push(Line::from(
            "One flow follows one path: start with --flows 8 to find load-balanced ones.",
        ));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Everything known about one hop, over the table.
fn render_detail(f: &mut Frame, area: Rect, session: &MtrSession, hops: &[HopStats], ui: &UiState) {
    let width = area.width.min(DETAIL_WIDTH);
    let hop = hops.iter().find(|hop| hop.hop == ui.detail_hop);
    let lines = match hop {
        Some(hop) if hop.sent > 0 => detail_lines(hop, session, ui, usize::from(width) - 4),
        _ => vec![Line::from("Nothing sent to this hop yet.")],
    };
    let title = match hop {
        Some(hop) if hop.is_target => format!(" Hop {} · target ", ui.detail_hop),
        _ => format!(" Hop {} ", ui.detail_hop),
    };
    let popup = centered(area, usize::from(width), lines.len() + 2);
    let block = Block::bordered()
        .title(title)
        .title_bottom(Line::from(" ↑↓ other hops · Esc closes ").alignment(Alignment::Right))
        .padding(Padding::horizontal(1));
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// The detail pane's text, `width` cells wide: a summary, every responder, the RTT
/// spread with a histogram, and MPLS labels.
fn detail_lines(
    hop: &HopStats,
    session: &MtrSession,
    ui: &UiState,
    width: usize,
) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let names = shows_names(session, ui);
    let flows = session.args.flows > 1;

    let mut facts = vec![
        format!("{} sent", hop.sent),
        format!("{:.1}% loss", hop.loss_percent),
    ];
    if hop.in_flight() > 0 {
        facts.push(format!("{} in flight", hop.in_flight()));
    }
    if flows {
        facts.push(format!("{} flows", hop.flow_count()));
    }
    let mut lines = vec![Line::from(facts.join(" · "))];

    let letters = topology::letters(hop);
    if !letters.is_empty() {
        lines.push(Line::from(""));
        lines.extend(responder_lines(
            hop, &letters, names, flows, width, ui.colors,
        ));
    }
    lines.extend(location_lines(hop, &letters, session, ui.colors, width));

    lines.push(Line::from(""));
    match hop.rtt_spread() {
        None => lines.push(Line::from("No replies yet.")),
        Some(spread) => {
            let ms = |rtt| format_ms(Some(rtt));
            lines.push(Line::styled(
                format!("RTT of the last {} replies", spread.samples),
                bold,
            ));
            lines.push(Line::from(format!(
                "min {}  p50 {}  p90 {}  p99 {}  max {} ms",
                ms(spread.min),
                ms(spread.p50),
                ms(spread.p90),
                ms(spread.p99),
                ms(spread.max)
            )));
            let rtts: Vec<_> = hop.recent_rtts().collect();
            let buckets = width.min(HISTOGRAM_WIDTH);
            lines.push(Line::from(histogram_spans(
                &histogram(&rtts, buckets),
                ui.colors,
            )));
            let lo = format_rtt(spread.min.as_secs_f64() * 1000.0);
            let hi = format_rtt(spread.max.as_secs_f64() * 1000.0);
            let pad = buckets.saturating_sub(lo.width());
            lines.push(Line::styled(format!("{lo}{hi:>pad$}"), dim));
        }
    }

    let labelled: Vec<_> = letters
        .iter()
        .map(|&(letter, addr)| (letter, &hop.responders[&addr]))
        .filter(|(_, r)| !r.mpls.is_empty())
        .collect();
    if !labelled.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::styled("MPLS labels", bold));
        for (letter, responder) in labelled {
            for label in &responder.mpls {
                lines.push(Line::from(vec![
                    letter_span(letter, ui.colors),
                    Span::raw(format!("  {label}")),
                ]));
            }
        }
    }

    if letters
        .iter()
        .any(|(_, addr)| hop.responders[addr].reply_ttl.is_some())
    {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!(
                "Back: routers the reply crossed, judged by its TTL. The probe crossed {}.",
                hop.hop.saturating_sub(1)
            ),
            dim,
        ));
        lines.push(Line::styled(
            "+N or -N: that many more or fewer, so the reply came back another way.",
            dim,
        ));
    }
    lines
}

/// Where each responder is, how far from the origin, and whether its best RTT
/// allows that, then the database's credit. Nothing without a location.
fn location_lines(
    hop: &HopStats,
    letters: &[(char, IpAddr)],
    session: &MtrSession,
    colors: ColorSupport,
    width: usize,
) -> Vec<Line<'static>> {
    let placed: Vec<_> = letters
        .iter()
        .filter_map(|&(letter, addr)| {
            let responder = &hop.responders[&addr];
            Some((letter, responder, responder.geo.as_ref()?))
        })
        .collect();
    if placed.is_empty() {
        return Vec::new();
    }
    let origin = session.origin();
    let from = match origin.and_then(|o| o.hop) {
        Some(n) => format!("hop {n}"),
        None => "--origin".to_string(),
    };
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::from(""), Line::styled("Location", bold)];
    for &(letter, responder, fix) in &placed {
        let mut place = fix.place();
        if let Some(km) = fix.accuracy_km {
            place.push_str(&format!(" ±{km} km"));
        }
        if let Some(code) = &fix.from_name {
            place.push_str(&format!(" ({code} in its name)"));
        }
        let mut facts = vec![place];
        let assessment = origin.and_then(|o| geo::assess(fix, responder.best_rtt, &o));
        if let Some(a) = assessment {
            facts.push(format!("{:.0} km from {from}", a.distance_km));
        }
        if let Some(rtt) = responder.best_rtt {
            let reach = geo::reach_km(rtt);
            facts.push(format!("{} ms reaches {reach:.0} km", format_ms(Some(rtt))));
        }
        let style = match assessment.is_some_and(|a| a.impossible) {
            true => {
                if let Some(last) = facts.last_mut() {
                    last.push_str(": too far, the location is wrong");
                }
                Style::default().fg(loss_color(colors))
            }
            false => Style::default(),
        };
        // Continuation lines line up under the first, past the letter
        for (i, text) in pack(&facts, " · ", width.saturating_sub(3))
            .into_iter()
            .enumerate()
        {
            let lead = match i {
                0 => letter_span(letter, colors),
                _ => Span::raw(" "),
            };
            lines.push(Line::from(vec![
                lead,
                Span::styled(format!("  {text}"), style),
            ]));
        }
    }
    // Credit the database where it placed something; names need none
    let from_database = placed.iter().any(|(_, _, fix)| fix.from_name.is_none());
    if let Some(credit) = session.geo_attribution().filter(|_| from_database) {
        let dim = Style::default().add_modifier(Modifier::DIM);
        lines.push(Line::styled(credit.to_string(), dim));
    }
    lines
}

type ResponderValue<'a> = Box<dyn Fn(&ResponderStats) -> String + 'a>;

/// A header and a row per responder: share of replies, RTTs, routers on the way
/// back, AS and flows. Best and Last go first when the pane is narrow.
fn responder_lines(
    hop: &HopStats,
    letters: &[(char, IpAddr)],
    names: bool,
    flows: bool,
    width: usize,
    colors: ColorSupport,
) -> Vec<Line<'static>> {
    let responders: Vec<(char, &ResponderStats)> = letters
        .iter()
        .map(|&(letter, addr)| (letter, &hop.responders[&addr]))
        .collect();
    let host = |r: &ResponderStats| match &r.hostname {
        Some(name) if names => format!("{name} ({})", r.addr),
        _ => r.addr.to_string(),
    };
    let asn = |r: &ResponderStats| r.asn.map(|n| format!("AS{n}"));
    let flows_of = |r: &ResponderStats| {
        let met: BTreeSet<u16> = hop
            .flows
            .iter()
            .filter(|(_, stats)| stats.responders.contains_key(&r.addr))
            .map(|(&flow, _)| flow)
            .collect();
        topology::ranges(&met)
    };

    let mut numbers: Vec<(&str, ResponderValue)> = vec![
        (
            "Share",
            Box::new(|r| format!("{:.0}%", hop.get_path_percentage(r))),
        ),
        ("Avg", Box::new(|r| format_ms(r.avg_rtt()))),
        ("Best", Box::new(|r| format_ms(r.best_rtt))),
        ("Last", Box::new(|r| format_ms(r.last_rtt))),
        (
            "Back",
            Box::new(|r| columns::routers_back(r.routers_back(), hop.hop)),
        ),
    ];
    if width < 72 {
        numbers.retain(|(name, _)| !matches!(*name, "Best" | "Last"));
    }
    let as_width = responders
        .iter()
        .filter_map(|(_, r)| asn(r))
        .map(|text| text.width() + 1)
        .max()
        .unwrap_or(0);
    let fixed = 3 + numbers.len() * 7 + as_width + if flows { 7 } else { 0 };
    let longest = responders
        .iter()
        .map(|(_, r)| host(r).width())
        .max()
        .unwrap_or(0);
    let host_width = longest.min(width.saturating_sub(fixed)).max(8);

    let row = |host: String, cells: Vec<String>, asn: String, flows_text: String| {
        let mut text = format!("{:<host_width$}", truncate_hostname(&host, host_width));
        for cell in cells {
            text.push_str(&format!(" {cell:>6}"));
        }
        if as_width > 0 {
            text.push_str(&format!(" {asn:<w$}", w = as_width - 1));
        }
        if flows {
            // Two spaces, so Back's "3 +2" doesn't run into the flow numbers
            text.push_str(&format!("  {flows_text}"));
        }
        text
    };
    let header = row(
        "Address".into(),
        numbers.iter().map(|(name, _)| name.to_string()).collect(),
        "AS".into(),
        "Flows".into(),
    );
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::styled(format!("   {header}"), bold)];
    for (letter, r) in responders {
        let text = row(
            host(r),
            numbers.iter().map(|(_, value)| value(r)).collect(),
            asn(r).unwrap_or_default(),
            flows_of(r),
        );
        lines.push(Line::from(vec![
            letter_span(letter, colors),
            Span::raw(format!("  {text}")),
        ]));
    }
    lines
}

fn centered(area: Rect, width: usize, height: usize) -> Rect {
    let width = u16::try_from(width).unwrap_or(u16::MAX).min(area.width);
    let height = u16::try_from(height).unwrap_or(u16::MAX).min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Widest key label among `bindings`.
fn label_width(bindings: &[Binding]) -> usize {
    bindings.iter().map(|b| b.label.width()).max().unwrap_or(0)
}

/// A key padded to `label_width` and what it does, padded to `help_width`.
fn binding_spans(b: &Binding, label_width: usize, help_width: usize) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!("{:<label_width$} ", b.label),
            Style::default().fg(Color::Green),
        ),
        Span::raw(format!("{:<help_width$}", b.help)),
    ]
}

fn binding_lines(
    bindings: &[Binding],
    label_width: usize,
) -> impl Iterator<Item = Line<'static>> + '_ {
    bindings
        .iter()
        .map(move |b| Line::from(binding_spans(b, label_width, 0)))
}

/// The table's keys side by side, for terminals too short to list them.
fn two_columns(bindings: &[Binding]) -> Vec<Line<'static>> {
    let (left, right) = bindings.split_at(bindings.len().div_ceil(2));
    let pad = left.iter().map(|b| b.help.width()).max().unwrap_or(0) + 3;
    let label = label_width(bindings);
    let right = right.iter().map(Some).chain([None]);
    left.iter()
        .zip(right)
        .map(|(l, r)| {
            let mut spans = binding_spans(l, label, pad);
            spans.extend(r.map(|r| binding_spans(r, label, 0)).into_iter().flatten());
            Line::from(spans)
        })
        .collect()
}

/// Generated from the key tables. The column chooser's and the detail pane's keys
/// are left out when the terminal is too short; both overlays name their keys.
fn render_help(f: &mut Frame, area: Rect) {
    let heading =
        |text: &'static str| Line::from(text).style(Style::default().add_modifier(Modifier::BOLD));
    // One label width for all sections, or the paired table's own when it pairs
    let mut label = [TABLE_KEYS, COLUMN_KEYS, DETAIL_KEYS]
        .map(label_width)
        .into_iter()
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line> = binding_lines(TABLE_KEYS, label).collect();
    if lines.len() + 2 > usize::from(area.height) {
        let paired = two_columns(TABLE_KEYS);
        if paired.iter().map(Line::width).max().unwrap_or(0) + 4 <= usize::from(area.width) {
            lines = paired;
            label = label_width(TABLE_KEYS);
        }
    }
    for (title, keys) in [
        ("In the column chooser", COLUMN_KEYS),
        ("In the hop details", DETAIL_KEYS),
    ] {
        if usize::from(area.height) >= lines.len() + keys.len() + 4 {
            lines.push(Line::from(""));
            lines.push(heading(title));
            lines.extend(binding_lines(keys, label.max(label_width(keys))));
        }
    }
    let width = lines.iter().map(Line::width).max().unwrap_or(0) + 4;
    let popup = centered(area, width, lines.len() + 2);
    let block = Block::bordered()
        .title(" Keys ")
        .title_bottom(Line::from(" Esc closes ").alignment(Alignment::Right));
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_chooser(f: &mut Frame, area: Rect, ui: &UiState) {
    let items: Vec<ListItem> = ui
        .chooser
        .entries
        .iter()
        .map(|&(column, shown)| {
            let mark = if shown { "[x]" } else { "[ ]" };
            ListItem::new(format!("{mark} {}", column.label()))
        })
        .collect();
    let width = items.iter().map(ListItem::width).max().unwrap_or(0) + 4;
    let popup = centered(area, width.max(48), items.len() + 2);
    let block = Block::bordered().title(" Columns ").title_bottom(
        Line::from(" Space shows/hides · ←→ moves · Esc closes ").alignment(Alignment::Center),
    );
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(ui.chooser.cursor));
    f.render_widget(Clear, popup);
    f.render_stateful_widget(list, popup, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::tests::{city, FakeGeo};
    use crate::Args;
    use clap::Parser;
    use ratatui::{backend::TestBackend, Terminal};
    use std::time::Duration;
    use std::{collections::HashMap, sync::Arc};

    const PATH: [&str; 3] = ["192.168.1.1", "203.0.113.254", "198.51.100.7"];

    async fn session(args: &[&str]) -> MtrSession {
        let args: Vec<&str> = ["mtr-ng"]
            .iter()
            .chain(args)
            .chain(&["198.51.100.7"])
            .copied()
            .collect();
        let mut s = MtrSession::new(Args::parse_from(args)).await.unwrap();
        for (i, addr) in PATH.iter().enumerate() {
            let hop = &mut s.hops[i];
            for ms in [1.0, 2.0, 3.0] {
                hop.increment_sent(0);
                let rtt = Duration::from_secs_f64((i + 1) as f64 * ms / 1000.0);
                hop.add_rtt_from_addr(addr.parse().unwrap(), rtt);
            }
        }
        s.hops[2].mark_as_target();
        s
    }

    /// `session` with two flows: at hop 2, flow 0 meets the usual router and flow 1
    /// another one inside an MPLS tunnel, three hops back.
    async fn diamond() -> MtrSession {
        let mut s = session(&["--flows", "2", "-n"]).await;
        let other: IpAddr = "203.0.113.253".parse().unwrap();
        let hop = &mut s.hops[1];
        for _ in 0..2 {
            let ordinal = hop.increment_sent(1);
            hop.record_reply(Some(ordinal), Some(other), Duration::from_millis(5));
        }
        let label = MplsLabel {
            label: 24005,
            tc: 0,
            bottom: true,
            ttl: 1,
        };
        hop.record_reply_details(other, Some(252), &[label]);
        s
    }

    fn screen(session: &MtrSession, ui: &mut UiState, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, session, ui)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn host_column_fits_the_longest_name_and_the_graph_takes_the_rest() {
        let columns = Column::default_columns();
        let natural: Vec<usize> = columns
            .iter()
            .map(|&c| match c {
                Column::Host => 40,
                _ => c.min_width(),
            })
            .collect();
        let host = columns.iter().position(|&c| c == Column::Host).unwrap();
        let graph = columns.iter().position(|&c| c == Column::Graph).unwrap();

        let wide = layout(&columns, &natural, 160);
        assert_eq!(wide[host], 40);
        let used: u16 = wide.iter().sum::<u16>() + columns.len() as u16 - 1;
        assert_eq!(used, 160, "the graph fills the row");

        let narrow = layout(&columns, &natural, 80);
        assert_eq!(
            narrow[graph],
            Column::Graph.header().len() as u16,
            "the host gives way until the graph's header just fits"
        );

        let cramped = layout(&columns, &natural, 70);
        assert_eq!(cramped.len(), columns.len());
        assert_eq!(
            usize::from(cramped[host]),
            HOST_MIN,
            "a full IPv4 address still fits"
        );
    }

    #[test]
    fn columns_that_do_not_fit_are_dropped_whole_from_the_right() {
        let columns = Column::default_columns();
        let natural: Vec<usize> = columns
            .iter()
            .map(|&c| match c {
                Column::Host => HOST_MIN,
                _ => c.min_width().max(2),
            })
            .collect();
        let everything = natural.iter().sum::<usize>() + columns.len() - 1;
        for total in 40..everything as u16 {
            let widths = layout(&columns, &natural, total);
            assert!(widths.len() >= 3, "{total}: hop, host and loss always fit");
            assert!(widths.len() < columns.len(), "{total}: something had to go");
            for (i, &w) in widths.iter().enumerate() {
                assert!(usize::from(w) >= natural[i], "{total}: column {i} squeezed");
            }
            let used = widths.iter().sum::<u16>() + widths.len() as u16 - 1;
            assert!(used <= total, "{total}: {used} cells used");
        }
    }

    #[tokio::test]
    async fn numbers_stay_whole_at_the_minimum_width() {
        let s = session(&["--simulate"]).await;
        let mut ui = UiState::new(&s.args);
        let lines = screen(&s, &mut ui, MIN_WIDTH, 10);
        assert!(lines[1].contains("Loss%"), "{}", lines[1]);
        for row in &lines[2..5] {
            assert!(row.starts_with(['1', '2', '3']), "hop number kept: {row}");
            assert!(row.contains('%'), "loss kept: {row}");
        }
    }

    #[tokio::test]
    async fn help_fits_a_short_terminal_in_two_columns() {
        let s = session(&[]).await;
        let mut ui = UiState::new(&s.args);
        ui.mode = Mode::Help;
        let text = screen(&s, &mut ui, 80, 16).join("\n");
        for binding in TABLE_KEYS {
            assert!(text.contains(binding.help), "missing {}", binding.help);
        }
    }

    #[tokio::test]
    async fn addresses_and_headers_are_whole_at_80_columns() {
        let s = session(&["-n"]).await;
        let mut ui = UiState::new(&s.args);
        let lines = screen(&s, &mut ui, 80, 24);
        assert!(
            lines[1].contains("Loss%") && lines[1].contains("Wrst"),
            "{}",
            lines[1]
        );
        assert!(lines.iter().any(|l| l.contains("203.0.113.254")));
        assert!(lines[0].contains("0.0% loss at target"), "{}", lines[0]);
        assert!(lines[0].contains("IPs"), "-n shows IPs: {}", lines[0]);
    }

    #[tokio::test]
    async fn long_names_shrink_only_when_space_runs_out() {
        let mut s = session(&[]).await;
        let name = "ae-12.r21.amstnl07.nl.bb.gin.ntt.net";
        s.hops[1].set_hostname_for_addr(PATH[1].parse().unwrap(), name.to_string());
        let mut ui = UiState::new(&s.args);
        assert!(screen(&s, &mut ui, 160, 24)
            .iter()
            .any(|l| l.contains(name)));
        let narrow = screen(&s, &mut ui, 80, 24);
        assert!(!narrow.iter().any(|l| l.contains(name)));
        assert!(narrow
            .iter()
            .any(|l| l.contains("ae-12.r21") && l.contains("...")));
    }

    #[tokio::test]
    async fn every_mode_survives_every_size() {
        let mut s = diamond().await;
        s.args.simulate = true;
        s.set_geo_source(Arc::new(FakeGeo(HashMap::new())));
        s.hops[1].set_geo_for_addr(PATH[1].parse().unwrap(), city("London", "GB", 51.5, -0.1));
        let modes = [Mode::Table, Mode::Help, Mode::Columns, Mode::Detail];
        let views = [View::Hops, View::Flows, View::Map];
        for (mode, view) in modes.into_iter().flat_map(|m| views.map(|v| (m, v))) {
            for (w, h) in [(1, 1), (39, 5), (40, 6), (50, 10), (80, 24), (200, 60)] {
                let mut ui = UiState::new(&s.args);
                (ui.mode, ui.view, ui.detail_hop, ui.show_mpls) = (mode, view, 2, true);
                ui.table.select(Some(99));
                let lines = screen(&s, &mut ui, w, h);
                if w >= MIN_WIDTH && h >= MIN_HEIGHT && mode == Mode::Table {
                    assert!(lines[0].contains("SIMULATED"), "{mode:?} {w}x{h}");
                }
            }
        }
    }

    #[tokio::test]
    async fn the_detail_pane_says_where_a_location_comes_from() {
        let mut s = session(&["-n"]).await;
        s.set_geo_source(Arc::new(FakeGeo(HashMap::new())));
        let addr = PATH[1].parse().unwrap();
        s.hops[1].set_geo_for_addr(addr, city("London", "GB", 51.5, -0.1));
        let mut ui = UiState::new(&s.args);
        (ui.mode, ui.detail_hop) = (Mode::Detail, 2);
        let text = screen(&s, &mut ui, 100, 30).join("\n");
        assert!(text.contains("London, GB ±20 km ·"), "{text}");
        assert!(
            text.contains("test locations"),
            "the database's credit: {text}"
        );

        let named = crate::geo::names::locate("core1.ams1.he.net").unwrap();
        s.hops[1].set_geo_for_addr(addr, named);
        let text = screen(&s, &mut ui, 100, 30).join("\n");
        assert!(
            text.contains("Amsterdam, NL ±50 km (ams in its name)"),
            "{text}"
        );
        assert!(
            !text.contains("test locations"),
            "nothing to credit: {text}"
        );
    }

    #[tokio::test]
    async fn the_map_numbers_located_hops_and_leaves_out_ruled_out_ones() {
        let mut s = session(&["-n"]).await;
        let mut ui = UiState::new(&s.args);
        ui.view = View::Map;
        let text = screen(&s, &mut ui, 100, 30).join("\n");
        assert!(text.contains("No geolocation database"), "{text}");
        assert!(text.contains("download.db-ip.com"), "{text}");

        s.set_geo_source(Arc::new(FakeGeo(HashMap::new())));
        let at = |s: &mut MtrSession, hop: usize, fix| {
            s.hops[hop].set_geo_for_addr(PATH[hop].parse().unwrap(), fix)
        };
        at(&mut s, 1, city("Amsterdam", "NL", 52.37, 4.89));
        at(&mut s, 2, city("Sydney", "AU", -33.87, 151.21));
        let text = screen(&s, &mut ui, 100, 30).join("\n");
        assert!(text.contains("●2"), "{text}");
        assert!(
            !text.contains("●3"),
            "6 ms from Amsterdam to Sydney: {text}"
        );
        assert!(
            text.contains("not placed: 1 · ruled out by RTT: 3"),
            "{text}"
        );
        assert!(text.contains("test locations"), "the credit: {text}");
        assert_eq!(ui.row_hops, [1, 2, 3], "Enter and the arrows work here too");
    }

    #[tokio::test]
    async fn the_flow_matrix_letters_what_each_flow_met() {
        let s = diamond().await;
        let mut ui = UiState::new(&s.args);
        ui.view = View::Flows;
        let lines = screen(&s, &mut ui, 80, 12);
        assert!(lines[0].contains("2 flows"), "{}", lines[0]);
        assert!(lines[1].contains("Hop 0 1 Responders"), "{}", lines[1]);
        let hop2 = lines
            .iter()
            .find(|l| l.trim_start().starts_with("2."))
            .unwrap();
        assert!(
            hop2.contains("2. A B A 203.0.113.254 (60%)  B 203.0.113.253 (40%)"),
            "{hop2}"
        );
        let hop1 = lines
            .iter()
            .find(|l| l.trim_start().starts_with("1."))
            .unwrap();
        assert!(
            hop1.contains("1. A   A 192.168.1.1"),
            "flow 1 never went there: {hop1}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("--flows 8")),
            "no hint with 2 flows"
        );
    }

    #[tokio::test]
    async fn mpls_labels_get_rows_of_their_own_with_e() {
        let s = diamond().await;
        let mut ui = UiState::new(&s.args);
        let label = "[MPLS: Lbl 24005 TC 0 S 1 TTL 1]";
        assert!(!screen(&s, &mut ui, 100, 12)
            .iter()
            .any(|l| l.contains(label)));

        ui.show_mpls = true;
        let lines = screen(&s, &mut ui, 100, 12);
        let alt = lines
            .iter()
            .position(|l| l.contains("↳ 203.0.113.253"))
            .unwrap();
        assert!(lines[alt + 1].contains(label), "{}", lines[alt + 1]);
        assert_eq!(
            ui.row_hops,
            [1, 2, 2, 2, 3],
            "the label row belongs to hop 2"
        );
    }

    #[tokio::test]
    async fn the_detail_pane_follows_the_selected_hop() {
        let s = diamond().await;
        let mut ui = UiState::new(&s.args);
        screen(&s, &mut ui, 100, 30);
        ui.table.select(Some(2)); // the alternate row of hop 2
        ui.open_detail();
        let text = screen(&s, &mut ui, 100, 30).join("\n");
        assert!(text.contains(" Hop 2 "), "{text}");
        assert!(text.contains("5 sent · 0.0% loss · 2 flows"), "{text}");
        for wanted in [
            "A  203.0.113.254",
            "B  203.0.113.253",
            "Back",
            "p50",
            "MPLS labels",
        ] {
            assert!(text.contains(wanted), "missing {wanted}: {text}");
        }
        let row_b = text
            .lines()
            .find(|l| l.contains("B  203.0.113.253"))
            .unwrap();
        assert!(
            row_b.trim_end_matches([' ', '│']).ends_with("3 +2  1"),
            "3 back, 2 more than there, flow 1: {row_b}"
        );

        ui.step_detail(1);
        let text = screen(&s, &mut ui, 100, 30).join("\n");
        assert!(text.contains(" Hop 3 · target "), "{text}");
        assert_eq!(ui.table.selected(), Some(3), "the table follows");
    }

    #[tokio::test]
    async fn help_lists_every_key() {
        let s = session(&[]).await;
        let mut ui = UiState::new(&s.args);
        ui.mode = Mode::Help;
        let text = screen(&s, &mut ui, 100, 40).join("\n");
        for binding in TABLE_KEYS.iter().chain(COLUMN_KEYS) {
            assert!(text.contains(binding.help), "missing {}", binding.help);
        }
    }

    #[test]
    fn scale_labels_cover_sub_millisecond_to_seconds() {
        assert_eq!(format_rtt(0.345), "0.34ms");
        assert_eq!(format_rtt(4.56), "4.6ms");
        assert_eq!(format_rtt(245.4), "245ms");
        assert_eq!(format_rtt(1530.0), "1.5s");
    }
}
