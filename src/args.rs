use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq)]
pub enum SparklineScale {
    Linear,
    Logarithmic,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq)]
pub enum ProbeProtocol {
    /// ICMP Echo (ping) - default and most common
    Icmp,
    /// UDP probes (not implemented yet)
    Udp,
    /// TCP SYN probes (not implemented yet)
    Tcp,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq)]
pub enum Column {
    /// Hop number
    Hop,
    /// Hostname/IP address
    Host,
    /// Origin AS number (looked up with --aslookup)
    Asn,
    /// Where the hop is, from the geolocation database (see --geo)
    Geo,
    /// Distance from the origin in km (see --origin)
    Dist,
    /// Farthest from here a reply can come in the best RTT, in km
    Reach,
    /// Packet loss percentage
    Loss,
    /// Number of packets sent
    Sent,
    /// Last RTT measurement
    Last,
    /// Average RTT
    Avg,
    /// Exponential moving average RTT
    Ema,
    /// Last jitter value
    Jitter,
    /// Average jitter
    JitterAvg,
    /// Best (minimum) RTT
    Best,
    /// Worst (maximum) RTT
    Worst,
    /// Standard deviation of the RTT
    Stdev,
    /// RTT sparkline graph
    Graph,
}

impl Column {
    /// Get all available columns in default order
    pub fn all() -> Vec<Column> {
        vec![
            Column::Hop,
            Column::Host,
            Column::Asn,
            Column::Geo,
            Column::Dist,
            Column::Reach,
            Column::Loss,
            Column::Sent,
            Column::Last,
            Column::Avg,
            Column::Ema,
            Column::Jitter,
            Column::JitterAvg,
            Column::Best,
            Column::Worst,
            Column::Stdev,
            Column::Graph,
        ]
    }

    /// Get default columns (excludes jitter by default for backwards compatibility)
    pub fn default_columns() -> Vec<Column> {
        vec![
            Column::Hop,
            Column::Host,
            Column::Loss,
            Column::Sent,
            Column::Last,
            Column::Avg,
            Column::Ema,
            Column::Best,
            Column::Worst,
            Column::Graph,
        ]
    }
}

#[derive(Parser, Debug, Clone)]
#[command(name = "mtr-ng")]
#[command(
    about = "A modern implementation of mtr (My Traceroute) with unicode and terminal graphics"
)]
#[command(version = env!("CARGO_PKG_VERSION"))]
pub struct Args {
    /// Target hostname or IP address
    pub target: String,

    /// Number of rounds to run (default: until quit; 10 in report mode)
    #[arg(short, long, value_parser = at_least_one)]
    pub count: Option<usize>,

    /// Time between rounds in milliseconds (at least 100, except with --simulate)
    #[arg(short, long, default_value = "1000", value_parser = clap::value_parser!(u64).range(1..))]
    pub interval: u64,

    /// Maximum number of hops (1-255)
    #[arg(short = 'M', long, default_value = "30", value_parser = clap::value_parser!(u8).range(1..))]
    pub max_hops: u8,

    /// Enable report mode (non-interactive)
    #[arg(short, long)]
    pub report: bool,

    /// Report as JSON, with mtr's field names (implies --report)
    #[arg(short = 'j', long, conflicts_with = "csv")]
    pub json: bool,

    /// Report as CSV, one row per hop (implies --report)
    #[arg(short = 'C', long)]
    pub csv: bool,

    /// Print the routes as a Graphviz graph instead of a table (implies --report).
    /// Most useful with --flows
    #[arg(long, conflicts_with_all = ["json", "csv"])]
    pub dot: bool,

    /// Show IP addresses instead of hostnames
    #[arg(short, long)]
    pub numeric: bool,

    /// Show the AS number of each hop. Asks Team Cymru's DNS about every public address
    #[arg(short = 'z', long)]
    pub aslookup: bool,

    /// Show where each hop is (Location and Dist columns), from a local
    /// geolocation database: nothing is sent anywhere. A location farther away
    /// than light in fibre gets in the hop's best RTT is marked with !
    #[arg(short = 'g', long)]
    pub geo: bool,

    /// Geolocation database in MaxMind's format: DB-IP City Lite or GeoLite2 City.
    /// Default: the newest .mmdb in ~/.local/share/mtr-ng, then the usual system places
    #[arg(long, value_name = "PATH")]
    pub geoip_mmdb: Option<PathBuf>,

    /// Where you are, in degrees, for distances and the plausibility check.
    /// Default: the first hop the database places in a city, with its uncertainty
    #[arg(long, value_name = "LAT,LON", value_parser = coordinates, allow_hyphen_values = true)]
    pub origin: Option<(f64, f64)>,

