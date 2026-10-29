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
        "mtr-ng -r --fields hop,host,back,loss,avg example.com",
        "Compare each reply's way back with the probe's way there.",
    ),
    (
        "mtr-ng -c 40 --flows 8 --dot example.com | dot -Tsvg > routes.svg",
        "Draw the routes with Graphviz.",
    ),
    (
        "mtr-ng -r -e example.com",
        "Show MPLS labels under their hop.",
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
        "mtr-ng --simulate example.com",
        "Fake data, marked SIMULATED.",
    ),
];

const PRIVILEGES: &str = "On macOS mtr-ng needs no privileges: it reads the routers' answers \
with an unprivileged ICMP socket, for ICMP and UDP probes alike. On Linux, ICMP works without \
privileges through a ping socket when the user's group is in net.ipv4.ping_group_range, as \
every group is on systemd 243 and newer, but a ping socket shows no MPLS labels. UDP probes and \
MPLS labels need root or the CAP_NET_RAW capability \
(setcap cap_net_raw+ep \"$(command -v mtr-ng)\"), which the Debian package grants on \
install. mtr-ng opens its raw sockets first and then gives up these privileges before it \
resolves the target or reads a packet; root keeps only the right to read files. Windows is not \
supported yet, and TCP probes are not implemented yet.";

const BACK: &str = "The back column counts the routers a reply crossed from the TTL it arrived \
with, assuming it left with the next common initial TTL up (32, 64, 128 or 255). When that differs \
from the routers the probe crossed on the way there, the difference follows: 9 +2 is nine \
routers back, two more than there, so the reply came back another way. Routers that send \
replies with an unusual TTL, and MPLS tunnels that hide their routers, skew the count.";

const GEOLOCATION: [&str; 3] = [
    "With -g, the geo or dist columns, or the map (m), mtr-ng places hops with a local \
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
     The probing host is then somewhere within that hop's reach, which widens the margin.",
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

    writeln!(out, ".SH GEOLOCATION\n{}", roff(GEOLOCATION[0]))?;
    writeln!(out, ".PP\n.nf\n{}\n.fi", roff(geo::DOWNLOAD))?;
    for paragraph in &GEOLOCATION[1..] {
        writeln!(out, ".PP\n{}", roff(paragraph))?;
    }

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
