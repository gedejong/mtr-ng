//! `--privacy-hops N`: the first N hops are shown without their addresses, names,
//! ASes or locations, so that a screenshot or a pasted report doesn't give away
//! the network it was taken from. They are never looked up either.
//!
//! What is shown is a redacted copy of the session (`MtrSession::redact`), in
//! which every hidden address is replaced by a stand-in. The stand-ins come from
//! ranges no reply can come from, so `show` can tell them apart without knowing
//! the hop, and a place that prints an address without asking `show` prints a
//! stand-in rather than the real one.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// What a hidden address, name or host is shown as.
pub const HIDDEN: &str = "(hidden)";

/// An address as it should be shown: [`HIDDEN`] for a stand-in.
pub fn show(addr: IpAddr) -> String {
    if is_stand_in(addr) {
        HIDDEN.to_string()
    } else {
        addr.to_string()
    }
}

/// True for the addresses that replace hidden ones: 0.0.0.0/8 ("this network",
/// never a source on the wire) and the IPv6 discard prefix 100::/64 (RFC 6666).
pub fn is_stand_in(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.octets()[0] == 0,
        IpAddr::V6(v6) => v6.segments()[..4] == [0x100, 0, 0, 0],
    }
}

/// Stand-ins for hidden addresses, numbered in the order they are first asked
/// for, so the same address gets the same stand-in at every hop.
#[derive(Default)]
pub struct StandIns {
    given: HashMap<IpAddr, IpAddr>,
}

impl StandIns {
    pub fn get(&mut self, addr: IpAddr) -> IpAddr {
        if is_stand_in(addr) {
            return addr;
        }
        let n = self.given.len() as u32 + 1;
        *self.given.entry(addr).or_insert_with(|| match addr {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::from(n)),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, n as u16)),
        })
    }

    /// The stand-in for `addr` if it was hidden somewhere, else `addr`.
    pub fn replace(&self, addr: IpAddr) -> IpAddr {
        self.given.get(&addr).copied().unwrap_or(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stand_ins_are_shown_as_hidden_and_stay_the_same() {
        let mut stand_ins = StandIns::default();
        let gateway: IpAddr = "192.168.1.1".parse().unwrap();
        let isp: IpAddr = "2001:db8::1".parse().unwrap();
        let a = stand_ins.get(gateway);
        let b = stand_ins.get(isp);
        assert_eq!(a, IpAddr::from([0, 0, 0, 1]));
        assert_eq!(b, "100::2".parse::<IpAddr>().unwrap());
        assert_eq!(
            stand_ins.get(gateway),
            a,
            "the same address, the same stand-in"
        );
        assert_eq!(stand_ins.get(a), a, "a stand-in stands for itself");
        assert_eq!(stand_ins.replace(gateway), a);
        let elsewhere: IpAddr = "1.1.1.1".parse().unwrap();
        assert_eq!(stand_ins.replace(elsewhere), elsewhere);

        assert_eq!(show(a), HIDDEN);
        assert_eq!(show(b), HIDDEN);
        assert_eq!(show(gateway), "192.168.1.1");
        assert_eq!(show("2001:db8::1".parse().unwrap()), "2001:db8::1");
    }
}