    /// Show the MPLS labels routers report (RFC 4950) under their hop
    #[arg(short = 'e', long)]
    pub mpls: bool,

    /// Probe this many flows in turn, one per round. All probes of a flow look
    /// alike to load balancers (Paris traceroute), so a flow follows one path;
    /// several flows reveal the paths a load balancer spreads traffic over
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=crate::probe::MAX_FLOWS as i64))]
    pub flows: u16,

    /// Sparkline scaling mode: linear or logarithmic (default: logarithmic)
    #[arg(long, value_enum, default_value = "logarithmic")]
    pub sparkline_scale: SparklineScale,

    /// Smoothing factor for the EMA column, above 0 and at most 1. Higher follows recent RTTs faster
    #[arg(long, default_value = "0.1", value_parser = smoothing_factor)]
    pub ema_alpha: f64,

    /// Select which columns to display (default: hop,host,loss,sent,last,avg,ema,best,worst,graph)
    #[arg(long, value_enum, value_delimiter = ',')]
    pub fields: Option<Vec<Column>>,

    /// Show all available columns including jitter metrics
    #[arg(long, help = "Display all available columns")]
    pub show_all: bool,

    /// Show fake data instead of probing (for demos and testing); marked SIMULATED
    #[arg(long, alias = "force-simulate")]
    pub simulate: bool,

    /// Probe protocol (only icmp is implemented so far)
    #[arg(short = 'P', long, value_enum, default_value = "icmp")]
    pub protocol: ProbeProtocol,

    /// Report mode: print errors only, no warnings
    #[arg(short, long)]
    pub quiet: bool,

    /// Append debug logs to this file (level can be overridden with RUST_LOG)
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,
}

fn at_least_one(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(0) => Err("must be at least 1".to_string()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}

/// `52.37,4.89`: latitude and longitude in degrees.
fn coordinates(s: &str) -> Result<(f64, f64), String> {
    let (lat, lon) = s
        .split_once(',')
        .ok_or("expected LAT,LON, such as 52.37,4.89")?;
    let parse = |v: &str| v.trim().parse::<f64>().map_err(|e| format!("{v}: {e}"));
    let (lat, lon) = (parse(lat)?, parse(lon)?);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err("latitude must be within ±90 and longitude within ±180".to_string());
    }
    Ok((lat, lon))
}

fn smoothing_factor(s: &str) -> Result<f64, String> {
    let alpha: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if alpha > 0.0 && alpha <= 1.0 {
        Ok(alpha)
    } else {
        Err("must be above 0 and at most 1".to_string())
    }
}

/// Shortest interval for real probes: every round sends one probe per hop.
pub const MIN_INTERVAL_MS: u64 = 100;

impl Args {
    /// Checks that depend on more than one flag.
    pub fn validate(&self) -> Result<(), String> {
        if self.interval < MIN_INTERVAL_MS && !self.simulate {
            return Err(format!(
                "--interval {} is too short for real probes: each round sends one probe per hop. \
                 Use at least {MIN_INTERVAL_MS} ms (any value works with --simulate)",
                self.interval
            ));
        }
        Ok(())
    }

    /// Get the columns to display based on command-line arguments
    pub fn get_columns(&self) -> Vec<Column> {
        if self.show_all {
            Column::all()
        } else if let Some(ref fields) = self.fields {
            fields.clone()
        } else {
            let mut columns = Column::default_columns();
            let mut extra = Vec::new();
            if self.aslookup {
                extra.push(Column::Asn);
            }
            if self.geo {
                extra.extend([Column::Geo, Column::Dist]);
            }
            columns.splice(2..2, extra);
            columns
        }
    }

    /// Locations were asked for: `--geo`, or `--fields` naming a column that shows
    /// them. `--show-all` doesn't count, so it works without a database.
    pub fn wants_geo(&self) -> bool {
        let named = |fields: &Vec<Column>| {
            fields
                .iter()
                .any(|c| matches!(c, Column::Geo | Column::Dist))
        };
        self.geo || self.fields.as_ref().is_some_and(named)
    }

