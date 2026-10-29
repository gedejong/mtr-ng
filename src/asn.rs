//! Origin AS lookups through Team Cymru's IP-to-ASN DNS service
//! (<https://www.team-cymru.com/ip-asn-mapping>), as `mtr -z` does, with the
//! announced prefix and the AS's name.

use hickory_resolver::{proto::rr::RData, TokioResolver};
use std::fmt::Write;
use std::net::IpAddr;

/// Addresses worth asking about. Private, shared (CGNAT), loopback, link-local and
/// documentation ranges have no public origin AS, and asking would leak the
/// layout of the local network.
pub fn is_public(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            let shared = a == 100 && (64..128).contains(&b);
            !(v4.is_private()
                || shared
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast())
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast())
        }
    }
}

/// `1.2.3.4` becomes `4.3.2.1.origin.asn.cymru.com.`; IPv6 uses reversed nibbles
/// under `origin6`.
fn query_name(addr: IpAddr) -> String {
    match addr {
        IpAddr::V4(v4) => {
            let [a, b, c, d] = v4.octets();
            format!("{d}.{c}.{b}.{a}.origin.asn.cymru.com.")
        }
        IpAddr::V6(v6) => {
            let mut name = String::new();
            for byte in v6.octets().iter().rev() {
                let _ = write!(name, "{:x}.{:x}.", byte & 0xf, byte >> 4);
            }
            name + "origin6.asn.cymru.com."
        }
    }
}

/// What Team Cymru knows about the network an address is in.
#[derive(Debug, Clone, PartialEq)]
pub struct AsInfo {
    /// The origin AS
    pub number: u32,
    /// The most specific announced prefix the address is in, as `1.1.1.0/24`
    pub prefix: Option<String>,
    /// The AS's registered name, as `CLOUDFLARENET - Cloudflare, Inc., US`
    pub name: Option<String>,
}

impl AsInfo {
    /// The name's first word, the network's handle: `CLOUDFLARENET`, `TWELVE99`.
    /// Registries write the rest differently (`GOOGLE - Google LLC, US`, `TWELVE99
    /// Arelion, fka Telia Carrier, SE`), but the handle always comes first.
    pub fn handle(&self) -> Option<&str> {
        let first = self.name.as_deref()?.split_whitespace().next()?;
        Some(first.trim_end_matches(','))
    }
}

/// The first AS and the prefix in an answer like `13335 | 1.1.1.0/24 | AU |
/// apnic | 2011-08-11`. A prefix announced by several ASes lists them all,
/// space-separated.
fn parse_origin(txt: &str) -> Option<AsInfo> {
    let mut fields = txt.split('|').map(str::trim);
    let number = fields.next()?.split_whitespace().next()?.parse().ok()?;
    let prefix = fields
        .next()
        .filter(|p| p.contains('/'))
        .map(str::to_string);
    Some(AsInfo {
        number,
        prefix,
        name: None,
    })
}

/// The name in an answer like `13335 | US | arin | 2010-07-14 | CLOUDFLARENET -
/// Cloudflare, Inc., US`.
fn parse_name(txt: &str) -> Option<String> {
    let name = txt.split('|').nth(4)?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The first TXT string in the answer to `name` that `parse` accepts.
async fn txt<T>(
    resolver: &TokioResolver,
    name: String,
    parse: impl Fn(&str) -> Option<T>,
) -> Option<T> {
    let answer = resolver.txt_lookup(name).await.ok()?;
    answer
        .answers()
        .iter()
        .find_map(|record| match &record.data {
            RData::TXT(txt) => txt
                .txt_data
                .iter()
                .find_map(|part| parse(&String::from_utf8_lossy(part))),
            _ => None,
        })
}

/// The origin AS and prefix of `addr`; the name comes from `lookup_name`.
pub async fn lookup(resolver: &TokioResolver, addr: IpAddr) -> Option<AsInfo> {
    txt(resolver, query_name(addr), parse_origin).await
}

/// The registered name of AS `number`, asked once per AS.
pub async fn lookup_name(resolver: &TokioResolver, number: u32) -> Option<String> {
    txt(resolver, format!("AS{number}.asn.cymru.com."), parse_name).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_names() {
        assert_eq!(
            query_name("1.2.3.4".parse().unwrap()),
            "4.3.2.1.origin.asn.cymru.com."
        );
        let v6 = query_name("2001:db8::1".parse().unwrap());
        assert!(v6.starts_with("1.0.0.0.0.0.0.0."), "{v6}");
        assert!(
            v6.ends_with(".8.b.d.0.1.0.0.2.origin6.asn.cymru.com."),
            "{v6}"
        );
    }

    #[test]
    fn origin_is_the_first_as_with_its_prefix() {
        let info = parse_origin("13335 | 1.1.1.0/24 | AU | apnic | 2011-08-11").unwrap();
        assert_eq!(info.number, 13335);
        assert_eq!(info.prefix.as_deref(), Some("1.1.1.0/24"));
        assert_eq!(
            parse_origin("6939 1299 | 192.0.2.0/24 | US | arin | ").map(|i| i.number),
            Some(6939)
        );
        assert_eq!(
            parse_origin("15169").map(|i| (i.number, i.prefix)),
            Some((15169, None))
        );
        assert_eq!(parse_origin("NA | 10.0.0.0/8 | | |"), None);
    }

    #[test]
    fn names_and_their_handles() {
        let name =
            parse_name("13335 | US | arin | 2010-07-14 | CLOUDFLARENET - Cloudflare, Inc., US");
        assert_eq!(
            name.as_deref(),
            Some("CLOUDFLARENET - Cloudflare, Inc., US")
        );
        assert_eq!(parse_name("64496 | ZZ | other | | "), None);
        assert_eq!(parse_name("64496"), None);

        let handle = |name: &str| {
            AsInfo {
                number: 1,
                prefix: None,
                name: Some(name.to_string()),
            }
            .handle()
            .map(str::to_string)
        };
        assert_eq!(handle("GOOGLE - Google LLC, US").as_deref(), Some("GOOGLE"));
        assert_eq!(
            handle("TWELVE99 Arelion, fka Telia Carrier, SE").as_deref(),
            Some("TWELVE99")
        );
        assert_eq!(handle("ZAYO-6461, US").as_deref(), Some("ZAYO-6461"));
    }

    #[test]
    fn private_and_special_addresses_stay_home() {
        for addr in [
            "192.168.1.1",
            "10.1.2.3",
            "172.16.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "192.0.2.7",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(!is_public(addr.parse().unwrap()), "{addr}");
        }
        for addr in [
            "1.1.1.1",
            "100.128.0.1",
            "195.190.228.107",
            "2606:4700::1111",
        ] {
            assert!(is_public(addr.parse().unwrap()), "{addr}");
        }
    }
}
