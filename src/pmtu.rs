//! Path MTU discovery (`--pmtu`).
//!
//! Each round one extra probe goes to the end of the path, as large as the path is
//! thought to allow and with Don't Fragment set. A router whose next link is
//! narrower drops it and answers Fragmentation Needed (IPv4) or Packet Too Big
//! (IPv6) with that link's MTU; the probes shrink to it, until one arrives. A size
//! that vanishes without such an answer, while small probes to the same hop get
//! through, is a black hole for path MTU discovery: the search then halves the gap
//! between the largest size that arrived and the smallest that vanished. Once it
//! knows that size, probes of it go to ever nearer or farther hops, halving the
//! hops between the farthest one they reach and the nearest one they don't, to
//! find where they vanish.

use crate::privacy::show;
use std::net::IpAddr;

/// Common MTUs, largest first: the next step down when a router reports no MTU
/// (as some did before RFC 1191) or this machine refuses a size.
const PLATEAUS: [u16; 13] = [
    9000, 1500, 1492, 1480, 1460, 1440, 1400, 1380, 1280, 1006, 576, 296, 68,
];

/// Losses in a row before a size counts as vanished, so a single lost probe
/// doesn't start a black hole search
const LOSSES_TO_VANISH: u8 = 2;

/// Where the path narrows
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtuDrop {
    /// Hop of the router whose next link is narrower: 0 for this machine, None when
    /// the router never answered a small probe
    pub hop: Option<u8>,
    /// The router that said so; None for this machine, which refused to send
    pub router: Option<IpAddr>,
    /// The narrower link's MTU
    pub mtu: u16,
}

/// Where probes of a black hole's size vanish
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hole {
    /// The smallest size that vanishes
    pub size: u16,
    /// The farthest hop that answered a probe of that size, 0 for none
    pub passed: u8,
    /// The nearest hop where probes of that size vanished
    pub lost_at: u8,
    /// No hop between the two is left to try
    pub found: bool,
    /// The hop being tried, and the losses in a row there
    ttl: u8,
    losses: u8,
}

/// The search for the largest packet that crosses the whole path
#[derive(Debug, Clone)]
pub struct PathMtu {
    /// Smallest MTU the family allows: 68 bytes for IPv4, 1280 for IPv6
    floor: u16,
    /// Size the search started at, the outgoing interface's MTU
    pub start: u16,
    /// Size of the probes now
    size: u16,
    /// Largest size that arrived
    arrived: Option<u16>,
    /// Smallest size that vanished without an answer
    vanished: Option<u16>,
    /// Losses in a row at `size`
    losses: u8,
    /// TTL of the hop that answered the largest probe
    reached: Option<u8>,
    done: bool,
    /// Where the path narrows, in the order the routers said so
    pub drops: Vec<MtuDrop>,
    /// Where a black hole is, once its size is known
    pub hole: Option<Hole>,
}

impl PathMtu {
    /// A search starting at `start` bytes, the outgoing interface's MTU.
    pub fn new(start: u16, ipv6: bool) -> Self {
        let floor = if ipv6 { 1280 } else { 68 };
        Self {
            floor,
            start: start.max(floor),
            size: start.max(floor),
            arrived: None,
            vanished: None,
            losses: 0,
            reached: None,
            done: false,
            drops: Vec::new(),
            hole: None,
        }
    }

    /// The next probe, TTL and size, if the search wants one: to `end`, the end of
    /// the path, until the path MTU is known, then to the hops a black hole may
    /// be near, of those that answer small probes (`answers`).
    pub fn next_probe(&mut self, end: u8, answers: impl Fn(u8) -> bool) -> Option<(u8, u16)> {
        if !self.done {
            return Some((end, self.size));
        }
        let hole = self.hole.as_mut().filter(|hole| !hole.found)?;
        let middle = (u16::from(hole.passed) + u16::from(hole.lost_at)) / 2;
        let next = (hole.passed + 1..hole.lost_at)
            .filter(|&ttl| answers(ttl))
            .min_by_key(|&ttl| u16::from(ttl).abs_diff(middle));
        let Some(ttl) = next else {
            hole.found = true;
            return None;
        };
        if ttl != hole.ttl {
            hole.ttl = ttl;
            hole.losses = 0;
        }
        Some((ttl, hole.size))
    }

