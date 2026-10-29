//! Where an address is, from a local database in MaxMind's format: DB-IP's City
//! Lite (free, CC BY 4.0) or MaxMind's GeoLite2 City. Lookups never leave the
//! machine, and private and special addresses are never looked up.
//!
//! A database places a router where its owner registered it, which can be a
//! continent away. A city code in the router's name ([`names`]) goes before it,
//! and every location is checked against the speed of light: a reply can't have
//! come from farther than light in fibre gets in its RTT.

pub mod ipmap;
mod name_codes;
pub mod names;

use anyhow::{Context, Result};
use maxminddb::{geoip2, Reader};
use std::{
    env, fs,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tracing::{info, warn};

/// Light in fibre covers about 200 km per millisecond, so a round trip of one
/// millisecond reaches at most 100 km.
pub const KM_PER_RTT_MS: f64 = 100.0;

/// Accuracy assumed for a city-level location whose database gives none, as
/// DB-IP's free edition doesn't.
const DEFAULT_ACCURACY_KM: f64 = 100.0;

const EARTH_RADIUS_KM: f64 = 6371.0;

/// How to get the free database, for the map and the docs. It is unpacked next
/// to its final name and renamed, so a running mtr-ng never maps a half-written
/// file; `.part` isn't `.mmdb`, so a failed download is never picked up. The URL
/// is quoted because zsh's url-quote-magic (on in oh-my-zsh) escapes the `(` of an
/// unquoted `$(` in a pasted URL, which breaks the command.
pub const DOWNLOAD: &str = "mkdir -p ~/.local/share/mtr-ng && \
curl -fL \"https://download.db-ip.com/free/dbip-city-lite-$(date +%Y-%m).mmdb.gz\" | \
gunzip > ~/.local/share/mtr-ng/dbip.part && \
mv ~/.local/share/mtr-ng/dbip.part ~/.local/share/mtr-ng/dbip-city-lite.mmdb";

/// Where an address is, as a database, the router's name or RIPE IPmap has it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GeoFix {
    pub lat: f64,
    pub lon: f64,
    /// Radius the database is fairly sure of, when it says
    pub accuracy_km: Option<u16>,
    pub city: Option<String>,
    /// ISO 3166 country code
    pub country: Option<String>,
    /// The city code in the router's name this comes from, such as `adm`; None
    /// when it comes from the database
    pub from_name: Option<String>,
    /// RIPE IPmap's score, when it comes from there (`--ipmap`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ipmap: Option<u8>,
}

impl GeoFix {
    /// `Amsterdam, NL`, or the country alone when the database knows no city.
    pub fn place(&self) -> String {
        match (&self.city, &self.country) {
            (Some(city), Some(country)) => format!("{city}, {country}"),
            (Some(place), None) | (None, Some(place)) => place.clone(),
            (None, None) => format!("{:.2}, {:.2}", self.lat, self.lon),
        }
    }

    /// Country-level locations sit at the country's centre: too coarse to
    /// measure distances with.
    pub fn is_city(&self) -> bool {
        self.city.is_some()
    }

    pub fn accuracy(&self) -> f64 {
        self.accuracy_km.map_or(DEFAULT_ACCURACY_KM, f64::from)
    }
}

/// Great-circle distance between two (latitude, longitude) points in degrees.
pub fn distance_km(from: (f64, f64), to: (f64, f64)) -> f64 {
    let (lat1, lat2) = (from.0.to_radians(), to.0.to_radians());
    let dlat = lat2 - lat1;
    let dlon = (to.1 - from.1).to_radians();
    let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().min(1.0).asin()
}

