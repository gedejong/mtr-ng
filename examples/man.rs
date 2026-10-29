//! Writes the man page from the CLI definition, the key table and the column list,
//! so it can't drift from the code: `cargo run --example man > install/mtr-ng.1`.
//! `tests/man_page.rs` fails when the checked-in page is stale.

use clap::{CommandFactory, ValueEnum};
use mtr_ng::{
    args::Column,
    geo,
    ui::events::{COLUMN_KEYS, DETAIL_KEYS, TABLE_KEYS},
    Args,
};
use std::io::{self, Write};

const EXAMPLES: &[(&str, &str)] = &[
    ("mtr-ng example.com", "Trace interactively until q."),
    ("mtr-ng -r example.com", "Print a report after 10 rounds."),
    (
        "mtr-ng -r -c 100 -n 1.1.1.1",
        "A 100-round report without DNS.",
    ),
    (
        "mtr-ng --fields hop,host,loss,avg,graph example.com",
        "Show fewer columns.",
    ),
    ("mtr-ng -i 500 example.com", "Probe every 500 ms."),
    (
        "mtr-ng -r -c 40 --flows 8 example.com",
        "Follow 8 flows and show which router each met: load balancers split them.",
    ),
    (
        "mtr-ng -r -c 40 -P udp --flows 8 example.com",
        "The same with UDP probes, whose ports load balancers hash more often than ICMP fields.",
    ),
    (
        "mtr-ng -r -P udp --flows auto example.com",
        "Find every load-balanced branch, 95% sure, then follow one flow per branch. See LOAD BALANCING.",
    ),
    (
        "mtr-ng -r --fields hop,host,back,loss,avg example.com",
        "Compare each reply's way back with the probe's way there.",
    ),
    (
        "mtr-ng -c 40 --flows 8 --dot example.com | dot -Tsvg > routes.svg",
        "Draw the routes with Graphviz.",
    ),
    (
        "mtr-ng -c 40 --flows 8 --mermaid example.com",
        "The routes as a Mermaid flowchart, for Markdown on GitHub or GitLab.",
    ),
    (
        "mtr-ng -r -e example.com",
        "Show MPLS labels and the interfaces routers name (RFC 5837) under their hop.",
    ),
    (
        "mtr-ng --record trace.ndjson example.com",
        "Trace interactively and keep a log of every probe, reply and lookup.",
    ),
    (
        "mtr-ng --replay trace.ndjson --replay-speed 10",
        "Play the log back in the TUI ten times as fast; with -r, print its report at once. See RECORD AND REPLAY.",
    ),
    (
        "mtr-ng -r -n --metrics 9819 example.com",
        "Trace until stopped, for Prometheus to scrape at http://127.0.0.1:9819/metrics. See METRICS.",
    ),
    (
        "mtr-ng -r -c 30 --pmtu example.com",
        "Find the path MTU and the hop where it narrows.",
    ),
    (
        "mtr-ng -g example.com",
        "Show where each hop is; m draws the path on a map. See GEOLOCATION.",
    ),
    (
        "mtr-ng -r -g --origin 52.37,4.89 example.com",
        "Measure distances from Amsterdam, which also tightens the light-speed check.",
    ),
    (
        "mtr-ng -c 20 --geojson example.com > path.geojson",
        "Save the located hops and the path for a map viewer such as geojson.io.",
    ),
    (
        "mtr-ng -r --privacy-hops 2 example.com",
        "A report to share: hops 1 and 2 are hidden and never looked up. See PRIVACY.",
    ),
    (
        "mtr-ng --simulate example.com",
        "Fake data, marked SIMULATED.",
    ),
];

const PRIVACY: &str = "Apart from the probes, mtr-ng sends the target's name and, unless -n, \
the name of every address that answers to this machine's resolver, and with -z every public \
address that answers to Team Cymru's DNS service. Locations come from a local database. \
Private addresses are never looked up anywhere. --privacy-hops N hides the first N hops in \
the TUI and every report format, with the local host name, and never looks them up; branches \
among them stay apart, and they are hidden from --metrics too. With --metrics, anyone who can \
reach its address can read the trace. The a key lists what the running trace sends, and the \
data sources with their licences.";

