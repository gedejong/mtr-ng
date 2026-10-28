//! What each column shows. The TUI table, the column chooser and the report all
//! read headers, names and values from here.

use crate::args::Column;
use crate::geo::{self, Assessment, Origin};
use crate::HopStats;
use std::time::Duration;

/// An RTT in milliseconds with one decimal, as mtr shows it; `???` without a reply.
pub fn format_ms(rtt: Option<Duration>) -> String {
    rtt.map_or_else(
        || "???".to_string(),
        |rtt| format!("{:.1}", rtt.as_secs_f64() * 1000.0),
    )
}

impl Column {
    /// Short header, as mtr prints it.
    pub fn header(self) -> &'static str {
        match self {
            Column::Hop => "",
            Column::Host => "Host",
            Column::Asn => "ASN",
            Column::Geo => "Location",
            Column::Dist => "Dist",
            Column::Reach => "Reach",
            Column::Loss => "Loss%",
            Column::Sent => "Snt",
            Column::Last => "Last",
            Column::Avg => "Avg",
            Column::Ema => "EMA",
            Column::Jitter => "Jttr",
            Column::JitterAvg => "JAvg",
            Column::Best => "Best",
            Column::Worst => "Wrst",
            Column::Stdev => "StDev",
            Column::Graph => "RTT history",
        }
    }

    /// Longer name for the column chooser.
    pub fn label(self) -> &'static str {
        match self {
            Column::Hop => "Hop number",
            Column::Host => "Host name or address",
            Column::Asn => "Autonomous system",
            Column::Geo => "Location (city, country)",
            Column::Dist => "Distance from the origin, km",
            Column::Reach => "Farthest the best RTT reaches, km",
            Column::Loss => "Packet loss",
            Column::Sent => "Probes sent",
            Column::Last => "Last RTT",
            Column::Avg => "Average RTT",
            Column::Ema => "Smoothed RTT (EMA)",
            Column::Jitter => "Last jitter",
            Column::JitterAvg => "Average jitter",
            Column::Best => "Best RTT",
            Column::Worst => "Worst RTT",
            Column::Stdev => "RTT standard deviation",
            Column::Graph => "RTT history graph",
        }
    }

    /// True for the columns that hold one number per hop.
    pub fn is_numeric(self) -> bool {
        !matches!(
            self,
            Column::Hop | Column::Host | Column::Asn | Column::Geo | Column::Graph
        )
    }

    /// Narrowest a numeric column gets, so the table doesn't shift as values change.
    pub fn min_width(self) -> usize {
        match self {
            Column::Loss => 6, // "100.0%"
            Column::Sent => 4,
            _ if self.is_numeric() => 5, // "999.9" ms; slower replies widen the column
            _ => self.header().len(),
        }
    }

    /// The value of a numeric column, in milliseconds, percent or km (`???` when
    /// there is nothing to measure), the AS number or the location. Distances are
    /// from `origin`; `!` marks a location the hop's RTT rules out. Empty for hop,
    /// host and graph.
    pub fn value(self, hop: &HopStats, origin: Option<&Origin>) -> String {
        let ms = format_ms;
        let assessment = || hop.assess(origin);
        let mark = |a: Option<Assessment>| {
            if a.is_some_and(|a| a.impossible) {
                "!"
            } else {
                ""
            }
        };
        match self {
            Column::Loss => format!("{:.1}%", hop.loss_percent),
            Column::Sent => hop.sent.to_string(),
            Column::Last => ms(hop.last_rtt),
            Column::Avg => ms(hop.avg_rtt),
            Column::Ema => ms(hop.ema_rtt),
            Column::Jitter => ms(hop.last_jitter),
            Column::JitterAvg => ms(hop.jitter_avg),
            Column::Best => ms(hop.best_rtt),
            Column::Worst => ms(hop.worst_rtt),
            Column::Stdev if hop.received == 0 => "???".to_string(),
            Column::Stdev => format!("{:.1}", hop.stddev_ms()),
            Column::Asn => match (hop.asn, hop.addr) {
                (Some(asn), _) => format!("AS{asn}"),
                (None, Some(_)) => "AS???".to_string(),
                (None, None) => String::new(),
            },
            Column::Geo => match (&hop.geo, hop.addr) {
                (Some(fix), _) => format!("{}{}", fix.place(), mark(assessment())),
                (None, Some(_)) => "???".to_string(),
                (None, None) => String::new(),
            },
            Column::Dist => match assessment() {
                Some(a) => format!("{:.0}{}", a.distance_km, mark(Some(a))),
                None => "???".to_string(),
            },
            Column::Reach => hop.best_rtt.map_or("???".to_string(), |rtt| {
                format!("{:.0}", geo::reach_km(rtt))
            }),
            Column::Hop | Column::Host | Column::Graph => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_column_has_a_distinct_header_and_label() {
        let all = Column::all();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.label(), b.label());
                if !a.header().is_empty() {
                    assert_ne!(a.header(), b.header());
                }
            }
        }
    }

    #[test]
    fn values_fit_their_minimum_width() {
        let mut hop = HopStats::new(1);
        for _ in 0..3 {
            hop.increment_sent(0);
            hop.add_timeout();
        }
        assert_eq!(Column::Loss.value(&hop, None), "100.0%");
        assert_eq!(Column::Avg.value(&hop, None), "???");
        hop.increment_sent(0);
        hop.add_rtt(Duration::from_micros(999_940));
        assert_eq!(Column::Last.value(&hop, None), "999.9");
        for column in Column::all().into_iter().filter(|c| c.is_numeric()) {
            assert!(
                column.value(&hop, None).len() <= column.min_width(),
                "{column:?}"
            );
        }
    }

    #[test]
    fn locations_and_distances_carry_the_light_speed_verdict() {
        let mut hop = HopStats::new(5);
        let addr = "192.0.2.1".parse().unwrap();
        hop.increment_sent(0);
        hop.add_rtt_from_addr(addr, Duration::from_millis(20));
        let origin = Origin {
            lat: 52.37,
            lon: 4.89,
            slack_km: 0.0,
            hop: None,
        };
        let value = |c: Column, hop: &HopStats| c.value(hop, Some(&origin));
        assert_eq!(value(Column::Geo, &hop), "???", "no location yet");
        assert_eq!(value(Column::Reach, &hop), "2000");

        hop.set_geo_for_addr(addr, geo::tests::city("London", "GB", 51.51, -0.13));
        assert_eq!(value(Column::Geo, &hop), "London, GB");
        assert_eq!(value(Column::Dist, &hop), "357");
        assert_eq!(Column::Dist.value(&hop, None), "???", "no origin");

        hop.set_geo_for_addr(addr, geo::tests::city("Ashburn", "US", 39.04, -77.49));
        assert_eq!(value(Column::Geo, &hop), "Ashburn, US!");
        assert!(value(Column::Dist, &hop).ends_with('!'));
    }
}
