use crate::args::Column;
use crate::utils::{network, time::duration_to_ms_f64};
use crate::{tracer, HopStats, MtrSession, Result};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

/// The host column is at least this wide, and as wide as the longest name.
const HOST_MIN_WIDTH: usize = 20;

/// Numeric columns are at least this wide, so short and long runs line up alike.
const VALUE_MIN_WIDTH: usize = 5;

fn header(column: Column) -> &'static str {
    match column {
        Column::Hop | Column::Host => "",
        Column::Loss => "Loss%",
        Column::Sent => "Snt",
        Column::Last => "Last",
        Column::Avg => "Avg",
        Column::Ema => "EMA",
        Column::Jitter => "Jttr",
        Column::JitterAvg => "JAvg",
        Column::Best => "Best",
        Column::Worst => "Wrst",
        // The report shows the spread where the TUI draws a graph
        Column::Graph => "StDev",
    }
}

fn cell(hop: &HopStats, host: &str, column: Column) -> String {
    let ms = |rtt: Option<Duration>| {
        rtt.map_or_else(
            || "???".to_string(),
            |rtt| format!("{:.1}", duration_to_ms_f64(rtt)),
        )
    };
    match column {
        Column::Hop => format!("{:2}.|--", hop.hop),
        Column::Host => host.to_string(),
        Column::Loss => format!("{:.1}%", hop.loss_percent),
        Column::Sent => hop.sent.to_string(),
        Column::Last => ms(hop.last_rtt),
        Column::Avg => ms(hop.avg_rtt),
        Column::Ema => ms(hop.ema_rtt),
        Column::Jitter => ms(hop.last_jitter),
        Column::JitterAvg => ms(hop.jitter_avg),
        Column::Best => ms(hop.best_rtt),
        Column::Worst => ms(hop.worst_rtt),
        Column::Graph if hop.received == 0 => "???".to_string(),
        Column::Graph => format!("{:.1}", hop.stddev_ms()),
    }
}

/// The header line and one line per probed hop, aligned however long the names are.
fn format_table(hops: &[HopStats], columns: &[Column], numeric: bool) -> Vec<String> {
    let rows: Vec<Vec<String>> = hops
        .iter()
        .filter(|hop| hop.sent > 0)
        .map(|hop| {
            let host = if numeric {
                network::format_optional_ip(hop.addr)
            } else {
                network::format_hostname_with_fallback(hop.hostname.clone(), hop.addr)
            };
            columns.iter().map(|&c| cell(hop, &host, c)).collect()
        })
        .collect();

    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, &column)| {
            let min = match column {
                Column::Hop => 0,
                Column::Host => HOST_MIN_WIDTH,
                _ => VALUE_MIN_WIDTH,
            };
            rows.iter()
                .map(|row| row[i].width())
                .chain([header(column).width(), min])
                .max()
                .unwrap_or(0)
        })
        .collect();

    let headers = columns.iter().map(|&c| header(c).to_string()).collect();
    std::iter::once(headers)
        .chain(rows)
        .map(|cells| align(&cells, columns, &widths))
        .collect()
}

/// Host left-aligned, everything else right-aligned, one space between columns.
fn align(cells: &[String], columns: &[Column], widths: &[usize]) -> String {
    let mut line = String::new();
    for (i, ((cell, &column), &width)) in cells.iter().zip(columns).zip(widths).enumerate() {
        if i > 0 {
            line.push(' ');
        }
        let pad = " ".repeat(width.saturating_sub(cell.width()));
        if column == Column::Host {
            line.push_str(cell);
            line.push_str(&pad);
        } else {
            line.push_str(&pad);
            line.push_str(cell);
        }
    }
    line.trim_end().to_string()
}

pub async fn run_report(mut session: MtrSession) -> Result<()> {
    match session.open_probe_engine()? {
        None => session.run_simulated_trace().await?,
        Some(prober) => {
            let shared = Arc::new(Mutex::new(session));
            tracer::run(Arc::clone(&shared), prober).await?;
            session = shared.lock().unwrap().clone();
        }
    }

    println!(
        "Start: {}",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );
    println!(
        "HOST: localhost → {} ({})",
        session.target, session.target_addr
    );
    if session.is_simulated() {
        println!("SIMULATED: fake data from --simulate, not a real trace");
    }
    println!();
    let hops = &session.hops[..session.display_hop_count().min(session.hops.len())];
    for line in format_table(hops, &session.args.get_columns(), session.args.numeric) {
        println!("{line}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn ms(n: f64) -> Duration {
        Duration::from_secs_f64(n / 1000.0)
    }

    /// A hop that answered from `addr` with these RTTs; `None` is a lost probe.
    fn hop(n: u8, addr: [u8; 4], name: Option<&str>, rtts: &[Option<f64>]) -> HopStats {
        let addr = IpAddr::from(addr);
        let mut hop = HopStats::new(n);
        for rtt in rtts {
            hop.increment_sent();
            match rtt {
                Some(rtt) => hop.add_rtt_from_addr(addr, ms(*rtt)),
                None => hop.add_timeout(),
            }
        }
        if let Some(name) = name {
            hop.set_hostname_for_addr(addr, name.to_string());
        }
        hop
    }

    fn path() -> Vec<HopStats> {
        let mut silent = HopStats::new(2);
        for _ in 0..3 {
            silent.increment_sent();
            silent.add_timeout();
        }
        vec![
            hop(
                1,
                [192, 168, 1, 1],
                Some("gateway.lan"),
                &[Some(1.0), Some(2.0), Some(3.0)],
            ),
            silent,
            hop(
                3,
                [195, 190, 228, 107],
                Some("195-190-228-107.fixed.kpn.net"),
                &[Some(10.0), None, Some(12.5)],
            ),
            hop(
                4,
                [150, 65, 7, 130],
                None,
                &[Some(1005.2), Some(998.0), Some(1001.0)],
            ),
            HopStats::new(5), // never probed: not shown
        ]
    }

    #[test]
    fn long_names_widen_the_host_column_and_keep_everything_aligned() {
        let lines = format_table(&path(), &Column::default_columns(), false);
        let expected = [
            "                                      Loss%   Snt   Last    Avg    EMA  Best   Wrst StDev",
            " 1.|-- gateway.lan                     0.0%     3    3.0    2.0    1.3   1.0    3.0   1.0",
            " 2.|-- ???                           100.0%     3    ???    ???    ???   ???    ???   ???",
            " 3.|-- 195-190-228-107.fixed.kpn.net  33.3%     3   12.5   11.2   10.2  10.0   12.5   1.8",
            " 4.|-- 150.65.7.130                    0.0%     3 1001.0 1001.4 1004.1 998.0 1005.2   3.6",
        ];
        assert_eq!(lines, expected);
    }

    #[test]
    fn numeric_mode_and_custom_fields() {
        let columns = [Column::Hop, Column::Host, Column::Avg, Column::Loss];
        let lines = format_table(&path(), &columns, true);
        let expected = [
            "                               Avg  Loss%",
            " 1.|-- 192.168.1.1             2.0   0.0%",
            " 2.|-- ???                     ??? 100.0%",
            " 3.|-- 195.190.228.107        11.2  33.3%",
            " 4.|-- 150.65.7.130         1001.4   0.0%",
        ];
        assert_eq!(lines, expected);
    }
}
