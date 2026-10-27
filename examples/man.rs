//! Writes the man page from the CLI definition, the key table and the column list,
//! so it can't drift from the code: `cargo run --example man > install/mtr-ng.1`.
//! `tests/man_page.rs` fails when the checked-in page is stale.

use clap::{CommandFactory, ValueEnum};
use mtr_ng::{args::Column, ui::events::TABLE_KEYS, Args};
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
        "mtr-ng --simulate example.com",
        "Fake data, marked SIMULATED.",
    ),
];

const PRIVILEGES: &str = "On macOS mtr-ng needs no privileges: it uses unprivileged ICMP sockets. \
On Linux it needs root or the CAP_NET_RAW capability \
(setcap cap_net_raw+ep \"$(command -v mtr-ng)\"). \
Windows is not supported yet. Only ICMP over IPv4 is implemented; \
IPv6 targets and other protocols exit with an error.";

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
    for binding in TABLE_KEYS {
        tagged(out, binding.label, binding.help)?;
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

    writeln!(out, ".SH EXAMPLES")?;
    for (command, what) in EXAMPLES {
        tagged(out, command, what)?;
    }

    writeln!(out, ".SH PRIVILEGES\n{}", roff(PRIVILEGES))?;
    writeln!(
        out,
        ".SH REPORTING BUGS\nhttps://github.com/edejong\\-dbc/mtr\\-ng/issues"
    )?;
    writeln!(out, ".SH SEE ALSO\n.BR mtr (8),\n.BR traceroute (8)")
}

fn main() -> io::Result<()> {
    render(&mut io::stdout().lock())
}
