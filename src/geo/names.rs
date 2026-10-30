//! Locations from router names. Backbones name their routers after the city they
//! stand in, each network in codes of its own: Arelion's
//! `adm-b3-link.ip.twelve99.net` and Hurricane Electric's `core1.ams1.he.net` are
//! both in Amsterdam. Databases place router addresses far worse than home ones,
//! often at the owner's head office, so a code in the name is the better guess.
//! Only networks whose codes are known are read: as an airport code, Arelion's
//! `adm` would be in Oklahoma. Zayo's names give an airport and its country,
//! as `ae7.cr1.ord8.us.zip.zayo.com` does for Chicago O'Hare, and the code
//! counts only when the airport is in that country.

use super::name_codes::{AIRPORTS, ARELION, COGENT, HURRICANE, LUMEN};
use super::GeoFix;

/// How far a router may be from the city its name gives: data centres sit in
/// suburbs and nearby towns.
const ACCURACY_KM: u16 = 50;

/// A city code and the city: (code, city, ISO country, latitude, longitude).
pub(super) type Code = (&'static str, &'static str, &'static str, f64, f64);

/// The city code in a name, and the ISO country when the name gives one.
type Site<'a> = (&'a str, Option<&'a str>);

/// Where one network's routers carry their city code, and what the codes mean.
struct Network {
    domains: &'static [&'static str],
    /// The code in a name with the domain taken off. When the name gives a
    /// country too, a code whose city is in another country says nothing.
    code: fn(&str) -> Option<Site<'_>>,
    codes: &'static [Code],
    /// Codes to try when `codes` lacks one
    fallback: &'static [Code],
}

const NETWORKS: [Network; 5] = [
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
    // ae7.cr1.ord8.us.zip.zayo.com, and eth.zayo.com on its Ethernet network.
    // Zayo's looking glass lists no codes, but most of its sites are named
    // after an airport, with the ISO country after it (uk for GB). Codes that
    // are no large airport, such as mil for Milan or St. George's sgu, say
    // nothing, and fmm in the US isn't Memmingen.
    Network {
        domains: &["zip.zayo.com", "eth.zayo.com"],
        code: before_country,
        codes: AIRPORTS,
        fallback: &[],
    },
];

/// `adm` of `adm-b3-link`
fn first_word(name: &str) -> Option<Site<'_>> {
    Some((name.split(['-', '.']).next()?, None))
}

/// `ams` of `core1.ams1`: the last label without its number
fn last_label(name: &str) -> Option<Site<'_>> {
    Some((without_number(name.rsplit('.').next()?), None))
}

/// `ord` and `us` of `ae7.cr1.ord8.us`: the label before a two-letter country,
/// without its number, and the country, where `uk` is ISO's GB
fn before_country(name: &str) -> Option<Site<'_>> {
    let (rest, country) = name.rsplit_once('.')?;
    if country.len() != 2 || !country.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    let country = if country == "uk" { "gb" } else { country };
    Some((without_number(rest.rsplit('.').next()?), Some(country)))
}

/// `ams` of `ams1`
fn without_number(label: &str) -> &str {
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
        let (code, named_country) = (network.code)(rest)?;
        let mut codes = network.codes.iter().chain(network.fallback);
        let &(code, city, country, lat, lon) = codes.find(|c| {
            c.0 == code && named_country.is_none_or(|named| c.2.eq_ignore_ascii_case(named))
        })?;
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
    fn zayo_names_give_an_airport_in_their_country() {
        // DB-IP has Amsterdam or San Jose for Zayo's routers in Chicago
        assert_eq!(
            place("ae7.cr1.ord8.us.zip.zayo.com").as_deref(),
            Some("Chicago, US")
        );
        assert_eq!(
            place("lo0.ter1.ams19.nl.zip.zayo.com").as_deref(),
            Some("Amsterdam, NL")
        );
        assert_eq!(
            place("ae17.cr1.fra6.de.zip.zayo.com").as_deref(),
            Some("Frankfurt am Main, DE")
        );
        assert_eq!(
            place("ae10.cr1.man7.uk.eth.zayo.com").as_deref(),
            Some("Manchester, GB"),
            "uk is GB"
        );
        for name in [
            "lo0.mpr1.fmm1.us.zip.zayo.com", // FMM is Memmingen, in Germany
            "ae7.cr1.ord8.de.zip.zayo.com",  // O'Hare isn't in Germany
            "ae1.er1.mil5.it.zip.zayo.com",  // Milan's city code, no airport
            "lo0.mpr1.sgu1.us.zip.zayo.com", // too small an airport
            "lo0.er1.sea25.zip.zayo.com",    // no country
            "ae15.er1-msp1-us.zip.zayo.com", // a shape not read
            "ae10.mpr1.sbn1.uszip.zayo.com", // a typo
            // A customer link, and a name with its domain twice
            "203.0.113.1.ipyx-123456-zyo.zip.zayo.com",
            "1-1-c14.ter1.slc2.us.zip.zayo.com.zip.zayo.com",
            "www.zayo.com",
        ] {
            assert_eq!(locate(name), None, "{name}");
        }
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
