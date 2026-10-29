//! ICMP extensions (RFC 4884) on Time Exceeded and Destination Unreachable.
//! mtr-ng reads two objects: the MPLS label stack (RFC 4950), with which a router
//! inside an MPLS tunnel reports the labels our probe carried when its TTL ran
//! out, and interface information (RFC 5837), with which a router names the
//! interface the probe came in on (or would have left by): its ifIndex, address,
//! name and MTU. That address can differ from the one the reply comes from, and
//! it is the only one an unnumbered interface has.

use crate::probe::calculate_icmp_checksum;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// What a router appended to an ICMP error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    pub mpls: Vec<MplsLabel>,
    pub interfaces: Vec<Interface>,
}

impl Extensions {
    /// Just one MPLS label, for tests
    #[cfg(test)]
    pub fn mpls_only(label: MplsLabel) -> Self {
        Self {
            mpls: vec![label],
            interfaces: Vec::new(),
        }
    }
}

/// Which interface an interface information object describes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The one the probe arrived on
    Incoming,
    /// A sub-IP component of that one, such as a member of a link bundle
    SubIp,
    /// The one it would have left by
    Outgoing,
    /// The next hop it would have gone to
    NextHop,
}

/// One interface information object (RFC 5837). Each part is optional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interface {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<IpAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
}

/// `[Interface in: et-0/0/2.100 203.0.113.6 ifIndex 531 MTU 9192]`
impl fmt::Display for Interface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let role = match self.role {
            Role::Incoming => "in",
            Role::SubIp => "in, below IP",
            Role::Outgoing => "out",
            Role::NextHop => "next hop",
        };
        let mut parts = Vec::new();
        parts.extend(self.name.clone());
        parts.extend(self.addr.map(|addr| addr.to_string()));
        parts.extend(self.index.map(|index| format!("ifIndex {index}")));
        parts.extend(self.mtu.map(|mtu| format!("MTU {mtu}")));
        match parts.is_empty() {
            true => write!(f, "[Interface {role}]"),
            false => write!(f, "[Interface {role}: {}]", parts.join(" ")),
        }
    }
}

/// One MPLS label stack entry, as quoted by the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MplsLabel {
    pub label: u32,
    /// Traffic class (formerly EXP)
    pub tc: u8,
    /// Bottom of stack
    pub bottom: bool,
    pub ttl: u8,
}

/// As mtr shows it: `[MPLS: Lbl 24005 TC 0 S 1 TTL 1]`
impl fmt::Display for MplsLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[MPLS: Lbl {} TC {} S {} TTL {}]",
            self.label,
            self.tc,
            u8::from(self.bottom),
            self.ttl
        )
    }
}

/// Where the quoted datagram ends when a router appends extensions without setting
/// the length attribute, as many did before RFC 4884 (its section 5).
const COMPAT_OFFSET: usize = 128;

/// The extensions of an ICMPv4 error. `icmp` starts at the ICMP type byte.
pub fn extensions(icmp: &[u8]) -> Extensions {
    // ICMPv4 puts the length of the quoted datagram, in 32-bit words, in byte 5.
    extension_objects(icmp, 5, 4)
        .map(read_objects)
        .unwrap_or_default()
}

/// The extensions of an ICMPv6 error. `icmp` starts at the ICMPv6 type byte.
pub fn extensions_v6(icmp: &[u8]) -> Extensions {
    // ICMPv6 puts it in byte 4, in 64-bit words.
    extension_objects(icmp, 4, 8)
        .map(read_objects)
        .unwrap_or_default()
}

/// The objects of the extension structure, if the message carries a valid one.
/// The quoted datagram's length is in byte `length_at`, counted in `unit` bytes.
fn extension_objects(icmp: &[u8], length_at: usize, unit: usize) -> Option<&[u8]> {
    let body = icmp.get(8..)?;
    let start = match usize::from(*icmp.get(length_at)?) {
        0 => COMPAT_OFFSET,
        words => words * unit,
    };
    let ext = body.get(start..)?;
    // Version 2 in the top nibble, then a checksum over the whole structure; zero
    // means none was sent.
    if ext.len() < 4 || ext[0] >> 4 != 2 {
        return None;
    }
    let checksum = u16::from_be_bytes([ext[2], ext[3]]);
    if checksum != 0 && calculate_icmp_checksum(ext) != 0 {
        return None;
    }
    Some(&ext[4..])
}