    /// AS numbers are looked up for `--aslookup`, or when the ASN column is chosen.
    pub fn wants_asn(&self) -> bool {
        self.aslookup || self.get_columns().contains(&Column::Asn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_args_default_values() {
        let args = Args::try_parse_from(["mtr-ng", "example.com"]).unwrap();
        assert_eq!(args.target, "example.com");
        assert_eq!(args.count, None); // Default is now infinite (None)
        assert_eq!(args.interval, 1000);
        assert_eq!(args.max_hops, 30);
        assert!(!args.report);
        assert!(!args.numeric);
        assert_eq!(args.sparkline_scale, SparklineScale::Logarithmic);
        assert_eq!(args.ema_alpha, 0.1);
        assert!(args.fields.is_none());
        assert!(!args.show_all);
        assert!(!args.simulate);
        assert_eq!(args.protocol, ProbeProtocol::Icmp);
        assert!(!args.quiet);
    }

    #[test]
    fn nonsense_values_are_rejected() {
        for bad in [
            &["--ema-alpha", "NaN"][..],
            &["--ema-alpha", "0"],
            &["--ema-alpha", "-1"],
            &["--ema-alpha", "1.5"],
            &["-i", "0"],
            &["-c", "0"],
            &["-M", "0"],
            &["-M", "256"],
        ] {
            let argv = ["mtr-ng"].iter().chain(bad).chain(&["example.com"]);
            assert!(Args::try_parse_from(argv).is_err(), "{bad:?} accepted");
        }
        let args =
            Args::try_parse_from(["mtr-ng", "--ema-alpha", "1", "--force-simulate", "x"]).unwrap();
        assert_eq!(args.ema_alpha, 1.0);
        assert!(args.simulate, "--force-simulate is an alias of --simulate");
    }

    #[test]
    fn short_intervals_only_for_simulation() {
        let parse = |argv: &[&str]| Args::try_parse_from(argv).unwrap().validate();
        assert!(parse(&["mtr-ng", "-i", "50", "x"]).is_err());
        assert!(parse(&["mtr-ng", "-i", "100", "x"]).is_ok());
        assert!(parse(&["mtr-ng", "-i", "10", "--simulate", "x"]).is_ok());
    }

    #[test]
    fn test_args_custom_values() {
        let args = Args::try_parse_from([
            "mtr-ng",
            "--count",
            "20",
            "--interval",
            "500",
            "--max-hops",
            "50",
            "--report",
            "--numeric",
            "google.com",
        ])
        .unwrap();

        assert_eq!(args.target, "google.com");
        assert_eq!(args.count, Some(20));
        assert_eq!(args.interval, 500);
        assert_eq!(args.max_hops, 50);
        assert!(args.report);
        assert!(args.numeric);
        assert_eq!(args.sparkline_scale, SparklineScale::Logarithmic);
        assert_eq!(args.ema_alpha, 0.1);
        assert!(args.fields.is_none());
        assert!(!args.show_all);
    }

    #[test]
    fn test_args_short_flags() {
        let args = Args::try_parse_from([
            "mtr-ng",
            "-c",
            "15",
            "-i",
            "2000",
            "-M",
            "25",
            "-r",
            "-n",
            "test.example.com",
        ])
        .unwrap();

        assert_eq!(args.target, "test.example.com");
        assert_eq!(args.count, Some(15));
        assert_eq!(args.interval, 2000);
        assert_eq!(args.max_hops, 25);
        assert!(args.report);
        assert!(args.numeric);
        assert_eq!(args.sparkline_scale, SparklineScale::Logarithmic);
        assert_eq!(args.ema_alpha, 0.1);
        assert!(args.fields.is_none());
        assert!(!args.show_all);
    }

    #[test]
    fn geo_adds_its_columns_and_origin_takes_negative_coordinates() {
        let args = Args::try_parse_from(["mtr-ng", "-z", "-g", "x"]).unwrap();
        let columns = args.get_columns();
        assert_eq!(
            columns[1..5],
            [Column::Host, Column::Asn, Column::Geo, Column::Dist]
        );
        assert!(args.wants_geo());
        let wants = |argv: &[&str]| Args::try_parse_from(argv).unwrap().wants_geo();
        assert!(!wants(&["mtr-ng", "--fields", "hop,reach", "x"]));
        assert!(wants(&["mtr-ng", "--fields", "hop,geo", "x"]));
        assert!(
            !wants(&["mtr-ng", "--show-all", "x"]),
            "works without a database"
        );

        let args = Args::try_parse_from(["mtr-ng", "--origin", "-33.87,151.21", "x"]).unwrap();
        assert_eq!(args.origin, Some((-33.87, 151.21)));
        for bad in ["52.3", "91,0", "0,181", "a,b"] {
            assert!(
                Args::try_parse_from(["mtr-ng", "--origin", bad, "x"]).is_err(),
                "{bad}"
            );
        }
    }
}
