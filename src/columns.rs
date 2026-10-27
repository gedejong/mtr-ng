//! What each column shows. The TUI table, the column chooser and the report all
//! read headers, names and values from here.

use crate::args::Column;
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
            Column::Hop | Column::Host | Column::Asn | Column::Graph
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

    /// The value of a numeric column, in milliseconds or percent (`???` when there
    /// is no reply to measure), or the AS number. Empty for hop, host and graph.
    pub fn value(self, hop: &HopStats) -> String {
        let ms = format_ms;
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
            hop.increment_sent();
            hop.add_timeout();
        }
        assert_eq!(Column::Loss.value(&hop), "100.0%");
        assert_eq!(Column::Avg.value(&hop), "???");
        hop.increment_sent();
        hop.add_rtt(Duration::from_micros(999_940));
        assert_eq!(Column::Last.value(&hop), "999.9");
        for column in Column::all().into_iter().filter(|c| c.is_numeric()) {
            assert!(column.value(&hop).len() <= column.min_width(), "{column:?}");
        }
    }
}