const RECORD: &str = "--record PATH writes what the trace sees to PATH as it happens, one JSON \
object per line: first what was traced, from where and how, then each probe sent, each reply \
(from whom, its RTT, TTL and MPLS labels) or timeout, and each name and AS that arrived, with \
the seconds since the start. PATH must not exist yet. --replay PATH plays a log back into the \
TUI at the pace it was recorded, or --replay-speed times faster, and with --report prints the \
report at once; it sends nothing and looks nothing up, and any report format works. The path \
MTU search and the branch search of --flows auto are not recorded. Hops hidden by \
--privacy-hops are recorded hidden, so a log can be passed on like a report. A trace of an hour \
at one round a second takes about 10 MB.";

const METRICS: &str = "--metrics [ADDR:]PORT serves the trace at http://ADDR:PORT/metrics while \
it runs, in OpenMetrics to scrapers that ask for it, as Prometheus does, and in the Prometheus \
text format to the rest. A bare port listens on 127.0.0.1. With --report and no --count, mtr-ng \
runs until stopped by Ctrl-C or SIGTERM, then prints the report. Per hop there are counters of \
probes sent, received and lost (mtr_ng_hop_sent_total and so on), the loss ratio, a summary of \
the round-trip times (mtr_ng_hop_rtt_seconds, with quantiles over the last 600 probes and sum \
and count over all), the last, best and worst RTT, its standard deviation and jitter, and per \
address that answered an info metric with its name and AS (mtr_ng_hop_responder_info) and a \
counter of its replies. Every sample is labelled with the target. Ports below 1024 need \
privileges mtr-ng has given up by then.";

const PRIVILEGES: &str = "On macOS mtr-ng needs no privileges: it reads the routers' answers \
with an unprivileged ICMP socket, for ICMP and UDP probes alike. On Linux, ICMP works without \
privileges through a ping socket when the user's group is in net.ipv4.ping_group_range, as \
every group is on systemd 243 and newer, but a ping socket shows no MPLS labels. UDP probes and \
MPLS labels need root or the CAP_NET_RAW capability \
(setcap cap_net_raw+ep \"$(command -v mtr-ng)\"), which the Debian package grants on \
install. FreeBSD has no unprivileged ICMP socket, so mtr-ng needs root there: run it with \
doas or sudo, or install it setuid root. mtr-ng opens its raw sockets first and then gives up \
these privileges before it resolves the target or reads a packet: a setuid install becomes the \
user who ran it, and root keeps only the right to read files. On Windows \
mtr-ng needs no administrator either: it sends echo requests through Windows' ICMP API, which \
builds them itself, so there are no flows, UDP probes, MPLS labels or --pmtu. TCP probes are not \
implemented yet.";

const BACK: &str = "The back column counts the routers a reply crossed from the TTL it arrived \
with, assuming it left with the next common initial TTL up (32, 64, 128 or 255). When that differs \
from the routers the probe crossed on the way there, the difference follows: 9 +2 is nine \
routers back, two more than there, so the reply came back another way. Routers that send \
replies with an unusual TTL, and MPLS tunnels that hide their routers, skew the count.";

const LOAD_BALANCING: &str = "With --flows auto, each round sends a new flow until every hop \
that answers has answered enough flows to rule out a branch no flow met: 6 flows where one address \
answered, 11 where two did, 16 where three did, and 63 for eleven, the most 64 flows can confirm. \
That leaves less than a 5% chance per hop of a missed branch, if load balancers spread flows \
evenly (MDA-Lite). Then the fewest flows that together pass every address take turns, so each \
branch keeps its own statistics, and every ten minutes the search starts over. A note under each \
balanced hop gives its branches; a report without -c runs until the search is done, and ends \
with what it found. Where branches cross between hops, one address leading to several and \
another reached from several, a branch may have been missed: MDA-Lite assumes they don't cross.";

