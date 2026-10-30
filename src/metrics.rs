//! `--metrics ADDR`: the trace for Prometheus and other scrapers, at
//! `http://ADDR/metrics`, for as long as it runs.
//!
//! The server is a few lines of HTTP/1.1 on a tokio listener, one request per
//! connection. Each scrape renders a redacted copy of the session, so
//! `--privacy-hops` holds here as it does everywhere else. A scraper that asks
//! for OpenMetrics gets it; any other gets the Prometheus text format, which
//! names counters and info metrics differently and has no `# EOF`.

use crate::hop_stats::{HopStats, ResponderStats};
use crate::privacy;
use crate::session::MtrSession;
use crate::utils::text::escape;
use crate::Result;
use anyhow::Context;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// How long a scraper may take over its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest request head read. Scrapers send a few hundred bytes.
const MAX_REQUEST: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// OpenMetrics 1.0
    OpenMetrics,
    /// The Prometheus text format, 0.0.4
    Prometheus,
}

impl Format {
    /// OpenMetrics if the Accept header names it at all.
    pub fn from_accept(accept: Option<&str>) -> Self {
        match accept {
            Some(accept) if accept.contains("application/openmetrics-text") => Self::OpenMetrics,
            _ => Self::Prometheus,
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::OpenMetrics => "application/openmetrics-text; version=1.0.0; charset=utf-8",
            Self::Prometheus => "text/plain; version=0.0.4; charset=utf-8",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Counter,
    Gauge,
    Info,
    Summary,
}

/// Metric families written one after another, each with its metadata first.
struct Exposition {
    format: Format,
    out: String,
    /// The target, on every sample
    target: String,
}

impl Exposition {
    /// Start a family. `name` is its OpenMetrics name: without `_total` for a
    /// counter or `_info` for an info metric, but with its unit.
    fn family(&mut self, name: &str, kind: Kind, help: &str) {
        let (name, kind) = match (self.format, kind) {
            (Format::OpenMetrics, Kind::Counter) => (name.to_string(), "counter"),
            (Format::OpenMetrics, Kind::Info) => (name.to_string(), "info"),
            (Format::Prometheus, Kind::Counter) => (format!("{name}_total"), "counter"),
            (Format::Prometheus, Kind::Info) => (format!("{name}_info"), "gauge"),
            (_, Kind::Gauge) => (name.to_string(), "gauge"),
            (_, Kind::Summary) => (name.to_string(), "summary"),
        };
        let _ = writeln!(self.out, "# TYPE {name} {kind}");
        if self.format == Format::OpenMetrics {
            let unit = ["seconds", "bytes", "ratio"]
                .into_iter()
                .find(|unit| name.ends_with(&format!("_{unit}")));
            if let Some(unit) = unit {
                let _ = writeln!(self.out, "# UNIT {name} {unit}");
            }
        }
        let _ = writeln!(self.out, "# HELP {name} {help}");
    }

    /// One sample, named as both formats name it (`_total`, `_info`, `_sum`).
    fn sample(&mut self, name: &str, labels: &[(&str, String)], value: f64) {
        let _ = write!(self.out, "{name}{{target=\"{}\"", escape(&self.target));
        for (label, value) in labels {
            let _ = write!(self.out, ",{label}=\"{}\"", escape(value));
        }
        let _ = writeln!(self.out, "}} {value}");
    }

    /// A family with a sample for each hop that has a value.
    fn per_hop(
        &mut self,
        hops: &[HopStats],
        (name, kind, help): (&str, Kind, &str),
        value: impl Fn(&HopStats) -> Option<f64>,
    ) {
        self.family(name, kind, help);
        let sample = match kind {
            Kind::Counter => format!("{name}_total"),
            _ => name.to_string(),
        };
        for hop in hops {
            if let Some(value) = value(hop) {
                self.sample(&sample, &[("hop", hop.hop.to_string())], value);
            }
        }
    }
}

fn seconds(rtt: Option<Duration>) -> Option<f64> {
    rtt.map(|rtt| rtt.as_secs_f64())
}

/// The session's statistics in `format`. Redact the session first.
pub fn render(session: &MtrSession, format: Format) -> String {
    let hops = &session.hops[..session.display_hop_count()];
    let mut x = Exposition {
        format,
        out: String::new(),
        target: session.target.clone(),
    };

    x.family("mtr_ng_trace", Kind::Info, "What is traced, and how.");
    let mut labels = vec![
        ("target_addr", session.target_addr.to_string()),
        ("protocol", session.args.protocol.name()),
        ("version", env!("CARGO_PKG_VERSION").to_string()),
    ];
    if session.is_simulated() {
        labels.push(("simulated", "true".to_string()));
    }
    x.sample("mtr_ng_trace_info", &labels, 1.0);

    x.family(
        "mtr_ng_rounds",
        Kind::Counter,
        "Rounds sent, one probe to each hop.",
    );
    let rounds = hops.first().map_or(0, |hop| hop.sent);
    x.sample("mtr_ng_rounds_total", &[], rounds as f64);
    x.family(
        "mtr_ng_path_hops",
        Kind::Gauge,
        "Hops shown: up to the target once it answers.",
    );
    x.sample("mtr_ng_path_hops", &[], hops.len() as f64);
    x.family(
        "mtr_ng_target_reached",
        Kind::Gauge,
        "1 once the target has answered.",
    );
    let reached = hops.iter().any(|hop| hop.is_target && hop.received > 0);
    x.sample("mtr_ng_target_reached", &[], f64::from(u8::from(reached)));
    if let Some(pmtu) = &session.pmtu {
        x.family(
            "mtr_ng_path_mtu_bytes",
            Kind::Gauge,
            "The path MTU, once found (--pmtu).",
        );
        if let Some(mtu) = pmtu.mtu() {
            x.sample("mtr_ng_path_mtu_bytes", &[], f64::from(mtu));
        }
    }

    let count = |n: usize| Some(n as f64);
    x.per_hop(
        hops,
        ("mtr_ng_hop_sent", Kind::Counter, "Probes sent to the hop."),
        |hop| count(hop.sent),
    );
    x.per_hop(
        hops,
        (
            "mtr_ng_hop_received",
            Kind::Counter,
            "Probes the hop answered.",
        ),
        |hop| count(hop.received),
    );
    x.per_hop(
        hops,
        (
            "mtr_ng_hop_lost",
            Kind::Counter,
            "Probes that went unanswered for 2 seconds.",
        ),
        |hop| count(hop.lost()),
    );
    x.per_hop(
        hops,
        (
            "mtr_ng_hop_loss_ratio",
            Kind::Gauge,
            "Share of the probes lost, not counting those still in flight.",
        ),
        |hop| Some(hop.loss_percent / 100.0),
    );

    x.family(
        "mtr_ng_hop_rtt_seconds",
        Kind::Summary,
        "Round-trip times. Quantiles over the last 600 probes, sum and count over all.",
    );
    for hop in hops {
        let label = || ("hop", hop.hop.to_string());
        if let Some(spread) = hop.rtt_spread() {
            let quantiles = [
                ("0.5", spread.p50),
                ("0.9", spread.p90),
                ("0.99", spread.p99),
            ];
            for (quantile, value) in quantiles {
                let labels = [label(), ("quantile", quantile.to_string())];
                x.sample("mtr_ng_hop_rtt_seconds", &labels, value.as_secs_f64());
            }
        }
        x.sample(
            "mtr_ng_hop_rtt_seconds_sum",
            &[label()],
            hop.rtt_sum().as_secs_f64(),
        );
        x.sample(
            "mtr_ng_hop_rtt_seconds_count",
            &[label()],
            hop.received as f64,
        );
    }
    x.per_hop(
        hops,
        (
            "mtr_ng_hop_last_rtt_seconds",
            Kind::Gauge,
            "The latest round-trip time.",
        ),
        |hop| seconds(hop.last_rtt),
    );
    x.per_hop(
        hops,
        (
            "mtr_ng_hop_best_rtt_seconds",
            Kind::Gauge,
            "The shortest round-trip time.",
        ),
        |hop| seconds(hop.best_rtt),
    );
    x.per_hop(
        hops,
        (
            "mtr_ng_hop_worst_rtt_seconds",
            Kind::Gauge,
            "The longest round-trip time.",
        ),
        |hop| seconds(hop.worst_rtt),
    );
    x.per_hop(
        hops,
        (
            "mtr_ng_hop_rtt_stddev_seconds",
            Kind::Gauge,
            "Standard deviation of the round-trip times.",
        ),
        |hop| (hop.received >= 2).then(|| hop.stddev_ms() / 1000.0),
    );
    x.per_hop(
        hops,
        (
            "mtr_ng_hop_jitter_seconds",
            Kind::Gauge,
            "Mean difference between consecutive round-trip times.",
        ),
        |hop| seconds(hop.jitter_avg),
    );

    x.family(
        "mtr_ng_hop_responder",
        Kind::Info,
        "An address that answered for the hop, its name and its AS.",
    );
    for hop in hops {
        for r in responders(hop) {
            let mut labels = vec![("hop", hop.hop.to_string()), ("addr", r.addr.to_string())];
            if let Some(name) = &r.info.hostname {
                labels.push(("host", name.clone()));
            }
            if let Some(asn) = &r.info.asn {
                labels.push(("asn", asn.number.to_string()));
                if let Some(prefix) = &asn.prefix {
                    labels.push(("prefix", prefix.clone()));
                }
                if let Some(name) = &asn.name {
                    labels.push(("as_name", name.clone()));
                }
            }
            x.sample("mtr_ng_hop_responder_info", &labels, 1.0);
        }
    }
    x.family(
        "mtr_ng_hop_responder_replies",
        Kind::Counter,
        "Replies from each address that answered for the hop.",
    );
    for hop in hops {
        for r in responders(hop) {
            let labels = [("hop", hop.hop.to_string()), ("addr", r.addr.to_string())];
            x.sample(
                "mtr_ng_hop_responder_replies_total",
                &labels,
                r.replies as f64,
            );
        }
    }

    if format == Format::OpenMetrics {
        x.out.push_str("# EOF\n");
    }
    x.out
}

/// The addresses that answered for `hop`, in order. Hidden ones are left out:
/// they would all be "(hidden)", and how many there are says something too.
fn responders(hop: &HopStats) -> Vec<&ResponderStats> {
    let mut responders: Vec<_> = hop
        .responders
        .values()
        .filter(|r| !privacy::is_stand_in(r.addr))
        .collect();
    responders.sort_by_key(|r| r.addr);
    responders
}

/// Bind before the trace starts, so that a port in use stops mtr-ng at once.
pub async fn bind(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("--metrics: cannot listen on {addr}"))
}

/// Answer scrapes on `listener` with what `session` has, until dropped.
pub async fn serve(listener: TcpListener, session: Arc<Mutex<MtrSession>>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let session = Arc::clone(&session);
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(REQUEST_TIMEOUT, answer(stream, &session)).await;
                });
            }
            // Out of file descriptors, say: wait rather than spin
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

