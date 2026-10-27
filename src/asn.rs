//! Origin AS lookups through Team Cymru's IP-to-ASN DNS service
//! (<https://www.team-cymru.com/ip-asn-mapping>), as `mtr -z` does.

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

/// The first AS in an answer like `13335 | 1.1.1.0/24 | AU | apnic | 2011-08-11`.
/// A prefix announced by several ASes lists them all, space-separated.
fn parse_origin(txt: &str) -> Option<u32> {
    txt.split('|')
        .next()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

pub async fn lookup(resolver: &TokioResolver, addr: IpAddr) -> Option<u32> {
    let answer = resolver.txt_lookup(query_name(addr)).await.ok()?;
    answer
        .answers()
        .iter()
        .find_map(|record| match &record.data {
            RData::TXT(txt) => txt
                .txt_data
                .iter()
                .find_map(|part| parse_origin(&String::from_utf8_lossy(part))),
            _ => None,
        })
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
    fn origin_is_the_first_as() {
        assert_eq!(
            parse_origin("13335 | 1.1.1.0/24 | AU | apnic | 2011-08-11"),
            Some(13335)
        );
        assert_eq!(
            parse_origin("6939 1299 | 192.0.2.0/24 | US | arin | "),
            Some(6939)
        );
        assert_eq!(parse_origin("NA | 10.0.0.0/8 | | |"), None);
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