/// The MPLS label stacks (class 1, C-type 1) and interface information objects
/// (class 2) among `objects`. Others are skipped.
fn read_objects(mut objects: &[u8]) -> Extensions {
    let mut found = Extensions::default();
    while objects.len() >= 4 {
        let len = usize::from(u16::from_be_bytes([objects[0], objects[1]]));
        if len < 4 || len > objects.len() {
            break;
        }
        let payload = &objects[4..len];
        match (objects[2], objects[3]) {
            (1, 1) => {
                for &entry in payload.as_chunks::<4>().0 {
                    let word = u32::from_be_bytes(entry);
                    found.mpls.push(MplsLabel {
                        label: word >> 12,
                        tc: ((word >> 9) & 0x7) as u8,
                        bottom: word & 0x100 != 0,
                        ttl: word as u8,
                    });
                }
            }
            (2, c_type) => found.interfaces.extend(interface(c_type, payload)),
            _ => {}
        }
        objects = &objects[len..];
    }
    found
}

/// The next `n` bytes of `rest`, which then starts after them.
fn take<'a>(rest: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    let (head, tail) = rest.split_at_checked(n)?;
    *rest = tail;
    Some(head)
}

/// An interface information object, whose C-type says which parts follow, in
/// this order: ifIndex, address, name, MTU. One that doesn't add up is dropped.
fn interface(c_type: u8, mut rest: &[u8]) -> Option<Interface> {
    let word = |bytes: &[u8]| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let role = match c_type >> 6 {
        0 => Role::Incoming,
        1 => Role::SubIp,
        2 => Role::Outgoing,
        _ => Role::NextHop,
    };
    let index = match c_type & 0x08 {
        0 => None,
        _ => Some(word(take(&mut rest, 4)?)),
    };
    let addr = match c_type & 0x04 {
        0 => None,
        // Address family (1 IPv4, 2 IPv6) and two reserved bytes
        _ => match take(&mut rest, 4)?[..2] {
            [0, 1] => Some(IpAddr::V4(Ipv4Addr::from(
                <[u8; 4]>::try_from(take(&mut rest, 4)?).ok()?,
            ))),
            [0, 2] => Some(IpAddr::V6(Ipv6Addr::from(
                <[u8; 16]>::try_from(take(&mut rest, 16)?).ok()?,
            ))),
            _ => return None,
        },
    };
    let name = match c_type & 0x02 {
        0 => None,
        _ => {
            // Its length counts itself, and is a multiple of 4 up to 64
            let len = usize::from(*rest.first()?);
            if len < 4 || len % 4 != 0 || len > 64 {
                return None;
            }
            Some(printable(&take(&mut rest, len)?[1..]))
        }
    };
    let mtu = match c_type & 0x01 {
        0 => None,
        _ => Some(word(take(&mut rest, 4)?)),
    };
    rest.is_empty().then_some(Interface {
        role,
        index,
        addr,
        name,
        mtu,
    })
}

