use crate::args::Column;
use crate::utils::network;
use crate::{tracer, HopStats, MtrSession, Result};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

/// The host column is at least this wide, and as wide as the longest name.
const HOST_MIN_WIDTH: usize = 20;

/// Numeric columns are at least this wide, so short and long runs line up alike.
const VALUE_MIN_WIDTH: usize = 5;

/// mtr prints the target on its own line, so the host column has no header.
fn header(column: Column) -> &'static str {
    match column {
        Column::Host => "",
        _ => column.header(),
    }
}

fn cell(hop: &HopStats, host: &str, column: Column) -> String {
    match column {
        Column::Hop => format!("{:2}.|--", hop.hop),
        Column::Host => host.to_string(),
        _ => column.value(hop),
    }
}

/// A report can't draw a graph; like mtr it shows the spread in its place.
fn report_columns(columns: &[Column]) -> Vec<Column> {
    let mut shown = Vec::new();
    for &column in columns {
        let column = if column == Column::Graph {
            Column::Stdev
        } else {
            column
        };
        if !shown.contains(&column) {
            shown.push(column);
        }
    }
    shown
}

/// The header line and one line per probed hop, aligned however long the names are.
fn format_table(hops: &[HopStats], columns: &[Column], numeric: bool) -> Vec<String> {
    let columns = &report_columns(columns);
    let rows: Vec<Vec<String>> = hops
        .iter()
        .filter(|hop| hop.sent > 0)
        .map(|hop| {
            let host = host_label(hop, numeric);
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

/// Host and ASN left-aligned, numbers right-aligned, one space between columns.
fn align(cells: &[String], columns: &[Column], widths: &[usize]) -> String {
    let mut line = String::new();
    for (i, ((cell, &column), &width)) in cells.iter().zip(columns).zip(widths).enumerate() {
        if i > 0 {
            line.push(' ');
        }
        let pad = " ".repeat(width.saturating_sub(cell.width()));
        if matches!(column, Column::Host | Column::Asn) {
            line.push_str(cell);
            line.push_str(&pad);
        } else {
            line.push_str(&pad);
            line.push_str(cell);
        }
    }
    line.trim_end().to_string()
}

fn host_label(hop: &HopStats, numeric: bool) -> String {
    if numeric {
        network::format_optional_ip(hop.addr)
    } else {
        network::format_hostname_with_fallback(hop.hostname.clone(), hop.addr)
    }
}

/// Milliseconds rounded to the microsecond, or null without a reply.
fn json_ms(rtt: Option<Duration>) -> Value {
    rtt.map_or(Value::Null, |rtt| {
        json!((rtt.as_secs_f64() * 1e6).round() / 1e3)
    })
}

/// One entry per probed hop, with mtr's field names (`count`, `Loss%`, `Snt`,
/// `Wrst`, ...) so parsers written for `mtr --json` keep working. Extra addresses
/// that answered for a hop are listed under `alternates`.
fn json_hubs(hops: &[HopStats], numeric: bool, asn: bool) -> Vec<Value> {
    let asn_of = |n: Option<u32>| n.map_or("AS???".to_string(), |n| format!("AS{n}"));
    hops.iter()
        .filter(|hop| hop.sent > 0)
        .map(|hop| {
            let mut hub = json!({
                "count": hop.hop,
                "host": host_label(hop, numeric),
                "ip": hop.addr.map(|a| a.to_string()),
                "Loss%": (hop.loss_percent * 10.0).round() / 10.0,
                "Snt": hop.sent,
                "Last": json_ms(hop.last_rtt),
                "Avg": json_ms(hop.avg_rtt),
                "EMA": json_ms(hop.ema_rtt),
                "Best": json_ms(hop.best_rtt),
                "Wrst": json_ms(hop.worst_rtt),
                "StDev": json_ms((hop.received > 0).then(|| Duration::from_secs_f64(hop.stddev_ms() / 1e3))),
                "Jttr": json_ms(hop.last_jitter),
                "JAvg": json_ms(hop.jitter_avg),
            });
            if asn && hop.addr.is_some() {
                hub["ASN"] = json!(asn_of(hop.asn));
            }
            let alternates: Vec<Value> = hop
                .get_alternate_paths()
                .into_iter()
                .map(|alt| {
                    json!({
                        "ip": alt.addr.to_string(),
                        "host": alt.hostname.clone().filter(|_| !numeric).unwrap_or_else(|| alt.addr.to_string()),
                        "replies": alt.replies,
                        "Last": json_ms(alt.last_rtt),
                        "Avg": json_ms(alt.avg_rtt()),
                        "Best": json_ms(alt.best_rtt),
                    })
                })
                .collect();
            if !alternates.is_empty() {
                hub["alternates"] = json!(alternates);
            }
            hub
        })
        .collect()
}

/// Quote a CSV field when it needs it (RFC 4180).
fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

const CSV_HEADER: &str = "hop,host,ip,asn,loss_pct,sent,last_ms,avg_ms,ema_ms,best_ms,worst_ms,stdev_ms,jitter_ms,jitter_avg_ms";

/// A header row, then one row per probed hop. Empty fields mean no reply (or no ASN).
fn csv_rows(hops: &[HopStats], numeric: bool) -> Vec<String> {
    let ms = |rtt: Option<Duration>| {
        rtt.map_or(String::new(), |rtt| {
            format!("{:.3}", rtt.as_secs_f64() * 1e3)
        })
    };
    let rows = hops.iter().filter(|hop| hop.sent > 0).map(|hop| {
        let stdev = (hop.received > 0).then(|| format!("{:.3}", hop.stddev_ms()));
        [
            hop.hop.to_string(),
            csv_field(&host_label(hop, numeric)),
            hop.addr.map(|a| a.to_string()).unwrap_or_default(),
            hop.asn.map(|n| format!("AS{n}")).unwrap_or_default(),
            format!("{:.1}", hop.loss_percent),
            hop.sent.to_string(),
            ms(hop.last_rtt),
            ms(hop.avg_rtt),
            ms(hop.ema_rtt),
            ms(hop.best_rtt),
            ms(hop.worst_rtt),
            stdev.unwrap_or_default(),
            ms(hop.last_jitter),
            ms(hop.jitter_avg),
        ]
        .join(",")
    });
    std::iter::once(CSV_HEADER.to_string())
        .chain(rows)
        .collect()
}

pub async fn run_report(mut session: MtrSession) -> Result<()> {
    let start = chrono::Utc::now();
    match session.open_probe_engine()? {
        None => session.run_simulated_trace().await?,
        Some(prober) => {
            let shared = Arc::new(Mutex::new(session));
            tracer::run(Arc::clone(&shared), prober).await?;
            session = shared.lock().unwrap().clone();
        }
    }

    let args = &session.args;
    let hops = &session.hops[..session.display_hop_count()];
    let source = gethostname::gethostname().to_string_lossy().into_owned();

    if args.json {
        let report = json!({
            "report": {
                "mtr": {
                    "src": source,
                    "dst": session.target,
                    "dst_addr": session.target_addr.to_string(),
                    "tests": session.hops.first().map_or(0, |hop| hop.sent),
                    "start": start.to_rfc3339(),
                    "simulated": session.is_simulated(),
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "hubs": json_hubs(hops, args.numeric, args.wants_asn()),
            }
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if args.csv {
        for line in csv_rows(hops, args.numeric) {
            println!("{line}");
        }
        return Ok(());
    }

    println!("Start: {}", start.format("%Y-%m-%d %H:%M:%S UTC"));
    println!(
        "HOST: {source} → {} ({})",
        session.target, session.target_addr
    );
    if session.is_simulated() {
        println!("SIMULATED: fake data from --simulate, not a real trace");
    }
    println!();
    for line in format_table(hops, &args.get_columns(), args.numeric) {
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

    #[test]
    fn json_uses_mtr_field_names_and_null_for_no_reply() {
        let hubs = json_hubs(&path(), false, false);
        assert_eq!(hubs.len(), 4);
        assert_eq!(hubs[0]["count"], 1);
        assert_eq!(hubs[0]["host"], "gateway.lan");
        assert_eq!(hubs[0]["ip"], "192.168.1.1");
        assert_eq!(hubs[0]["Avg"], 2.0);
        assert_eq!(hubs[0]["StDev"], 1.0);
        assert_eq!(hubs[1]["Loss%"], 100.0);
        assert!(hubs[1]["Avg"].is_null() && hubs[1]["ip"].is_null());
        assert_eq!(hubs[2]["Loss%"], 33.3);
        assert_eq!(hubs[3]["Wrst"], 1005.2);
        assert!(hubs[0].get("ASN").is_none(), "ASN only with --aslookup");
        let with_asn = json_hubs(&path(), false, true);
        assert_eq!(with_asn[0]["ASN"], "AS???");
    }

    #[test]
    fn csv_has_a_header_and_quotes_awkward_names() {
        let mut hops = path();
        hops[0].set_hostname_for_addr(IpAddr::from([192, 168, 1, 1]), "a,\"b\"".to_string());
        let rows = csv_rows(&hops, false);
        assert_eq!(rows[0], CSV_HEADER);
        assert_eq!(
            rows[1],
            r#"1,"a,""b""",192.168.1.1,,0.0,3,3.000,2.000,1.290,1.000,3.000,1.000,1.000,1.000"#
        );
        assert_eq!(rows[2], "2,???,,,100.0,3,,,,,,,,");
        assert_eq!(rows.len(), 5);
    }
}