/// Points along the great circle from `from` to `to` (latitude, longitude in
/// degrees), at most `step_deg` apart, both ends included. Long routes bow
/// towards the pole, as they do on a globe.
pub fn great_circle(from: (f64, f64), to: (f64, f64), step_deg: f64) -> Vec<(f64, f64)> {
    let unit = |(lat, lon): (f64, f64)| {
        let (lat, lon) = (f64::to_radians(lat), f64::to_radians(lon));
        [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
    };
    let angle = distance_km(from, to) / EARTH_RADIUS_KM;
    if angle.sin().abs() < 1e-9 {
        return vec![from, to]; // the same place, or opposite ends of the earth
    }
    let (a, b) = (unit(from), unit(to));
    let steps = (angle.to_degrees() / step_deg).ceil().max(1.0) as usize;
    (0..=steps)
        .map(|i| {
            let t = i as f64 / steps as f64;
            let wa = ((1.0 - t) * angle).sin() / angle.sin();
            let wb = (t * angle).sin() / angle.sin();
            let [x, y, z] = [0, 1, 2].map(|k| wa * a[k] + wb * b[k]);
            (z.atan2(x.hypot(y)).to_degrees(), y.atan2(x).to_degrees())
        })
        .collect()
}

/// `points` (latitude, longitude) as lines that don't cross the antimeridian:
/// where a step jumps across ±180°, one line ends on it and the next starts on
/// the other side, at the latitude where the step crosses. GeoJSON asks for this
/// (RFC 7946, 3.1.9), and a map drawn from -180° to 180° needs it too.
pub fn split_at_antimeridian(points: &[(f64, f64)]) -> Vec<Vec<(f64, f64)>> {
    let mut lines = vec![Vec::new()];
    for (i, &(lat, lon)) in points.iter().enumerate() {
        if let Some(&(prev_lat, prev_lon)) = i.checked_sub(1).map(|j| &points[j]) {
            if (lon - prev_lon).abs() > 180.0 {
                let edge = 180f64.copysign(prev_lon);
                let unwrapped = lon + 360f64.copysign(prev_lon);
                let t = (edge - prev_lon) / (unwrapped - prev_lon);
                let cut = prev_lat + t * (lat - prev_lat);
                lines.last_mut().unwrap().push((cut, edge));
                lines.push(vec![(cut, -edge)]);
            }
        }
        lines.last_mut().unwrap().push((lat, lon));
    }
    lines
}

/// Farthest away a reply can have come from, given its round trip.
pub fn reach_km(rtt: Duration) -> f64 {
    rtt.as_secs_f64() * 1000.0 * KM_PER_RTT_MS
}

/// Where distances are measured from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Origin {
    pub lat: f64,
    pub lon: f64,
    /// How far the probing host may be from here: nothing for `--origin`, else
    /// the first located hop's accuracy plus its reach
    pub slack_km: f64,
    /// The hop it was taken from; `None` for `--origin`
    pub hop: Option<u8>,
    /// That hop's best RTT, zero for `--origin`. Stretch counts from there, so
    /// the delay of the local network and the access line isn't held against
    /// the rest of the path.
    pub rtt: Duration,
}

/// Nearer than this, an RTT is mostly the routers' own delay, and a hop's
/// stretch or a path's detour says little.
const STRETCH_MIN_KM: f64 = 100.0;

/// A located hop seen from the origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Assessment {
    pub distance_km: f64,
    /// Farther from the origin than light in fibre gets in the hop's best RTT,
    /// even granting both locations their accuracy: the location is wrong.
    pub impossible: bool,
    /// The best RTT past the origin's, over the round trip light in fibre needs
    /// for the distance: 1 is a straight fibre, and most paths take 2 or 3.
    /// `None` without a reply, or when the hop is too near to tell.
    pub stretch: Option<f64>,
}

/// How `fix` squares with `best_rtt`; `None` for a country-level fix.
pub fn assess(fix: &GeoFix, best_rtt: Option<Duration>, origin: &Origin) -> Option<Assessment> {
    if !fix.is_city() {
        return None;
    }
    let distance_km = distance_km((origin.lat, origin.lon), (fix.lat, fix.lon));
    let bound = |rtt| reach_km(rtt) + fix.accuracy() + origin.slack_km;
    let stretch = best_rtt
        .and_then(|rtt| rtt.checked_sub(origin.rtt))
        .filter(|rtt| !rtt.is_zero() && distance_km >= STRETCH_MIN_KM.max(fix.accuracy()))
        .map(|rtt| reach_km(rtt) / distance_km);
    Some(Assessment {
        distance_km,
        impossible: best_rtt.is_some_and(|rtt| distance_km > bound(rtt)),
        stretch,
    })
}

/// How far the path goes through the located hops, against the great circle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Inflation {
    /// From the origin through each located hop to the last
    pub path_km: f64,
    /// From the origin straight to the last located hop
    pub direct_km: f64,
    /// The last located hop
    pub to_hop: u8,
}

impl Inflation {
    pub fn ratio(&self) -> f64 {
        self.path_km / self.direct_km
    }
}