/// A name as UTF-8 without its NUL padding, and with control characters, which
/// could steer the terminal, replaced.
fn printable(bytes: &[u8]) -> String {
    let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    String::from_utf8_lossy(&bytes[..end])
        .chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mpls_labels(icmp: &[u8]) -> Vec<MplsLabel> {
        extensions(icmp).mpls
    }

    fn mpls_labels_v6(icmp: &[u8]) -> Vec<MplsLabel> {
        extensions_v6(icmp).mpls
    }

    fn entry(label: u32, tc: u8, bottom: bool, ttl: u8) -> [u8; 4] {
        let word = label << 12 | u32::from(tc) << 9 | u32::from(bottom) << 8 | u32::from(ttl);
        word.to_be_bytes()
    }

    /// An extension structure holding one object per (class, c-type, payload).
    fn extension(objects: &[(u8, u8, Vec<u8>)], with_checksum: bool) -> Vec<u8> {
        let mut ext = vec![0x20, 0, 0, 0];
        for (class, ctype, payload) in objects {
            ext.extend_from_slice(&(4 + payload.len() as u16).to_be_bytes());
            ext.extend_from_slice(&[*class, *ctype]);
            ext.extend_from_slice(payload);
        }
        if with_checksum {
            let checksum = calculate_icmp_checksum(&ext);
            ext[2..4].copy_from_slice(&checksum.to_be_bytes());
        }
        ext
    }

    /// Time Exceeded with a 128-byte quoted datagram, then `ext`. `length` sets
    /// the RFC 4884 length attribute; 0 is the pre-RFC layout.
    fn time_exceeded(length: u8, ext: &[u8]) -> Vec<u8> {
        let mut icmp = vec![11, 0, 0, 0, 0, length, 0, 0];
        icmp.extend(vec![0x45; 128]);
        icmp.extend_from_slice(ext);
        icmp
    }

    fn two_labels() -> Vec<u8> {
        [entry(24005, 0, false, 1), entry(16, 5, true, 254)].concat()
    }

    #[test]
    fn reads_a_label_stack_with_and_without_the_length_attribute() {
        let expected = vec![
            MplsLabel {
                label: 24005,
                tc: 0,
                bottom: false,
                ttl: 1,
            },
            MplsLabel {
                label: 16,
                tc: 5,
                bottom: true,
                ttl: 254,
            },
        ];
        let ext = extension(&[(1, 1, two_labels())], true);
        assert_eq!(mpls_labels(&time_exceeded(32, &ext)), expected, "RFC 4884");
        assert_eq!(mpls_labels(&time_exceeded(0, &ext)), expected, "pre-RFC");
    }

    #[test]
    fn skips_other_objects_and_accepts_a_missing_checksum() {
        let interface_info = (2, 0x0b, vec![0; 8]);
        let ext = extension(&[interface_info, (1, 1, two_labels())], false);
        assert_eq!(mpls_labels(&time_exceeded(32, &ext)).len(), 2);
    }

    #[test]
    fn ignores_bad_checksums_versions_and_lengths() {
        let mut ext = extension(&[(1, 1, two_labels())], true);
        ext[5] ^= 0xff;
        assert!(mpls_labels(&time_exceeded(32, &ext)).is_empty(), "checksum");

        let mut ext = extension(&[(1, 1, two_labels())], false);
        ext[0] = 0x10;
        assert!(mpls_labels(&time_exceeded(32, &ext)).is_empty(), "version");

        let mut ext = extension(&[(1, 1, two_labels())], false);
        ext[4..6].copy_from_slice(&200u16.to_be_bytes());
        assert!(mpls_labels(&time_exceeded(32, &ext)).is_empty(), "length");
    }

    #[test]
    fn icmpv6_counts_the_quoted_length_in_64_bit_words() {
        let ext = extension(&[(1, 1, two_labels())], true);
        let mut icmp = vec![3, 0, 0, 0, 16, 0, 0, 0]; // 16 words: 128 bytes quoted
        icmp.extend(vec![0x60; 128]);
        icmp.extend_from_slice(&ext);
        assert_eq!(mpls_labels_v6(&icmp).len(), 2, "RFC 4884");
        icmp[4] = 0;
        assert_eq!(mpls_labels_v6(&icmp).len(), 2, "pre-RFC");
        icmp[4] = 15;
        assert!(mpls_labels_v6(&icmp).is_empty(), "wrong length");
    }

    #[test]
    fn plain_errors_and_truncated_packets_have_no_labels() {
        let mut short = vec![11, 0, 0, 0, 0, 0, 0, 0];
        short.extend(vec![0x45; 28]);
        assert!(mpls_labels(&short).is_empty());

        let full = time_exceeded(32, &extension(&[(1, 1, two_labels())], true));
        for len in 0..full.len() {
            assert!(mpls_labels(&full[..len]).is_empty(), "length {len}");
        }
    }

    /// Incoming interface information as Linux (6.19 and later, with
    /// net.ipv4.icmp_errors_extension_mask=1) appends it: ifIndex, the IPv4
    /// address sub-object, a 20-byte name sub-object (IFNAMSIZ plus the length
    /// byte, rounded up to 4) and the MTU, after a quoted datagram padded to 128.
    fn linux_interface_object(name: &str) -> Vec<u8> {
        let mut payload = 7u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&[0, 1, 0, 0, 192, 0, 2, 1]);
        let mut subobject = vec![0; 20];
        subobject[0] = 20;
        subobject[1..1 + name.len()].copy_from_slice(name.as_bytes());
        payload.extend(subobject);
        payload.extend_from_slice(&1500u32.to_be_bytes());
        payload
    }

    #[test]
    fn reads_interface_information_as_linux_sends_it() {
        let ext = extension(&[(2, 0x0f, linux_interface_object("veth1"))], true);
        let found = extensions(&time_exceeded(32, &ext));
        let interface = Interface {
            role: Role::Incoming,
            index: Some(7),
            addr: Some("192.0.2.1".parse().unwrap()),
            name: Some("veth1".to_string()),
            mtu: Some(1500),
        };
        assert_eq!(found.interfaces, std::slice::from_ref(&interface));
        assert!(found.mpls.is_empty());
        assert_eq!(
            interface.to_string(),
            "[Interface in: veth1 192.0.2.1 ifIndex 7 MTU 1500]"
        );
    }

    #[test]
    fn reads_every_role_and_any_subset_of_parts() {
        let v6: Vec<u8> = [&[0, 2, 0, 0][..], &[0x20, 0x01, 0x0d, 0xb8], &[0; 11], &[1]].concat();
        let objects = [
            (2, 0x00, vec![]),                                // incoming, nothing but the role
            (2, 0x40 | 0x01, 9000u32.to_be_bytes().to_vec()), // sub-IP, MTU
            (2, 0x80 | 0x04, v6),                             // outgoing, an IPv6 address
            (2, 0xc0 | 0x02, [&[8][..], b"ae1\0\0\0\0"].concat()), // next hop, a name
            (1, 1, two_labels()),
        ];
        let ext = extension(&objects, true);
        let found = extensions(&time_exceeded(32, &ext));
        assert_eq!(found.mpls.len(), 2, "labels next to interfaces");
        let shown: Vec<String> = found.interfaces.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "[Interface in]",
                "[Interface in, below IP: MTU 9000]",
                "[Interface out: 2001:db8::1]",
                "[Interface next hop: ae1]",
            ]
        );
    }

    #[test]
    fn interface_objects_that_dont_add_up_are_dropped() {
        let good = linux_interface_object("eth0");
        let cases: [(&str, u8, Vec<u8>); 6] = [
            ("a part missing", 0x0f, good[..good.len() - 4].to_vec()),
            ("a part too many", 0x0e, good.clone()),
            ("unknown family", 0x04, vec![0, 3, 0, 0, 1, 2, 3, 4]),
            // scapy counts only the name, and pads nothing
            (
                "name length without itself",
                0x02,
                [&[3][..], b"hey"].concat(),
            ),
            (
                "name longer than 64",
                0x02,
                [vec![68], vec![b'x'; 67]].concat(),
            ),
            ("name past the end", 0x02, vec![8, b'x', b'y', b'z']),
        ];
        for (what, c_type, payload) in cases {
            let ext = extension(&[(2, c_type, payload), (1, 1, two_labels())], false);
            let found = extensions(&time_exceeded(32, &ext));
            assert!(found.interfaces.is_empty(), "{what}");
            assert_eq!(found.mpls.len(), 2, "{what}: the next object still counts");
        }
    }

    #[test]
    fn interface_names_cannot_steer_the_terminal() {
        let mut name = vec![12, 0x1b];
        name.extend_from_slice(b"[2Jeth\0\0\0\0");
        let ext = extension(&[(2, 0x02, name)], true);
        let found = extensions_v6(&{
            let mut icmp = vec![3, 0, 0, 0, 16, 0, 0, 0];
            icmp.extend(vec![0x60; 128]);
            icmp.extend_from_slice(&ext);
            icmp
        });
        assert_eq!(found.interfaces[0].name.as_deref(), Some("\u{fffd}[2Jeth"));
    }
}