async fn answer(mut stream: TcpStream, session: &Mutex<MtrSession>) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut buf = [0; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < MAX_REQUEST {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let response = respond(&String::from_utf8_lossy(&head), |format| {
        let mut snapshot = session.lock().unwrap().clone();
        snapshot.redact();
        render(&snapshot, format)
    });
    stream.write_all(&response).await?;
    stream.shutdown().await
}

/// The whole response to a request whose head is `head`; `metrics` makes the
/// body of `/metrics`.
fn respond(head: &str, metrics: impl FnOnce(Format) -> String) -> Vec<u8> {
    const TEXT: &str = "text/plain; charset=utf-8";
    let mut lines = head.lines();
    let mut request = lines.next().unwrap_or_default().split(' ');
    let method = request.next().unwrap_or_default();
    let path = request.next().unwrap_or_default();
    let path = path.split('?').next().unwrap_or_default();
    let accept = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("accept"))
        .map(|(_, value)| value.trim());

    let (status, content_type, body) = match (method, path) {
        ("GET" | "HEAD", "/metrics") => {
            let format = Format::from_accept(accept);
            ("200 OK", format.content_type(), metrics(format))
        }
        ("GET" | "HEAD", "/") => (
            "200 OK",
            TEXT,
            "mtr-ng: the metrics are at /metrics\n".into(),
        ),
        ("GET" | "HEAD", _) => (
            "404 Not Found",
            TEXT,
            "Not found: the metrics are at /metrics\n".into(),
        ),
        (_, "") => ("400 Bad Request", TEXT, "Bad request\n".into()),
        _ => ("405 Method Not Allowed", TEXT, "Only GET and HEAD\n".into()),
    };
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if status.starts_with("405") {
        response.push_str("Allow: GET, HEAD\r\n");
    }
    response.push_str("\r\n");
    if method != "HEAD" {
        response.push_str(&body);
    }
    response.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Args;
    use crate::asn::AsInfo;
    use clap::Parser;
    use std::net::IpAddr;

    /// Three hops: two answer, from one or two addresses, and the third doesn't.
    async fn session(cli: &[&str]) -> MtrSession {
        let cli = ["mtr-ng", "-n"].iter().chain(cli).chain(&["192.0.2.1"]);
        let mut session = MtrSession::new(Args::parse_from(cli)).await.unwrap();
        let ms = Duration::from_millis;
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "198.51.100.1".parse().unwrap();
        let c: IpAddr = "198.51.100.2".parse().unwrap();
        for (i, rtt) in [(0, ms(1)), (1, ms(3))] {
            session.hops[0].increment_sent(0);
            session.hops[0].record_reply(Some(i), Some(a), rtt);
        }
        for (i, from) in [(0, b), (1, c)] {
            session.hops[1].increment_sent(0);
            session.hops[1].record_reply(Some(i), Some(from), ms(10));
        }
        session.hops[1].increment_sent(0);
        session.hops[1].record_timeout(Some(2));
        session.hops[1].increment_sent(0); // in flight
        session.hops[2].increment_sent(0);
        session.set_hostname(b, Some("core \"1\"\\a".to_string()));
        session.set_asn(
            b,
            Some(AsInfo {
                number: 64500,
                prefix: Some("198.51.100.0/24".to_string()),
                name: Some("EXAMPLE".to_string()),
            }),
        );
        session.hops[1].mark_as_target();
        session
    }

    #[tokio::test]
    async fn hops_are_counted_and_timed() {
        let text = render(&session(&[]).await, Format::OpenMetrics);
        let has = |line: &str| text.lines().any(|l| l == line);
        let t = "target=\"192.0.2.1\"";
        assert!(has(&format!("mtr_ng_rounds_total{{{t}}} 2")), "{text}");
        assert!(has(&format!("mtr_ng_path_hops{{{t}}} 2")));
        assert!(has(&format!("mtr_ng_target_reached{{{t}}} 1")));
        assert!(has(&format!("mtr_ng_hop_sent_total{{{t},hop=\"2\"}} 4")));
        assert!(has(&format!(
            "mtr_ng_hop_received_total{{{t},hop=\"2\"}} 2"
        )));
        assert!(
            has(&format!("mtr_ng_hop_lost_total{{{t},hop=\"2\"}} 1")),
            "in flight isn't lost"
        );
        let ratio = format!("mtr_ng_hop_loss_ratio{{{t},hop=\"2\"}} 0.333");
        assert!(
            text.lines().any(|l| l.starts_with(&ratio)),
            "one of three settled"
        );
        assert!(has(&format!(
            "mtr_ng_hop_rtt_seconds{{{t},hop=\"1\",quantile=\"0.5\"}} 0.001"
        )));
        assert!(has(&format!(
            "mtr_ng_hop_rtt_seconds{{{t},hop=\"1\",quantile=\"0.99\"}} 0.003"
        )));
        assert!(has(&format!(
            "mtr_ng_hop_rtt_seconds_sum{{{t},hop=\"1\"}} 0.004"
        )));
        assert!(has(&format!(
            "mtr_ng_hop_rtt_seconds_count{{{t},hop=\"1\"}} 2"
        )));
        assert!(has(&format!(
            "mtr_ng_hop_best_rtt_seconds{{{t},hop=\"1\"}} 0.001"
        )));
        assert!(!text.contains("hop=\"3\""), "past the target: {text}");
        assert!(!text.contains("mtr_ng_path_mtu"), "only with --pmtu");
        assert!(has(&format!(
            "mtr_ng_hop_responder_info{{{t},hop=\"2\",addr=\"198.51.100.1\",\
             host=\"core \\\"1\\\"\\\\a\",asn=\"64500\",prefix=\"198.51.100.0/24\",as_name=\"EXAMPLE\"}} 1"
        )));
        assert!(has(&format!(
            "mtr_ng_hop_responder_replies_total{{{t},hop=\"2\",addr=\"198.51.100.2\"}} 1"
        )));
        assert!(text.ends_with("# EOF\n"));
    }

    #[tokio::test]
    async fn the_two_formats_name_counters_and_info_metrics_differently() {
        let session = session(&[]).await;
        let open = render(&session, Format::OpenMetrics);
        let prometheus = render(&session, Format::Prometheus);
        for line in [
            "# TYPE mtr_ng_hop_sent counter",
            "# TYPE mtr_ng_trace info",
            "# UNIT mtr_ng_hop_rtt_seconds seconds",
            "# UNIT mtr_ng_hop_loss_ratio ratio",
        ] {
            assert!(open.lines().any(|l| l == line), "{line}");
        }
        for line in [
            "# TYPE mtr_ng_hop_sent_total counter",
            "# TYPE mtr_ng_trace_info gauge",
            "# TYPE mtr_ng_hop_rtt_seconds summary",
        ] {
            assert!(prometheus.lines().any(|l| l == line), "{line}");
        }
        assert!(!prometheus.contains("# UNIT") && !prometheus.contains("# EOF"));
        let samples = |text: &str| -> Vec<String> {
            text.lines()
                .filter(|l| !l.starts_with('#'))
                .map(str::to_string)
                .collect()
        };
        assert_eq!(samples(&open), samples(&prometheus), "the same samples");
    }

    #[tokio::test]
    async fn hidden_hops_show_no_responders() {
        let mut session = session(&["--privacy-hops", "1"]).await;
        session.redact();
        let text = render(&session, Format::Prometheus);
        assert!(
            !text.contains("10.0.0.1") && !text.contains("hidden"),
            "{text}"
        );
        assert!(text.contains("mtr_ng_hop_sent_total{target=\"192.0.2.1\",hop=\"1\"} 2"));
        assert!(text.contains("addr=\"198.51.100.1\""));
    }

    #[test]
    fn requests_are_routed_and_the_format_negotiated() {
        let answer = |head: &str| {
            let mut asked = None;
            let response = respond(head, |format| {
                asked = Some(format);
                "body\n".to_string()
            });
            (String::from_utf8(response).unwrap(), asked)
        };
        let prometheus = "GET /metrics HTTP/1.1\r\nHost: x\r\nAccept: \
            application/openmetrics-text;version=1.0.0,text/plain;version=0.0.4;q=0.5\r\n\r\n";
        let (response, asked) = answer(prometheus);
        assert_eq!(asked, Some(Format::OpenMetrics));
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type: application/openmetrics-text; version=1.0.0"));
        assert!(response.ends_with("Content-Length: 5\r\nConnection: close\r\n\r\nbody\n"));

        let (response, asked) = answer("GET /metrics?x=1 HTTP/1.0\r\naccept: */*\r\n\r\n");
        assert_eq!(asked, Some(Format::Prometheus));
        assert!(response.contains("Content-Type: text/plain; version=0.0.4"));

        let (response, _) = answer("HEAD /metrics HTTP/1.1\r\n\r\n");
        assert!(
            response.ends_with("Content-Length: 5\r\nConnection: close\r\n\r\n"),
            "no body"
        );

        let (response, asked) = answer("GET /other HTTP/1.1\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 404 "));
        assert_eq!(asked, None);
        let (response, _) = answer("POST /metrics HTTP/1.1\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 405 ") && response.contains("Allow: GET, HEAD\r\n"));
        let (response, _) = answer("");
        assert!(response.starts_with("HTTP/1.1 400 "));
    }

    #[tokio::test]
    async fn a_scrape_over_tcp_gets_the_trace() {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Mutex::new(session(&[]).await));
        let server = tokio::spawn(serve(listener, shared));
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        server.abort();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("mtr_ng_hop_sent_total{target=\"192.0.2.1\",hop=\"1\"} 2"));
    }
}
