//! Draws one frame: status line, hop table, RTT scale, and the help or column overlay.

use super::events::{Binding, COLUMN_KEYS, TABLE_KEYS};
use super::state::{Mode, UiState};
use super::visualization::{gradient, history_spans, symbol, RttScale};
use crate::args::Column;
use crate::columns::format_ms;
use crate::hop_stats::ResponderStats;
use crate::utils::network::truncate_hostname;
use crate::{HopStats, MtrSession, SparklineScale};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap},
    Frame,
};
use std::net::IpAddr;
use unicode_width::UnicodeWidthStr;

/// Smallest terminal the table makes sense in.
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 6;

/// While a graph shares the row, the host column gets at least this much (a full
/// IPv4 address) and the graph at least its header's width.
const HOST_MIN: usize = 15;

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
    let scale = RttScale::from_hops(hops, ui.scale);
    f.render_widget(Paragraph::new(status_line(session, hops, ui)), status);
    render_table(f, table, session, hops, scale.as_ref(), ui);
    render_scale(f, scale_area, scale.as_ref(), ui);
    match ui.mode {
        Mode::Table => {}
        Mode::Help => render_help(f, area),
        Mode::Columns => render_chooser(f, area, ui),
    }
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
    if let Some(end) = hops.last().filter(|hop| hop.is_target) {
        facts.push(format!("{:.1}% loss at target", end.loss_percent));
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

/// One table row: a hop, or one more address that answered for it.
struct HostRow<'a> {
    hop: &'a HopStats,
    host: String,
    alternate: Option<&'a ResponderStats>,
}

fn host_rows<'a>(session: &MtrSession, hops: &'a [HopStats], ui: &UiState) -> Vec<HostRow<'a>> {
    let names = shows_names(session, ui);
    let label = |name: Option<&String>, addr: Option<IpAddr>| match (name, addr) {
        (Some(name), _) if names => name.clone(),
        (_, Some(addr)) => addr.to_string(),
        _ => "???".to_string(),
    };
    let mut rows = Vec::new();
    for hop in hops.iter().filter(|hop| hop.sent > 0) {
        let mut host = label(hop.hostname.as_ref(), hop.addr);
        if hop.has_multiple_paths() && hop.addr.is_some() {
            host = format!("{host} ({:.0}%)", hop.get_primary_path_percentage());
        }
        rows.push(HostRow {
            hop,
            host,
            alternate: None,
        });
        for alt in hop.get_alternate_paths() {
            let host = format!(
                "↳ {} ({:.0}%)",
                label(alt.hostname.as_ref(), Some(alt.addr)),
                hop.get_path_percentage(alt)
            );
            rows.push(HostRow {
                hop,
                host,
                alternate: Some(alt),
            });
        }
    }
    rows
}

fn cell_text(column: Column, row: &HostRow) -> String {
    match (column, row.alternate) {
        (Column::Hop, None) => format!("{}.", row.hop.hop),
        (Column::Host, _) => row.host.clone(),
        (Column::Hop | Column::Graph, _) => String::new(),
        (_, None) => column.value(row.hop),
        (Column::Last, Some(alt)) => format_ms(alt.last_rtt),
        (Column::Avg, Some(alt)) => format_ms(alt.avg_rtt()),
        (Column::Best, Some(alt)) => format_ms(alt.best_rtt),
        (Column::Asn, Some(alt)) => alt.asn.map_or("AS???".to_string(), |n| format!("AS{n}")),
        (_, Some(_)) => String::new(),
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
    let columns = &ui.columns;
    let rows = host_rows(session, hops, ui);
    let texts: Vec<Vec<String>> = rows
        .iter()
        .map(|row| columns.iter().map(|&c| cell_text(c, row)).collect())
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
        Column::Host | Column::Asn | Column::Graph => Line::from(c.header()),
        _ => right(c.header()),
    }))
    .style(Style::default().add_modifier(Modifier::BOLD));

    let body = rows.iter().zip(texts).map(|(row, cells)| {
        let cells = columns
            .iter()
            .zip(cells)
            .zip(&widths)
            .map(|((&c, text), &width)| match c {
                Column::Graph if row.alternate.is_none() => Cell::from(Line::from(history_spans(
                    row.hop,
                    scale,
                    ui.visualization,
                    ui.colors,
                    usize::from(width),
                ))),
                Column::Host => Cell::from(truncate_hostname(&text, usize::from(width))),
                Column::Asn => Cell::from(text),
                _ => Cell::from(right(text)),
            });
        let style = match row.alternate {
            Some(_) => Style::default().add_modifier(Modifier::DIM),
            None => Style::default(),
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

/// A key and what it does, the description padded to `help_width`.
fn binding_spans(b: &Binding, help_width: usize) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!("{:<10} ", b.label),
            Style::default().fg(Color::Green),
        ),
        Span::raw(format!("{:<help_width$}", b.help)),
    ]
}

fn binding_lines(bindings: &[Binding]) -> impl Iterator<Item = Line<'static>> + '_ {
    bindings.iter().map(|b| Line::from(binding_spans(b, 0)))
}

/// The table's keys side by side, for terminals too short to list them.
fn two_columns(bindings: &[Binding]) -> Vec<Line<'static>> {
    let (left, right) = bindings.split_at(bindings.len().div_ceil(2));
    let pad = left.iter().map(|b| b.help.width()).max().unwrap_or(0) + 3;
    let right = right.iter().map(Some).chain([None]);
    left.iter()
        .zip(right)
        .map(|(l, r)| {
            let mut spans = binding_spans(l, pad);
            spans.extend(r.map(|r| binding_spans(r, 0)).into_iter().flatten());
            Line::from(spans)
        })
        .collect()
}

/// Generated from the key tables. The column chooser's keys are left out when the
/// terminal is too short for both; the chooser lists its own.
fn render_help(f: &mut Frame, area: Rect) {
    let heading =
        |text: &'static str| Line::from(text).style(Style::default().add_modifier(Modifier::BOLD));
    let mut lines: Vec<Line> = binding_lines(TABLE_KEYS).collect();
    if lines.len() + 2 > usize::from(area.height) {
        let paired = two_columns(TABLE_KEYS);
        if paired.iter().map(Line::width).max().unwrap_or(0) + 4 <= usize::from(area.width) {
            lines = paired;
        }
    }
    if usize::from(area.height) >= lines.len() + COLUMN_KEYS.len() + 4 {
        lines.push(Line::from(""));
        lines.push(heading("In the column chooser"));
        lines.extend(binding_lines(COLUMN_KEYS));
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
    use crate::Args;
    use clap::Parser;
    use ratatui::{backend::TestBackend, Terminal};
    use std::time::Duration;

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
                hop.increment_sent();
                let rtt = Duration::from_secs_f64((i + 1) as f64 * ms / 1000.0);
                hop.add_rtt_from_addr(addr.parse().unwrap(), rtt);
            }
        }
        s.hops[2].mark_as_target();
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
        let s = session(&["--simulate"]).await;
        for mode in [Mode::Table, Mode::Help, Mode::Columns] {
            for (w, h) in [(1, 1), (39, 5), (40, 6), (50, 10), (80, 24), (200, 60)] {
                let mut ui = UiState::new(&s.args);
                ui.mode = mode;
                ui.table.select(Some(99));
                let lines = screen(&s, &mut ui, w, h);
                if w >= MIN_WIDTH && h >= MIN_HEIGHT && mode == Mode::Table {
                    assert!(lines[0].contains("SIMULATED"), "{mode:?} {w}x{h}");
                }
            }
        }
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