    /// Size of the next probe, IP header included, while the search goes on.
    pub fn probe_size(&self) -> Option<u16> {
        (!self.done).then_some(self.size)
    }

    /// The path MTU, once found
    pub fn mtu(&self) -> Option<u16> {
        self.arrived.filter(|_| self.done)
    }

    /// The smallest size that vanished without an error, once the search is done
    pub fn black_hole(&self) -> Option<u16> {
        self.vanished.filter(|_| self.done)
    }

    /// TTL of the hop that answered the largest probe
    pub fn reached(&self) -> Option<u8> {
        self.reached
    }

    /// `router`, at `hop` if known, could not forward a probe of `size`: its next
    /// link takes `mtu` bytes (0 when it didn't say).
    pub fn too_big(&mut self, size: u16, router: IpAddr, hop: Option<u8>, mtu: u16) {
        if self.done || size != self.size {
            return; // about a size already given up
        }
        let mtu = if (self.floor..size).contains(&mtu) {
            mtu
        } else {
            self.plateau_below(size)
        };
        self.drops.push(MtuDrop {
            hop,
            router: Some(router),
            mtu,
        });
        self.shrink_to(mtu);
    }

    /// This machine refused to send `size` bytes: it has a smaller path MTU cached
    /// for the target, or the route's MTU is smaller than the interface's.
    pub fn refused(&mut self, size: u16) {
        if self.done || size != self.size {
            return;
        }
        let mtu = self.plateau_below(size);
        match self.drops.iter_mut().find(|drop| drop.router.is_none()) {
            Some(drop) => drop.mtu = mtu,
            None => self.drops.insert(
                0,
                MtuDrop {
                    hop: Some(0),
                    router: None,
                    mtu,
                },
            ),
        }
        self.shrink_to(mtu);
    }

    /// A probe of `size` arrived: the hop at `ttl` answered it.
    pub fn arrived(&mut self, size: u16, ttl: u8) {
        if self.done {
            if let Some(hole) = self.hole_probed(size) {
                if ttl < hole.lost_at {
                    hole.passed = hole.passed.max(ttl);
                }
            }
            return;
        }
        if self.arrived.is_some_and(|arrived| arrived >= size) {
            return;
        }
        self.arrived = Some(size);
        self.reached = Some(ttl);
        if self.vanished.is_none() {
            self.done = size == self.size;
        } else {
            self.bisect();
        }
    }

    /// A probe of `size` to `ttl` went unanswered, although the hop there answers
    /// small probes.
    pub fn lost(&mut self, size: u16, ttl: u8) {
        if self.done {
            if let Some(hole) = self.hole_probed(size).filter(|hole| hole.ttl == ttl) {
                hole.losses += 1;
                if hole.losses >= LOSSES_TO_VANISH && ttl > hole.passed {
                    hole.lost_at = hole.lost_at.min(ttl);
                }
            }
            return;
        }
        if size != self.size {
            return;
        }
        self.losses += 1;
        if self.losses >= LOSSES_TO_VANISH {
            self.vanished = Some(self.vanished.map_or(size, |vanished| vanished.min(size)));
            self.bisect();
        }
    }

    /// The black hole being located, if `size` is what it is probed with
    fn hole_probed(&mut self, size: u16) -> Option<&mut Hole> {
        self.hole
            .as_mut()
            .filter(|hole| !hole.found && hole.size == size)
    }

