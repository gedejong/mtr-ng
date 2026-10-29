//! ICMP extensions (RFC 4884) on Time Exceeded and Destination Unreachable.
//! mtr-ng reads the MPLS label stack object (RFC 4950): a router inside an MPLS
//! tunnel reports the labels our probe carried when its TTL ran out.

use crate::probe::calculate_icmp_checksum;
use std::fmt;

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

/// MPLS labels in an ICMPv4 error. `icmp` starts at the ICMP type byte.
pub fn mpls_labels(icmp: &[u8]) -> Vec<MplsLabel> {
    // ICMPv4 puts the length of the quoted datagram, in 32-bit words, in byte 5.
    extension_objects(icmp, 5, 4)
        .map(labels)
        .unwrap_or_default()
}

/// MPLS labels in an ICMPv6 error. `icmp` starts at the ICMPv6 type byte.
pub fn mpls_labels_v6(icmp: &[u8]) -> Vec<MplsLabel> {
    // ICMPv6 puts it in byte 4, in 64-bit words.
    extension_objects(icmp, 4, 8)
        .map(labels)
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

/// Label stack entries from the MPLS objects (class 1, C-type 1) among `objects`.
fn labels(mut objects: &[u8]) -> Vec<MplsLabel> {
    let mut labels = Vec::new();
    while objects.len() >= 4 {
        let len = usize::from(u16::from_be_bytes([objects[0], objects[1]]));
        if len < 4 || len > objects.len() {
            break;
        }
        if (objects[2], objects[3]) == (1, 1) {
            for &entry in objects[4..len].as_chunks::<4>().0 {
                let word = u32::from_be_bytes(entry);
                labels.push(MplsLabel {
                    label: word >> 12,
                    tc: ((word >> 9) & 0x7) as u8,
                    bottom: word & 0x100 != 0,
                    ttl: word as u8,
                });
            }
        }
        objects = &objects[len..];
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
