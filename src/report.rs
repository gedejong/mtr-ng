use crate::args::Column;
use crate::geo::{self, Origin};
use crate::hop_stats::FlowStats;
use crate::icmp_ext::MplsLabel;
use crate::utils::network;
use crate::{topology, tracer, HopStats, MtrSession, Result};
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

fn cell(hop: &HopStats, host: &str, column: Column, origin: Option<&Origin>) -> String {
    match column {
        Column::Hop => format!("{:2}.|--", hop.hop),
        Column::Host => host.to_string(),
        _ => column.value(hop, origin),
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

/// The header line and one line per probed hop, aligned however long the names
/// are. With `mpls`, each hop is followed by the labels its router reported, as
/// `mtr -e` prints them. Distances are from `origin`.
fn format_table(
    hops: &[HopStats],
    columns: &[Column],
    numeric: bool,
    mpls: bool,
    origin: Option<&Origin>,
) -> Vec<String> {
    let columns = &report_columns(columns);
    let hops: Vec<&HopStats> = hops.iter().filter(|hop| hop.sent > 0).collect();
    let rows: Vec<Vec<String>> = hops
        .iter()
        .map(|hop| {
            let host = host_label(hop, numeric);
            columns
                .iter()
                .map(|&c| cell(hop, &host, c, origin))
                .collect()
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

    let headers: Vec<String> = columns.iter().map(|&c| header(c).to_string()).collect();
    let mut lines = vec![align(&headers, columns, &widths)];
    for (hop, row) in hops.iter().zip(&rows) {
        lines.push(align(row, columns, &widths));
        if mpls {
            lines.extend(hop.mpls().iter().map(|label| format!("       {label}")));
        }
    }
    lines
}

/// Host, ASN and location left-aligned, numbers right-aligned, one space between
/// columns.
fn align(cells: &[String], columns: &[Column], widths: &[usize]) -> String {
    let mut line = String::new();
    for (i, ((cell, &column), &width)) in cells.iter().zip(columns).zip(widths).enumerate() {
        if i > 0 {
            line.push(' ');
        }
        let pad = " ".repeat(width.saturating_sub(cell.width()));
        if matches!(column, Column::Host | Column::Asn | Column::Geo) {
            line.push_str(cell);
            line.push_str(&pad);
        } else {
            line.push_str(&pad);
            line.push_str(cell);
        }
    }
    line.trim_end().to_string()
}

/// What the location columns rest on: where distances are measured from, what
/// `!` means when it appears, and the database's credit.
fn geo_notes(
    hops: &[HopStats],
    columns: &[Column],
    origin: Option<&Origin>,
    attribution: Option<&str>,
) -> Vec<String> {
    if !columns
        .iter()
        .any(|c| matches!(c, Column::Geo | Column::Dist))
    {
        return Vec::new();
    }
    let mut notes = Vec::new();
    match origin {
        Some(Origin { hop: Some(n), .. }) => notes.push(format!(
            "Distances from hop {n}, the first one placed in a city"
        )),
        Some(o) => notes.push(format!("Distances from {:.2},{:.2}", o.lat, o.lon)),
        None => {}
    }
    if hops
        .iter()
        .any(|hop| hop.assess(origin).is_some_and(|a| a.impossible))
    {
        notes.push(
            "! The hop's best RTT rules its location out: light in fibre covers \
             about 100 km per ms of round trip"
                .to_string(),
        );
    }
    let named: std::collections::BTreeSet<u16> = hops
        .iter()
        .filter(|hop| hop.geo.as_ref().is_some_and(|fix| fix.from_name.is_some()))
        .map(|hop| u16::from(hop.hop))
        .collect();
    if !named.is_empty() {
        notes.push(format!(
            "Placed by the city code in the router's name: hop {}",
            topology::ranges(&named)
        ));
    }
    if let Some(credit) = attribution {
        notes.push(format!("Locations: {credit}"));
    }
    notes
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
fn json_mpls(labels: &[MplsLabel]) -> Value {
    labels
        .iter()
        .map(|l| json!({"label": l.label, "tc": l.tc, "s": l.bottom, "ttl": l.ttl}))
        .collect()
}

/// mtr's hub fields, plus our own. With more than one flow, `flows[f]` is the
/// address flow `f` met here most often. `geo` holds the location, when known,
/// with the distance from `origin` and the light-speed verdict.
fn json_hubs(
    hops: &[HopStats],
    numeric: bool,
    asn: bool,
    flows: u16,
    origin: Option<&Origin>,
) -> Vec<Value> {
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
            if !hop.mpls().is_empty() {
                hub["MPLS"] = json_mpls(hop.mpls());
            }
            if let Some(fix) = &hop.geo {
                let assessment = hop.assess(origin);
                hub["geo"] = json!({
                    "city": fix.city,
                    "country": fix.country,
                    "lat": fix.lat,
                    "lon": fix.lon,
                    "accuracy_km": fix.accuracy_km,
                    "distance_km": assessment.map(|a| a.distance_km.round()),
                    "impossible": assessment.is_some_and(|a| a.impossible),
                    "from_name": fix.from_name,
                });
            }
            if flows > 1 {
                let per_flow: Vec<Option<String>> = (0..flows)
                    .map(|f| hop.flows.get(&f).and_then(FlowStats::responder))
                    .map(|addr| addr.map(|a| a.to_string()))
                    .collect();
                hub["flows"] = json!(per_flow);
            }
            let alternates: Vec<Value> = hop
                .get_alternate_paths()
                .into_iter()
                .map(|alt| {
                    let mut alternate = json!({
                        "ip": alt.addr.to_string(),
                        "host": alt.hostname.clone().filter(|_| !numeric).unwrap_or_else(|| alt.addr.to_string()),
                        "replies": alt.replies,
                        "Last": json_ms(alt.last_rtt),
                        "Avg": json_ms(alt.avg_rtt()),
                        "Best": json_ms(alt.best_rtt),
                    });
                    if !alt.mpls.is_empty() {
                        alternate["MPLS"] = json_mpls(&alt.mpls);
                    }
                    alternate
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

const CSV_HEADER: &str = "hop,host,ip,asn,loss_pct,sent,last_ms,avg_ms,ema_ms,best_ms,worst_ms,stdev_ms,jitter_ms,jitter_avg_ms,location,lat,lon,dist_km,impossible";

/// A header row, then one row per probed hop. Empty fields mean no reply, or no
/// ASN or location.
fn csv_rows(hops: &[HopStats], numeric: bool, origin: Option<&Origin>) -> Vec<String> {
    let ms = |rtt: Option<Duration>| {
        rtt.map_or(String::new(), |rtt| {
            format!("{:.3}", rtt.as_secs_f64() * 1e3)
        })
    };
    let rows = hops.iter().filter(|hop| hop.sent > 0).map(|hop| {
        let stdev = (hop.received > 0).then(|| format!("{:.3}", hop.stddev_ms()));
        let fix = hop.geo.as_ref();
        let assessment = hop.assess(origin);
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
            fix.map(|f| csv_field(&f.place())).unwrap_or_default(),
            fix.map(|f| f.lat.to_string()).unwrap_or_default(),
            fix.map(|f| f.lon.to_string()).unwrap_or_default(),
            assessment
                .map(|a| format!("{:.0}", a.distance_km))
                .unwrap_or_default(),
            assessment
                .map(|a| a.impossible.to_string())
                .unwrap_or_default(),
        ]
        .join(",")
    });
    std::iter::once(CSV_HEADER.to_string())
        .chain(rows)
        .collect()
}

/// What the hubs' `geo` objects rest on: whose locations they are and where
/// distances are measured from (`hop` is null for `--origin`). None without
/// a location source.
fn json_geo(attribution: Option<&str>, origin: Option<&Origin>) -> Option<Value> {
    Some(json!({
        "attribution": attribution?,
        "origin": origin.map(|o| json!({"lat": o.lat, "lon": o.lon, "hop": o.hop})),
    }))
}

/// Map viewers' colours (simplestyle-spec): a hop its RTT rules out, and the path
const IMPOSSIBLE_MARKER: &str = "#d32f2f";
const PATH_STROKE: &str = "#e6a100";

/// The located hops as GeoJSON (RFC 7946): a point per hop, and the path through
/// the locations their RTT allows, along great circles and cut at the date line.
/// Coordinates are [longitude, latitude]. `title`, `marker-color` and `stroke`
/// follow simplestyle-spec, which map viewers such as geojson.io read.
fn geojson(hops: &[HopStats], numeric: bool, origin: Option<&Origin>) -> Value {
    let lon_lat = |(lat, lon): (f64, f64)| {
        let round = |degrees: f64| (degrees * 1e4).round() / 1e4;
        [round(lon), round(lat)]
    };
    let point = |at| json!({"type": "Point", "coordinates": lon_lat(at)});
    let feature = |geometry, properties| json!({"type": "Feature", "geometry": geometry, "properties": properties});
    let mut features = Vec::new();
    let mut stops = Vec::new();
    for hop in hops.iter().filter(|hop| hop.sent > 0) {
        let (Some(fix), Some(addr)) = (&hop.geo, hop.addr) else {
            continue;
        };
        let assessment = hop.assess(origin);
        let impossible = assessment.is_some_and(|a| a.impossible);
        let mut properties = json!({
            "title": format!("{}. {}", hop.hop, host_label(hop, numeric)),
            "hop": hop.hop,
            "host": host_label(hop, numeric),
            "ip": addr.to_string(),
            "city": fix.city,
            "country": fix.country,
            "accuracy_km": fix.accuracy_km,
            "distance_km": assessment.map(|a| a.distance_km.round()),
            "impossible": impossible,
            "from_name": fix.from_name,
            "loss_pct": (hop.loss_percent * 10.0).round() / 10.0,
            "sent": hop.sent,
            "best_ms": json_ms(hop.best_rtt),
            "avg_ms": json_ms(hop.avg_rtt),
        });
        match impossible {
            true => properties["marker-color"] = json!(IMPOSSIBLE_MARKER),
            false => stops.push((fix.lat, fix.lon)),
        }
        features.push(feature(point((fix.lat, fix.lon)), properties));
    }
    if let Some(o) = origin.filter(|o| o.hop.is_none()) {
        let properties = json!({"title": "--origin", "marker-symbol": "star"});
        features.push(feature(point((o.lat, o.lon)), properties));
    }
    let mut path: Vec<(f64, f64)> = Vec::new();
    for pair in stops.windows(2) {
        let arc = geo::great_circle(pair[0], pair[1], 1.0);
        // Each arc starts where the one before ended
        path.extend(&arc[usize::from(!path.is_empty())..]);
    }
    if path.len() >= 2 {
        let lines: Vec<Vec<[f64; 2]>> = geo::split_at_antimeridian(&path)
            .into_iter()
            .map(|line| line.into_iter().map(lon_lat).collect())
            .collect();
        let geometry = match lines.as_slice() {
            [line] => json!({"type": "LineString", "coordinates": line}),
            _ => json!({"type": "MultiLineString", "coordinates": lines}),
        };
        let properties = json!({"title": "Path", "stroke": PATH_STROKE, "stroke-width": 2});
        // First, so viewers draw the hops over it
        features.insert(0, feature(geometry, properties));
    }
    json!({"type": "FeatureCollection", "features": features})
}

pub async fn run_report(mut session: MtrSession) -> Result<()> {
    let start = chrono::Utc::now();
    let prober = session.open_probe_engine()?;
    let shared = Arc::new(Mutex::new(session));
    tracer::run(Arc::clone(&shared), prober).await?;
    session = shared.lock().unwrap().clone();
    if let Some(why) = session.silence() {
        tracing::warn!("{why}");
    }

    let args = &session.args;
    let hops = &session.hops[..session.display_hop_count()];
    let origin = session.origin();
    let origin = origin.as_ref();
    let source = gethostname::gethostname().to_string_lossy().into_owned();

    if args.json {
        let mut report = json!({
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
                "hubs": json_hubs(hops, args.numeric, args.wants_asn(), args.flows, origin),
            }
        });
        if let Some(geo) = json_geo(session.geo_attribution(), origin) {
            report["report"]["mtr"]["geo"] = geo;
        }
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if args.geojson {
        let mut collection = geojson(hops, args.numeric, origin);
        collection["target"] = json!(session.target);
        collection["start"] = json!(start.to_rfc3339());
        collection["simulated"] = json!(session.is_simulated());
        if let Some(geo) = json_geo(session.geo_attribution(), origin) {
            collection["geo"] = geo;
        }
        println!("{}", serde_json::to_string_pretty(&collection)?);
        return Ok(());
    }
    if args.csv {
        for line in csv_rows(hops, args.numeric, origin) {
            println!("{line}");
        }
        return Ok(());
    }
    if args.dot {
        print!("{}", topology::to_dot(hops, &source, args.numeric));
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
    let columns = args.get_columns();
    for line in format_table(hops, &columns, args.numeric, args.mpls, origin) {
        println!("{line}");
    }
    let notes = geo_notes(hops, &columns, origin, session.geo_attribution());
    if !notes.is_empty() {
        println!();
        notes.iter().for_each(|note| println!("{note}"));
    }
    if args.flows > 1 {
        println!();
        for line in topology::matrix_lines(hops, args.flows, args.numeric) {
            println!("{line}");
        }
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
            hop.increment_sent(0);
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
            silent.increment_sent(0);
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
        let lines = format_table(&path(), &Column::default_columns(), false, false, None);
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
        let lines = format_table(&path(), &columns, true, false, None);
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
        let hubs = json_hubs(&path(), false, false, 1, None);
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
        let with_asn = json_hubs(&path(), false, true, 1, None);
        assert_eq!(with_asn[0]["ASN"], "AS???");
        assert!(hubs[0].get("flows").is_none(), "flows only with --flows");
    }

    #[test]
    fn mpls_labels_follow_their_hop() {
        let mut hops = path();
        let label = MplsLabel {
            label: 24005,
            tc: 0,
            bottom: true,
            ttl: 1,
        };
        let addr = hops[2].addr.unwrap();
        hops[2].record_reply_details(addr, Some(250), &[label]);

        let plain = format_table(&hops, &Column::default_columns(), true, false, None);
        let with_mpls = format_table(&hops, &Column::default_columns(), true, true, None);
        assert_eq!(with_mpls.len(), plain.len() + 1);
        assert_eq!(with_mpls[4], "       [MPLS: Lbl 24005 TC 0 S 1 TTL 1]");
        assert!(
            with_mpls[3].starts_with(" 3.|--"),
            "under hop 3: {with_mpls:?}"
        );

        let hubs = json_hubs(&hops, true, false, 1, None);
        assert_eq!(hubs[2]["MPLS"][0]["label"], 24005);
        assert!(hubs[1].get("MPLS").is_none());
    }

    /// `path()` with hop 3 in The Hague and hop 4 in Tokyo, as seen from hop 3.
    fn located() -> (Vec<HopStats>, Origin) {
        use crate::geo::tests::city;
        let mut hops = path();
        let kpn = IpAddr::from([195, 190, 228, 107]);
        hops[2].set_geo_for_addr(kpn, city("The Hague", "NL", 52.08, 4.31));
        let jaist = IpAddr::from([150, 65, 7, 130]);
        hops[3].set_geo_for_addr(jaist, city("Nomi", "JP", 36.44, 136.59));
        let origin = Origin {
            lat: 52.08,
            lon: 4.31,
            slack_km: 1020.0,
            hop: Some(3),
        };
        (hops, origin)
    }

    #[test]
    fn geojson_draws_the_path_through_the_locations_rtt_allows() {
        let (mut hops, origin) = located();
        let collection = geojson(&hops, true, Some(&origin));
        assert_eq!(collection["type"], "FeatureCollection");
        let features = collection["features"].as_array().unwrap();
        assert_eq!(features.len(), 3, "the path, then hops 3 and 4");
        let path = &features[0]["geometry"];
        assert_eq!(path["type"], "LineString");
        let coordinates = path["coordinates"].as_array().unwrap();
        assert_eq!(coordinates[0], json!([4.31, 52.08]), "longitude first");
        assert_eq!(coordinates.last(), Some(&json!([136.59, 36.44])));
        assert!(
            coordinates.iter().any(|c| c[1].as_f64().unwrap() > 60.0),
            "along the great circle, over Siberia"
        );
        let hague = &features[1];
        assert_eq!(
            hague["geometry"],
            json!({"type": "Point", "coordinates": [4.31, 52.08]})
        );
        assert_eq!(hague["properties"]["title"], "3. 195.190.228.107");
        assert_eq!(hague["properties"]["city"], "The Hague");
        assert_eq!(hague["properties"]["impossible"], false);
        assert!(hague["properties"].get("marker-color").is_none());

        // 10 ms rules Nomi out: red, and no path through it
        hops[3].best_rtt = Some(ms(10.0));
        let given = Origin {
            hop: None,
            ..origin
        };
        let collection = geojson(&hops, true, Some(&given));
        let features = collection["features"].as_array().unwrap();
        let titles: Vec<&str> = features
            .iter()
            .map(|f| f["properties"]["title"].as_str().unwrap())
            .collect();
        assert_eq!(
            titles,
            ["3. 195.190.228.107", "4. 150.65.7.130", "--origin"]
        );
        assert_eq!(features[1]["properties"]["impossible"], true);
        assert_eq!(features[1]["properties"]["marker-color"], IMPOSSIBLE_MARKER);

        // Tokyo to San Francisco: two lines, meeting at the date line
        use crate::geo::tests::city;
        hops[2].set_geo_for_addr(
            IpAddr::from([195, 190, 228, 107]),
            city("Tokyo", "JP", 35.69, 139.75),
        );
        hops[3].set_geo_for_addr(
            IpAddr::from([150, 65, 7, 130]),
            city("San Francisco", "US", 37.77, -122.42),
        );
        hops[3].best_rtt = Some(ms(200.0));
        let collection = geojson(&hops, true, None);
        let path = &collection["features"][0]["geometry"];
        assert_eq!(path["type"], "MultiLineString");
        let lines = path["coordinates"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        let (end, start) = (lines[0].as_array().unwrap().last().unwrap(), &lines[1][0]);
        assert_eq!((&end[0], &start[0]), (&json!(180.0), &json!(-180.0)));
        assert_eq!(end[1], start[1], "the same latitude on both sides");
    }

    #[test]
    fn locations_are_checked_against_the_rtt_and_credited() {
        let (mut hops, origin) = located();
        let columns = [
            Column::Hop,
            Column::Host,
            Column::Geo,
            Column::Dist,
            Column::Best,
        ];
        let lines = format_table(&hops, &columns, true, false, Some(&origin));
        let expected = [
            "                            Location       Dist  Best",
            " 1.|-- 192.168.1.1          ???             ???   1.0",
            " 2.|-- ???                                  ???   ???",
            " 3.|-- 195.190.228.107      The Hague, NL     0  10.0",
            " 4.|-- 150.65.7.130         Nomi, JP       9139 998.0",
        ];
        assert_eq!(lines, expected);
        let notes = geo_notes(&hops, &columns, Some(&origin), Some("Test credit"));
        assert_eq!(
            notes,
            [
                "Distances from hop 3, the first one placed in a city",
                "Locations: Test credit"
            ]
        );
        assert!(geo_notes(&hops, &[Column::Hop], Some(&origin), Some("x")).is_empty());

        // A 9139 km hop answering in 10 ms is somewhere else
        hops[3].best_rtt = Some(ms(10.0));
        let lines = format_table(&hops, &columns, true, false, Some(&origin));
        assert!(
            lines[4].contains("Nomi, JP!") && lines[4].contains("9139!"),
            "{}",
            lines[4]
        );
        let notes = geo_notes(&hops, &columns, Some(&origin), None);
        assert!(
            notes[1].starts_with("! The hop's best RTT rules"),
            "{notes:?}"
        );

        let hubs = json_hubs(&hops, true, false, 1, Some(&origin));
        assert_eq!(hubs[2]["geo"]["city"], "The Hague");
        assert_eq!(hubs[3]["geo"]["distance_km"], 9139.0);
        assert_eq!(hubs[3]["geo"]["impossible"], true);
        assert!(hubs[0].get("geo").is_none());
        let rows = csv_rows(&hops, true, Some(&origin));
        assert!(
            rows[4].ends_with(",\"Nomi, JP\",36.44,136.59,9139,true"),
            "{}",
            rows[4]
        );

        // Its name places it in Tokyo, which 10 ms rules out too
        let tokyo = crate::geo::names::locate("core1.tyo1.he.net").unwrap();
        hops[3].set_geo_for_addr(IpAddr::from([150, 65, 7, 130]), tokyo);
        let notes = geo_notes(&hops, &columns, Some(&origin), Some("Test credit"));
        assert_eq!(
            notes[2..],
            [
                "Placed by the city code in the router's name: hop 4",
                "Locations: Test credit"
            ]
        );
        let hubs = json_hubs(&hops, true, false, 1, Some(&origin));
        assert_eq!(hubs[3]["geo"]["from_name"], "tyo");
        assert_eq!(hubs[2]["geo"]["from_name"], Value::Null);
        assert_eq!(
            json_geo(Some("Test credit"), Some(&origin)),
            Some(json!({
                "attribution": "Test credit",
                "origin": {"lat": 52.08, "lon": 4.31, "hop": 3},
            }))
        );
        assert_eq!(json_geo(None, Some(&origin)), None, "no locations at all");
    }

    #[test]
    fn csv_has_a_header_and_quotes_awkward_names() {
        let mut hops = path();
        hops[0].set_hostname_for_addr(IpAddr::from([192, 168, 1, 1]), "a,\"b\"".to_string());
        let rows = csv_rows(&hops, false, None);
        assert_eq!(rows[0], CSV_HEADER);
        assert_eq!(
            rows[1],
            r#"1,"a,""b""",192.168.1.1,,0.0,3,3.000,2.000,1.290,1.000,3.000,1.000,1.000,1.000,,,,,"#
        );
        assert_eq!(rows[2], "2,???,,,100.0,3,,,,,,,,,,,,,");
        assert_eq!(rows.len(), 5);
    }
}