    fn shrink_to(&mut self, mtu: u16) {
        self.size = mtu;
        self.losses = 0;
        if self.arrived.is_some_and(|arrived| arrived > mtu) {
            self.arrived = None; // the path changed
        }
    }

    /// Probe halfway between the largest size that arrived and the smallest that
    /// vanished, or stop when they are adjacent.
    fn bisect(&mut self) {
        let Some(high) = self.vanished else {
            return;
        };
        // Until something arrives, the floor itself is still to be tried.
        let low = self.arrived.unwrap_or(self.floor - 1);
        self.losses = 0;
        if high <= low + 1 {
            self.size = low;
            self.done = true;
            // Probes of `high` reach the end of the path no more: find where they stop.
            self.hole = self.reached.map(|end| Hole {
                size: high,
                passed: 0,
                lost_at: end,
                found: false,
                ttl: 0,
                losses: 0,
            });
        } else {
            self.size = low + (high - low) / 2;
        }
    }

    fn plateau_below(&self, size: u16) -> u16 {
        PLATEAUS
            .into_iter()
            .find(|&mtu| mtu < size)
            .unwrap_or(self.floor)
            .max(self.floor)
    }

    /// A few words for the status line: `PMTU 1492`, or the size being tried.
    pub fn status(&self) -> String {
        let found = self.hole.as_ref().filter(|hole| hole.found);
        match (self.mtu(), self.black_hole()) {
            (Some(mtu), None) => format!("PMTU {mtu}"),
            (Some(mtu), Some(_)) => match found {
                Some(hole) if hole.passed > 0 => {
                    format!("PMTU {mtu}, black hole past hop {}", hole.passed)
                }
                _ => format!("PMTU {mtu}, black hole above"),
            },
            (None, _) if self.done => "PMTU unknown".to_string(),
            (None, _) => format!("PMTU trying {}", self.size),
        }
    }

    /// Sentences for under the report. `target_hop` is the hop the target answered
    /// at, if it did.
    pub fn summary(&self, target_hop: Option<u8>) -> Vec<String> {
        let up_to = match (self.reached, target_hop) {
            (Some(reached), Some(target)) if reached >= target => String::new(),
            (Some(reached), _) => format!(" up to hop {reached}, the last that answered"),
            (None, _) => String::new(),
        };
        let mut lines = vec![match (self.mtu(), self.black_hole()) {
            (Some(mtu), None) => format!("Path MTU {mtu}{up_to}."),
            (Some(mtu), Some(vanished)) => format!(
                "Path MTU {mtu}{up_to}: probes of {vanished} bytes and more vanish without \
                 an ICMP error, a black hole for path MTU discovery."
            ),
            (None, _) if self.done => {
                "Path MTU unknown: even the smallest probes vanished.".to_string()
            }
            (None, _) => match self.vanished {
                Some(vanished) => format!(
                    "Path MTU unknown: probes of {vanished} bytes vanish without an ICMP \
                     error, and the search for the largest that arrives was still trying {}.",
                    self.size
                ),
                None => format!(
                    "Path MTU unknown: the search was still trying {} bytes.",
                    self.size
                ),
            },
        }];
        if let Some(hole) = &self.hole {
            let reach = match hole.passed {
                0 => String::new(),
                passed => format!("get as far as hop {passed} and "),
            };
            lines.push(match hole.found {
                true => format!("  They {reach}vanish before hop {}.", hole.lost_at),
                false => format!(
                    "  They {reach}vanish by hop {}; the search for where was trying hop {}.",
                    hole.lost_at, hole.ttl
                ),
            });
        }
        for (drop, before) in self.steps() {
            let place = match (drop.hop, drop.router) {
                (_, None) => "on this machine's route".to_string(),
                (Some(hop), Some(router)) => format!("after hop {hop} ({})", show(router)),
                (None, Some(router)) => format!("after {}", show(router)),
            };
            lines.push(format!("  {before} to {} {place}", drop.mtu));
        }
        lines
    }

