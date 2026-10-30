//! Locations from router names. Backbones name their routers after the city they
//! stand in, each network in codes of its own: Arelion's
//! `adm-b3-link.ip.twelve99.net` and Hurricane Electric's `core1.ams1.he.net` are
//! both in Amsterdam. Databases place router addresses far worse than home ones,
//! often at the owner's head office, so a code in the name is the better guess.
//! Only networks whose codes are known are read: as an airport code, Arelion's
//! `adm` would be in Oklahoma.

use super::name_codes::{AIRPORTS, ARELION, COGENT, HURRICANE, LUMEN};
use super::GeoFix;

/// How far a router may be from the city its name gives: data centres sit in
/// suburbs and nearby towns.
const ACCURACY_KM: u16 = 50;

/// A city code and the city: (code, city, ISO country, latitude, longitude).
pub(super) type Code = (&'static str, &'static str, &'static str, f64, f64);

/// Where one network's routers carry their city code, and what the codes mean.
struct Network {
    domains: &'static [&'static str],
    /// The code in a name with the domain taken off
    code: fn(&str) -> &str,
    codes: &'static [Code],
    /// Codes to try when `codes` lacks one
    fallback: &'static [Code],
}

const NETWORKS: [Network; 4] = [
    // adm-b3-link.ip.twelve99.net, and before 2021 adm-bb3-link.telia.net
    Network {
        domains: &["ip.twelve99.net", "telia.net"],
        code: first_word,
        codes: ARELION,
        fallback: &[],
    },
    // 100ge0-34.core1.ams1.he.net
    Network {
        domains: &["he.net"],
        code: last_label,
        codes: HURRICANE,
        fallback: &[],
    },
    // be2434.ccr41.ams03.atlas.cogentco.com. Every code its looking glass lists
    // is its city's airport or metro code, so sites it leaves out, such as
    // Frankfurt's fra05, are read as airports.
    Network {
        domains: &["atlas.cogentco.com"],
        code: last_label,
        codes: COGENT,
        fallback: AIRPORTS,
    },
    // ae-1-3501.ear1.Amsterdam1.Level3.net
    Network {
        domains: &["level3.net"],
        code: last_label,
        codes: LUMEN,
        fallback: &[],
    },
];

/// `adm` of `adm-b3-link`
fn first_word(name: &str) -> &str {
    name.split(['-', '.']).next().unwrap_or_default()
}

/// `ams` of `core1.ams1`: the last label without its number
fn last_label(name: &str) -> &str {
    let label = name.rsplit('.').next().unwrap_or_default();
    label.trim_end_matches(|c: char| c.is_ascii_digit())
}

/// Where a router's name says it is, when its network's codes are known.
pub fn locate(hostname: &str) -> Option<GeoFix> {
    let name = hostname.trim_end_matches('.').to_ascii_lowercase();
    NETWORKS.iter().find_map(|network| {
        let rest = network
            .domains
            .iter()
            .find_map(|domain| name.strip_suffix(domain)?.strip_suffix('.'))?;
        let code = (network.code)(rest);
        let mut codes = network.codes.iter().chain(network.fallback);
        let &(code, city, country, lat, lon) = codes.find(|c| c.0 == code)?;
        Some(GeoFix {
            lat,
            lon,
            accuracy_km: Some(ACCURACY_KM),
            city: Some(city.to_string()),
            country: Some(country.to_string()),
            from_name: Some(code.to_string()),
            from_ipmap: None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(hostname: &str) -> Option<String> {
        locate(hostname).map(|fix| fix.place())
    }

    #[test]
    fn backbone_names_give_their_city() {
        // A trace to 91.160.93.4, where DB-IP has Miami, Denver and Singapore
        assert_eq!(
            place("adm-b3-link.ip.twelve99.net").as_deref(),
            Some("Amsterdam, NL")
        );
        assert_eq!(
            place("prs-bb1-link.ip.twelve99.net.").as_deref(),
            Some("Paris, FR")
        );
        assert_eq!(
            place("adm-bb3-link.telia.net").as_deref(),
            Some("Amsterdam, NL")
        );
        assert_eq!(
            place("100ge0-34.core1.ams1.he.net").as_deref(),
            Some("Amsterdam, NL")
        );
        assert_eq!(
            place("be2434.ccr41.ams03.atlas.cogentco.com").as_deref(),
            Some("Amsterdam, NL")
        );
        assert_eq!(
            place("ae-1-3501.ear1.Amsterdam1.Level3.net").as_deref(),
            Some("Amsterdam, NL")
        );
        let fix = locate("core1.ams1.he.net").unwrap();
        assert_eq!(
            (fix.from_name.as_deref(), fix.accuracy_km),
            (Some("ams"), Some(50))
        );
    }

    #[test]
    fn each_network_reads_its_own_codes() {
        assert_eq!(
            place("san-b2-link.ip.twelve99.net").as_deref(),
            Some("San Antonio, US")
        );
        assert_eq!(
            place("be1.ccr21.san01.atlas.cogentco.com").as_deref(),
            Some("San Diego, US")
        );
        assert_eq!(
            place("core1.lyn1.he.net").as_deref(),
            Some("Lyon, FR"),
            "Lyon's airport is LYS"
        );
        assert_eq!(
            place("be3343.ccr41.fra05.atlas.cogentco.com").as_deref(),
            Some("Frankfurt am Main, DE"),
            "not in Cogent's looking glass: its airport"
        );
        assert_eq!(
            place("core1.fra1.he.net").as_deref(),
            Some("Frankfurt, DE"),
            "Hurricane Electric's own list"
        );
        assert_eq!(place("core1.mxp1.he.net"), None, "no airports for it");
    }

    #[test]
    fn other_names_say_nothing() {
        for name in [
            "freesas-ic-387365.ip.twelve99-cust.net", // a customer link
            "s-b5-link.ip.twelve99.net",              // a code the table doesn't have
            "core1.ams1.the.net",                     // not he.net
            "tserv1.he.net",
            "ae1.ams1.example.net",
            "Level3.net",
            "",
        ] {
            assert_eq!(locate(name), None, "{name}");
        }
    }

    #[test]
    fn code_tables_are_sound() {
        let tables = NETWORKS.iter().flat_map(|n| [n.codes, n.fallback]);
        for codes in tables {
            for (i, &(code, city, country, lat, lon)) in codes.iter().enumerate() {
                assert!(!codes[..i].iter().any(|c| c.0 == code), "{code} twice");
                assert!(
                    code.chars().all(|c| c.is_ascii_lowercase()),
                    "{code}: names are matched in lower case"
                );
                assert!(!city.is_empty() && country.len() == 2, "{code}");
                assert!(lat.abs() <= 90.0 && lon.abs() <= 180.0, "{code}");
            }
        }
    }
}