/// The detour of the path from `origin` through `stops`, (hop, (latitude,
/// longitude)) in path order. `None` without a stop between the origin and the
/// last, or when the last is too near the origin to tell.
pub fn inflation(origin: &Origin, stops: &[(u8, (f64, f64))]) -> Option<Inflation> {
    let (&(to_hop, last), between) = stops.split_last()?;
    let from = (origin.lat, origin.lon);
    let direct_km = distance_km(from, last);
    if between.is_empty() || direct_km < STRETCH_MIN_KM {
        return None;
    }
    let points: Vec<(f64, f64)> = std::iter::once(from)
        .chain(stops.iter().map(|&(_, at)| at))
        .collect();
    let path_km = points.windows(2).map(|w| distance_km(w[0], w[1])).sum();
    Some(Inflation {
        path_km,
        direct_km,
        to_hop,
    })
}

/// Something that can place an address.
pub trait GeoSource: Send + Sync {
    /// Where `addr` is, if known. Callers don't ask about private addresses.
    fn locate(&self, addr: IpAddr) -> Option<GeoFix>;
    /// The credit its licence asks for wherever its data is shown.
    fn attribution(&self) -> &str;
}

/// A database in MaxMind's format with the City layout.
pub struct Mmdb<S: AsRef<[u8]>> {
    reader: Reader<S>,
    attribution: String,
}

impl<S: AsRef<[u8]>> Mmdb<S> {
    pub fn new(reader: Reader<S>) -> Self {
        let attribution = attribution(&reader.metadata().database_type);
        Self {
            reader,
            attribution,
        }
    }
}

impl<S: AsRef<[u8]> + Send + Sync> GeoSource for Mmdb<S> {
    fn locate(&self, addr: IpAddr) -> Option<GeoFix> {
        let city: geoip2::City = self.reader.lookup(addr).ok()?.decode().ok()??;
        Some(GeoFix {
            lat: city.location.latitude?,
            lon: city.location.longitude?,
            accuracy_km: city.location.accuracy_radius,
            city: city.city.names.english.map(str::to_string),
            country: city.country.iso_code.map(str::to_string),
            from_name: None,
            from_ipmap: None,
        })
    }

    fn attribution(&self) -> &str {
        &self.attribution
    }
}

/// The credit each licence asks for: DB-IP's CC BY 4.0 and MaxMind's GeoLite2
/// licence both require one.
fn attribution(database_type: &str) -> String {
    let lower = database_type.to_ascii_lowercase();
    if lower.contains("geolite2") {
        "GeoLite2 data by MaxMind, maxmind.com".to_string()
    } else if lower.contains("geoip2") {
        "GeoIP2 data by MaxMind, maxmind.com".to_string()
    } else if lower.contains("dbip") || lower.contains("db-ip") {
        "IP geolocation by DB-IP, db-ip.com".to_string()
    } else {
        format!("Locations from {database_type}")
    }
}

/// Opens a database. It is memory-mapped, since the free ones are over 100 MB.
pub fn open_mmdb(path: &Path) -> Result<Arc<dyn GeoSource>> {
    // SAFETY: the mapping is only sound while nobody truncates or rewrites the file
    // in place. geoipupdate and the documented download (`DOWNLOAD`) replace it
    // with a rename, which leaves the mapped original intact.
    let reader = unsafe { Reader::open_mmap(path) }
        .with_context(|| format!("cannot read the geolocation database {}", path.display()))?;
    Ok(Arc::new(Mmdb::new(reader)))
}

/// `$XDG_DATA_HOME/mtr-ng`, or `~/.local/share/mtr-ng`.
pub fn user_dir() -> Option<PathBuf> {
    let base = match env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(data) => PathBuf::from(data),
        None => PathBuf::from(env::var_os("HOME")?).join(".local/share"),
    };
    Some(base.join("mtr-ng"))
}

/// Where geoipupdate and the distributions put databases.
const SYSTEM_DIRS: [&str; 5] = [
    "/usr/share/GeoIP",
    "/usr/local/share/GeoIP",
    "/var/lib/GeoIP",
    "/opt/homebrew/var/GeoIP",
    "/usr/local/var/GeoIP",
];
const SYSTEM_NAMES: [&str; 3] = [
    "dbip-city-lite.mmdb",
    "GeoLite2-City.mmdb",
    "GeoIP2-City.mmdb",
];

/// Databases to try, in order: every `.mmdb` in the user directory (DB-IP's
/// dated files newest first), then the known names in the system directories.
fn candidates(user_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut own: Vec<PathBuf> = user_dir
        .and_then(|dir| fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "mmdb"))
        .collect();
    own.sort_by(|a, b| b.cmp(a));
    let system = SYSTEM_DIRS.iter().flat_map(|dir| {
        SYSTEM_NAMES
            .iter()
            .map(move |name| Path::new(dir).join(name))
    });
    own.into_iter().chain(system).collect()
}