    /// Each drop with the MTU before it
    fn steps(&self) -> impl Iterator<Item = (&MtuDrop, u16)> {
        let befores = std::iter::once(self.start).chain(self.drops.iter().map(|drop| drop.mtu));
        self.drops.iter().zip(befores)
    }

    /// A line for under hop `hop` per drop past it, as the table shows them, and
    /// where a black hole is.
    pub fn notes_after(&self, hop: u8) -> Vec<String> {
        let mut notes: Vec<String> = self
            .steps()
            .filter(|(drop, _)| drop.hop == Some(hop) && drop.router.is_some())
            .map(|(drop, before)| format!("MTU {before} → {} past this hop", drop.mtu))
            .collect();
        if let Some(hole) = self.hole.as_ref().filter(|hole| hole.found) {
            let mtu = hole.size - 1;
            if hole.passed == hop {
                notes.push(format!("probes over {mtu} bytes vanish past this hop"));
            } else if hole.passed == 0 && hole.lost_at == hop {
                notes.push(format!("probes over {mtu} bytes vanish before this hop"));
            }
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn shrinks_to_what_routers_report_until_a_probe_arrives() {
        let mut pmtu = PathMtu::new(1500, false);
        assert_eq!(pmtu.probe_size(), Some(1500));
        pmtu.too_big(1500, router(1), Some(1), 1492);
        assert_eq!(pmtu.probe_size(), Some(1492));
        pmtu.too_big(1500, router(1), Some(1), 1492); // a second answer about 1500
        pmtu.too_big(1492, router(4), Some(4), 1400);
        assert_eq!(pmtu.probe_size(), Some(1400));
        assert_eq!(pmtu.mtu(), None);

        pmtu.arrived(1400, 9);
        assert_eq!((pmtu.probe_size(), pmtu.mtu()), (None, Some(1400)));
        assert_eq!(pmtu.status(), "PMTU 1400");
        assert_eq!(pmtu.notes_after(4), ["MTU 1492 → 1400 past this hop"]);
        assert!(pmtu.notes_after(2).is_empty());
        assert_eq!(
            pmtu.summary(Some(9)),
            [
                "Path MTU 1400.",
                "  1500 to 1492 after hop 1 (10.0.0.1)",
                "  1492 to 1400 after hop 4 (10.0.0.4)",
            ]
        );
    }

    #[test]
    fn a_router_that_names_no_mtu_gets_the_next_common_one() {
        let mut pmtu = PathMtu::new(1500, false);
        pmtu.too_big(1500, router(2), Some(2), 0);
        assert_eq!(pmtu.probe_size(), Some(1492));
        pmtu.too_big(1492, router(2), Some(2), 1600); // nonsense: larger than the probe
        assert_eq!(pmtu.probe_size(), Some(1480));
    }

    #[test]
    fn a_size_that_vanishes_starts_a_black_hole_search() {
        // The path carries 1400 bytes; larger probes vanish without an error.
        let mut pmtu = PathMtu::new(1500, false);
        pmtu.lost(1500, 9);
        assert_eq!(pmtu.probe_size(), Some(1500), "one loss proves nothing");
        pmtu.lost(1500, 9);
        let mut probes = 0;
        while let Some(size) = pmtu.probe_size() {
            probes += 1;
            assert!(probes < 40, "the search ends");
            if size <= 1400 {
                pmtu.arrived(size, 9);
            } else {
                pmtu.lost(size, 9);
                pmtu.lost(size, 9);
            }
        }
        assert_eq!((pmtu.mtu(), pmtu.black_hole()), (Some(1400), Some(1401)));
        assert_eq!(pmtu.status(), "PMTU 1400, black hole above");
        assert!(pmtu.summary(Some(9))[0].contains("1401 bytes and more vanish"));
    }

    /// Find and locate a black hole for more than 1400 bytes past hop `past` on a
    /// 9-hop path, probing only the hops for which `answers` holds.
    fn locate(past: u8, answers: impl Fn(u8) -> bool) -> PathMtu {
        let mut pmtu = PathMtu::new(1500, false);
        pmtu.lost(1500, 9);
        pmtu.lost(1500, 9);
        while let Some((ttl, size)) = pmtu.next_probe(9, &answers) {
            if size <= 1400 || ttl <= past {
                pmtu.arrived(size, ttl);
            } else {
                pmtu.lost(size, ttl);
                pmtu.lost(size, ttl);
            }
        }
        pmtu
    }

    #[test]
    fn a_black_hole_is_located_by_halving_the_hops() {
        let pmtu = locate(6, |_| true);
        let hole = pmtu.hole.as_ref().unwrap();
        assert!(hole.found);
        assert_eq!((hole.passed, hole.lost_at), (6, 7));
        assert_eq!(pmtu.status(), "PMTU 1400, black hole past hop 6");
        assert_eq!(
            pmtu.notes_after(6),
            ["probes over 1400 bytes vanish past this hop"]
        );
        assert!(pmtu.notes_after(7).is_empty());
        assert_eq!(
            pmtu.summary(Some(9))[1],
            "  They get as far as hop 6 and vanish before hop 7."
        );
    }

    #[test]
    fn silent_hops_leave_a_black_hole_between_the_ones_that_answer() {
        let pmtu = locate(6, |ttl| ttl != 7);
        let hole = pmtu.hole.as_ref().unwrap();
        assert_eq!((hole.passed, hole.lost_at), (6, 8));

        let pmtu = locate(0, |_| true);
        let hole = pmtu.hole.as_ref().unwrap();
        assert_eq!(
            (hole.passed, hole.lost_at),
            (0, 1),
            "lost on the first link"
        );
        assert_eq!(
            pmtu.notes_after(1),
            ["probes over 1400 bytes vanish before this hop"]
        );
        assert_eq!(pmtu.summary(Some(9))[1], "  They vanish before hop 1.");
    }

    #[test]
    fn ipv6_never_goes_below_1280() {
        let mut pmtu = PathMtu::new(1500, true);
        pmtu.too_big(1500, router(3), Some(3), 1000);
        assert_eq!(pmtu.probe_size(), Some(1492), "1000 is no IPv6 MTU");
        for _ in 0..40 {
            if let Some(size) = pmtu.probe_size() {
                pmtu.lost(size, 9);
            }
        }
        assert_eq!(pmtu.probe_size(), None);
        assert_eq!(pmtu.mtu(), None);
        assert_eq!(
            pmtu.summary(None)[0],
            "Path MTU unknown: even the smallest probes vanished."
        );
    }

    #[test]
    fn a_local_refusal_steps_down_and_names_this_machine() {
        let mut pmtu = PathMtu::new(1500, false);
        pmtu.refused(1500);
        pmtu.refused(1492);
        assert_eq!(pmtu.probe_size(), Some(1480));
        assert_eq!(pmtu.drops.len(), 1, "one entry for this machine");
        pmtu.arrived(1480, 3);
        let summary = pmtu.summary(None);
        assert_eq!(
            summary[0],
            "Path MTU 1480 up to hop 3, the last that answered."
        );
        assert_eq!(summary[1], "  1500 to 1480 on this machine's route");
    }

    #[test]
    fn stale_answers_change_nothing() {
        let mut pmtu = PathMtu::new(1500, false);
        pmtu.too_big(1500, router(1), Some(1), 1400);
        pmtu.lost(1500, 9);
        pmtu.lost(1500, 9);
        assert_eq!(pmtu.probe_size(), Some(1400), "losses of the old size");
        pmtu.arrived(1400, 5);
        pmtu.too_big(1400, router(2), Some(2), 1300);
        assert_eq!(pmtu.mtu(), Some(1400), "the search is over");
    }
}