const GEOLOCATION: [&str; 3] = [
    "With -g, the geo, dist or stretch columns, or the map (m), mtr-ng places hops with a local \
     database in MaxMind's format: --geoip-mmdb, else the newest .mmdb in \
     ~/.local/share/mtr-ng, else dbip-city-lite.mmdb or GeoLite2-City.mmdb in \
     /usr/share/GeoIP, /var/lib/GeoIP or Homebrew's var/GeoIP. Lookups never leave the \
     machine, and private addresses are never looked up. DB-IP's free City Lite \
     database (CC BY 4.0) can be fetched with:",
    "A database places a router where its owner registered it, which can be a continent \
     away. Where a router's name carries its network's city code, as Arelion's \
     adm-b3-link.ip.twelve99.net does, that city goes before the database; mtr-ng knows \
     the codes of Arelion, Cogent, Hurricane Electric and Lumen. Either way, a reply \
     cannot come from farther than light in fibre travels in its round trip, about \
     100 km per ms, so a location farther from the origin than the hop's best RTT \
     allows, granting both locations their accuracy, is marked ! and left off the map.",
    "Distances are measured from --origin, or else from the first hop placed in a city. \
     The probing host is then somewhere within that hop's reach, which widens the margin. \
     The stretch column is a hop's best RTT, less that hop's, over the round trip light in \
     fibre needs for the distance: 1.0 is a straight fibre, most paths take 2 or 3. The \
     report and the map also compare the path through the located hops with the great circle.",
];

/// Escapes text for roff, where a backslash starts an escape and `-` is a hyphen.
fn roff(text: &str) -> String {
    text.replace('\\', "\\\\").replace('-', "\\-")
}

fn tagged(out: &mut impl Write, tag: &str, text: &str) -> io::Result<()> {
    writeln!(out, ".TP\n.B {}\n{}", roff(tag), roff(text))
}

pub fn render(out: &mut impl Write) -> io::Result<()> {
    let man = clap_mangen::Man::new(Args::command());
    man.render_title(out)?;
    man.render_name_section(out)?;
    man.render_synopsis_section(out)?;
    man.render_description_section(out)?;
    man.render_options_section(out)?;

    writeln!(out, ".SH KEYS")?;
    for (heading, keys) in [
        (None, TABLE_KEYS),
        (Some("In the column chooser"), COLUMN_KEYS),
        (Some("In the hop details"), DETAIL_KEYS),
    ] {
        if let Some(heading) = heading {
            writeln!(out, ".SS {heading}")?;
        }
        for binding in keys {
            tagged(out, binding.label, binding.help)?;
        }
    }

    writeln!(out, ".SH COLUMNS")?;
    writeln!(
        out,
        "Pick them with \\fB\\-\\-fields\\fR, or with \\fBo\\fR while running. \
         Times are in milliseconds."
    )?;
    for column in Column::all() {
        let name = column.to_possible_value().expect("no skipped columns");
        tagged(out, name.get_name(), column.label())?;
    }
    writeln!(out, ".PP\n{}", roff(BACK))?;

    writeln!(out, ".SH EXAMPLES")?;
    for (command, what) in EXAMPLES {
        tagged(out, command, what)?;
    }

    writeln!(out, ".SH LOAD BALANCING\n{}", roff(LOAD_BALANCING))?;
    writeln!(out, ".SH GEOLOCATION\n{}", roff(GEOLOCATION[0]))?;
    writeln!(out, ".PP\n.nf\n{}\n.fi", roff(geo::DOWNLOAD))?;
    for paragraph in &GEOLOCATION[1..] {
        writeln!(out, ".PP\n{}", roff(paragraph))?;
    }

    writeln!(out, ".SH RECORD AND REPLAY\n{}", roff(RECORD))?;
    writeln!(out, ".SH METRICS\n{}", roff(METRICS))?;
    writeln!(out, ".SH PRIVACY\n{}", roff(PRIVACY))?;
    writeln!(out, ".SH PRIVILEGES\n{}", roff(PRIVILEGES))?;
    writeln!(
        out,
        ".SH REPORTING BUGS\nhttps://github.com/gedejong/mtr\\-ng/issues"
    )?;
    writeln!(out, ".SH SEE ALSO\n.BR mtr (8),\n.BR traceroute (8)")
}

fn main() -> io::Result<()> {
    render(&mut io::stdout().lock())
}