/// The database at `explicit`, or the first one found; `None` when there is
/// none. Only an explicit path that can't be read is an error.
pub fn open(explicit: Option<&Path>) -> Result<Option<Arc<dyn GeoSource>>> {
    if let Some(path) = explicit {
        return open_mmdb(path).map(Some);
    }
    for path in candidates(user_dir().as_deref())
        .iter()
        .filter(|path| path.is_file())
    {
        match open_mmdb(path) {
            Ok(source) => {
                info!("Geolocation from {}", path.display());
                return Ok(Some(source));
            }
            Err(e) => warn!("{e:#}"),
        }
    }
    Ok(None)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A source that knows a fixed set of addresses.
    pub struct FakeGeo(pub HashMap<IpAddr, GeoFix>);

    impl GeoSource for FakeGeo {
        fn locate(&self, addr: IpAddr) -> Option<GeoFix> {
            self.0.get(&addr).cloned()
        }

        fn attribution(&self) -> &str {
            "test locations"
        }
    }

    pub fn city(name: &str, country: &str, lat: f64, lon: f64) -> GeoFix {
        GeoFix {
            lat,
            lon,
            accuracy_km: Some(20),
            city: Some(name.to_string()),
            country: Some(country.to_string()),
            from_name: None,
            from_ipmap: None,
        }
    }

    /// A value in the MaxMind DB data format.
    enum V {
        S(&'static str),
        D(f64),
        U16(u16),
        U32(u32),
        U64(u64),
        M(Vec<(&'static str, V)>),
        A(Vec<V>),
    }

    fn control(out: &mut Vec<u8>, kind: u8, size: usize) {
        let size = u8::try_from(size).ok().filter(|&s| s < 29).unwrap();
        match kind {
            0..=7 => out.push(kind << 5 | size),
            _ => out.extend([size, kind - 7]),
        }
    }

    fn encode(value: &V, out: &mut Vec<u8>) {
        match value {
            V::S(s) => {
                control(out, 2, s.len());
                out.extend(s.as_bytes());
            }
            V::D(d) => {
                control(out, 3, 8);
                out.extend(d.to_be_bytes());
            }
            V::U16(n) => {
                control(out, 5, 2);
                out.extend(n.to_be_bytes());
            }
            V::U32(n) => {
                control(out, 6, 4);
                out.extend(n.to_be_bytes());
            }
            V::U64(n) => {
                control(out, 9, 8);
                out.extend(n.to_be_bytes());
            }
            V::M(pairs) => {
                control(out, 7, pairs.len());
                for (key, value) in pairs {
                    encode(&V::S(key), out);
                    encode(value, out);
                }
            }
            V::A(items) => {
                control(out, 11, items.len());
                items.iter().for_each(|item| encode(item, out));
            }
        }
    }

    /// An IPv4 database (24-bit records) mapping each network to its record.
    fn mmdb(database_type: &'static str, networks: Vec<([u8; 4], u8, V)>) -> Vec<u8> {
        #[derive(Clone, Copy)]
        enum Record {
            Empty,
            Node(usize),
            Data(usize),
        }
        let mut nodes = vec![[Record::Empty; 2]];
        let mut data = Vec::new();
        for (ip, prefix, value) in networks {
            let offset = data.len();
            encode(&value, &mut data);
            let bits = u32::from_be_bytes(ip);
            let mut node = 0;
            for depth in 0..prefix {
                let bit = ((bits >> (31 - depth)) & 1) as usize;
                if depth + 1 == prefix {
                    nodes[node][bit] = Record::Data(offset);
                } else if let Record::Node(next) = nodes[node][bit] {
                    node = next;
                } else {
                    nodes.push([Record::Empty; 2]);
                    nodes[node][bit] = Record::Node(nodes.len() - 1);
                    node = nodes.len() - 1;
                }
            }
        }
        let count = nodes.len();
        let mut out = Vec::new();
        for record in nodes.iter().flatten() {
            let value = match *record {
                Record::Empty => count,
                Record::Node(next) => next,
                Record::Data(offset) => count + 16 + offset,
            };
            out.extend(&(value as u32).to_be_bytes()[1..]);
        }
        out.extend([0; 16]);
        out.extend(data);
        out.extend(b"\xab\xcd\xefMaxMind.com");
        let metadata = V::M(vec![
            ("node_count", V::U32(count as u32)),
            ("record_size", V::U16(24)),
            ("ip_version", V::U16(4)),
            ("database_type", V::S(database_type)),
            ("languages", V::A(vec![V::S("en")])),
            ("binary_format_major_version", V::U16(2)),
            ("binary_format_minor_version", V::U16(0)),
            ("build_epoch", V::U64(0)),
            ("description", V::M(vec![("en", V::S("test"))])),
        ]);
        encode(&metadata, &mut out);
        out
    }

    fn city_record(name: &'static str, country: &'static str, lat: f64, lon: f64) -> V {
        V::M(vec![
            (
                "city",
                V::M(vec![("names", V::M(vec![("en", V::S(name))]))]),
            ),
            ("country", V::M(vec![("iso_code", V::S(country))])),
            (
                "location",
                V::M(vec![
                    ("accuracy_radius", V::U16(20)),
                    ("latitude", V::D(lat)),
                    ("longitude", V::D(lon)),
                ]),
            ),
        ])
    }

    #[test]
    fn reads_cities_countries_and_the_licence_credit() {
        let country_only = V::M(vec![
            ("country", V::M(vec![("iso_code", V::S("US"))])),
            (
                "location",
                V::M(vec![("latitude", V::D(37.75)), ("longitude", V::D(-97.82))]),
            ),
        ]);
        let bytes = mmdb(
            "DBIP-City-Lite",
            vec![
                (
                    [192, 0, 2, 0],
                    24,
                    city_record("Amsterdam", "NL", 52.37, 4.89),
                ),
                ([198, 51, 100, 0], 24, country_only),
            ],
        );
        let db = Mmdb::new(Reader::from_source(bytes).unwrap());

        let amsterdam = db.locate("192.0.2.77".parse().unwrap()).unwrap();
        assert_eq!(amsterdam, city("Amsterdam", "NL", 52.37, 4.89));
        assert_eq!(amsterdam.place(), "Amsterdam, NL");

        let us = db.locate("198.51.100.1".parse().unwrap()).unwrap();
        assert_eq!(
            (us.place(), us.is_city(), us.accuracy()),
            ("US".into(), false, 100.0)
        );

        assert_eq!(db.locate("203.0.113.1".parse().unwrap()), None);
        assert_eq!(
            db.locate("2001:db8::1".parse().unwrap()),
            None,
            "IPv4-only database"
        );
        assert_eq!(db.attribution(), "IP geolocation by DB-IP, db-ip.com");
        assert!(attribution("GeoLite2-City").contains("MaxMind"));
    }

    #[test]
    fn distances_follow_the_great_circle() {
        let amsterdam = (52.37, 4.89);
        let london = (51.51, -0.13);
        assert!((distance_km(amsterdam, london) - 357.0).abs() < 5.0);
        assert!((distance_km((0.0, 0.0), (0.0, 180.0)) - 20_015.0).abs() < 1.0);
        assert_eq!(distance_km(amsterdam, amsterdam), 0.0);
        assert_eq!(reach_km(Duration::from_millis(10)), 1000.0);
    }

    #[test]
    fn great_circles_bow_towards_the_pole() {
        let (new_york, tokyo) = ((40.71, -74.01), (35.68, 139.69));
        let arc = great_circle(new_york, tokyo, 2.0);
        let close =
            |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6;
        assert!(close(arc[0], new_york) && close(*arc.last().unwrap(), tokyo));
        let top = arc.iter().map(|p| p.0).fold(f64::MIN, f64::max);
        assert!(top > 65.0, "over Alaska, not across the Pacific: {top}");
        assert!(arc
            .windows(2)
            .all(|w| distance_km(w[0], w[1]) < 2.0 * 111.2 + 1.0));
        assert_eq!(great_circle(tokyo, tokyo, 2.0).len(), 2);
    }

    #[test]
    fn lines_across_the_date_line_end_on_it() {
        let tokyo_sf = great_circle((35.69, 139.75), (37.77, -122.42), 2.0);
        let lines = split_at_antimeridian(&tokyo_sf);
        assert_eq!(lines.len(), 2);
        let (end, start) = (*lines[0].last().unwrap(), lines[1][0]);
        assert_eq!((end.1, start.1), (180.0, -180.0));
        assert_eq!(end.0, start.0, "cut at one latitude");
        assert!(end.0 > 45.0, "the arc bows north: {}", end.0);
        assert_eq!(lines.concat().len(), tokyo_sf.len() + 2);

        let westward = split_at_antimeridian(&[(0.0, -179.0), (2.0, 179.0)]);
        assert_eq!(
            westward,
            [
                vec![(0.0, -179.0), (1.0, -180.0)],
                vec![(1.0, 180.0), (2.0, 179.0)]
            ]
        );
        let amsterdam_tokyo = great_circle((52.37, 4.89), (35.69, 139.75), 2.0);
        assert_eq!(split_at_antimeridian(&amsterdam_tokyo), [amsterdam_tokyo]);
    }

    #[test]
    fn a_location_farther_than_light_gets_is_impossible() {
        let origin = Origin {
            lat: 52.37,
            lon: 4.89,
            slack_km: 0.0,
            hop: None,
            rtt: Duration::ZERO,
        };
        let ashburn = city("Ashburn", "US", 39.04, -77.49);
        let ms = |n| Some(Duration::from_millis(n));
        let far = assess(&ashburn, ms(20), &origin).unwrap();
        assert!(far.impossible, "6200 km in 20 ms");
        assert!((far.distance_km - 6_200.0).abs() < 100.0);
        assert!(!assess(&ashburn, ms(85), &origin).unwrap().impossible);
        assert!(
            !assess(&ashburn, None, &origin).unwrap().impossible,
            "no RTT, no verdict"
        );

        let generous = Origin {
            slack_km: 5_000.0,
            ..origin
        };
        assert!(!assess(&ashburn, ms(20), &generous).unwrap().impossible);

        let us = GeoFix {
            city: None,
            ..ashburn
        };
        assert_eq!(assess(&us, ms(20), &origin), None, "country level");
    }

    #[test]
    fn stretch_counts_from_the_origins_rtt_and_needs_some_distance() {
        let ms = |n| Some(Duration::from_millis(n));
        let origin = Origin {
            lat: 52.37,
            lon: 4.89,
            slack_km: 0.0,
            hop: Some(2),
            rtt: Duration::from_millis(5),
        };
        let london = city("London", "GB", 51.51, -0.13);
        let stretch = |rtt| assess(&london, rtt, &origin).unwrap().stretch;
        // 357 km takes 3.57 ms of round trip in fibre; 15 - 5 ms is 2.8 times that
        assert!(
            (stretch(ms(15)).unwrap() - 2.8).abs() < 0.05,
            "{:?}",
            stretch(ms(15))
        );
        assert_eq!(stretch(ms(5)), None, "no faster than the origin");
        assert_eq!(stretch(None), None);
        let haarlem = city("Haarlem", "NL", 52.38, 4.64);
        assert_eq!(
            assess(&haarlem, ms(15), &origin).unwrap().stretch,
            None,
            "17 km"
        );
    }

    #[test]
    fn inflation_compares_the_path_with_the_great_circle() {
        let origin = Origin {
            lat: 52.37,
            lon: 4.89,
            slack_km: 0.0,
            hop: None,
            rtt: Duration::ZERO,
        };
        let (london, paris, frankfurt) = ((51.51, -0.13), (48.86, 2.35), (50.11, 8.68));
        let detour = inflation(&origin, &[(4, london), (7, paris)]).unwrap();
        assert_eq!(detour.to_hop, 7);
        assert!((detour.direct_km - 430.0).abs() < 10.0, "{detour:?}");
        assert!(
            (detour.path_km - (357.0 + 344.0)).abs() < 10.0,
            "{detour:?}"
        );
        assert!((detour.ratio() - 1.63).abs() < 0.05);
        assert_eq!(inflation(&origin, &[(7, paris)]), None, "nothing between");
        assert_eq!(inflation(&origin, &[]), None);
        let back_home = inflation(&origin, &[(3, frankfurt), (5, (52.30, 4.95))]);
        assert_eq!(back_home, None, "too near to tell");
    }

    #[test]
    fn own_databases_come_first_newest_first() {
        let dir = env::temp_dir().join(format!("mtr-ng-geo-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "dbip-city-lite-2026-09.mmdb",
            "dbip-city-lite-2026-10.mmdb",
            "notes.txt",
        ] {
            fs::write(dir.join(name), b"").unwrap();
        }
        let found = candidates(Some(&dir));
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found[0], dir.join("dbip-city-lite-2026-10.mmdb"));
        assert_eq!(found[1], dir.join("dbip-city-lite-2026-09.mmdb"));
        assert_eq!(found[2], Path::new("/usr/share/GeoIP/dbip-city-lite.mmdb"));
        assert!(!found.iter().any(|p| p.ends_with("notes.txt")));
    }
}
