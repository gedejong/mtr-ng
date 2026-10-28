//! RTT colours, the history cells (sparkline or heatmap), and the RTT scale they share.

use crate::hop_stats::PacketOutcome;
use crate::{HopStats, SparklineScale};
use ratatui::{
    style::{Color, Style},
    text::Span,
};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorSupport {
    None,      // monochrome: NO_COLOR, TERM=dumb
    Basic,     // 16 colors
    Extended,  // 256 colors
    TrueColor, // 24-bit RGB
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VisualizationMode {
    Sparkline, // bar height and colour
    Heatmap,   // colour only
}

/// Keeps the log scale finite and spread out for sub-millisecond RTTs.
const LOG_OFFSET_MS: f64 = 0.1;

/// Probes per hop the RTT scale looks back over.
const SCALE_WINDOW: usize = 100;

/// Maps an RTT onto 0..=1 for bar height and colour. The axis labels use the same
/// mapping, so a bar and its label always agree.
#[derive(Debug, Clone, Copy)]
pub struct RttScale {
    min_ms: f64,
    max_ms: f64,
    kind: SparklineScale,
}

impl RttScale {
    /// Spans the replies among each hop's latest `SCALE_WINDOW` probes, about what
    /// the graph shows; `None` before the first reply.
    pub fn from_hops<'a>(
        hops: impl IntoIterator<Item = &'a HopStats>,
        kind: SparklineScale,
    ) -> Option<Self> {
        let (mut min_ms, mut max_ms) = (f64::INFINITY, f64::NEG_INFINITY);
        let latest = |hop: &'a HopStats| hop.history.iter().rev().take(SCALE_WINDOW);
        let rtts = hops
            .into_iter()
            .flat_map(latest)
            .filter_map(|r| match r.outcome {
                PacketOutcome::Received(rtt) => Some(rtt),
                _ => None,
            });
        for rtt in rtts {
            let ms = rtt.as_secs_f64() * 1000.0;
            min_ms = min_ms.min(ms);
            max_ms = max_ms.max(ms);
        }
        (min_ms <= max_ms).then_some(Self {
            min_ms,
            max_ms,
            kind,
        })
    }

    fn forward(&self, ms: f64) -> f64 {
        match self.kind {
            SparklineScale::Linear => ms,
            SparklineScale::Logarithmic => (ms + LOG_OFFSET_MS).ln(),
        }
    }

    fn inverse(&self, t: f64) -> f64 {
        match self.kind {
            SparklineScale::Linear => t,
            SparklineScale::Logarithmic => t.exp() - LOG_OFFSET_MS,
        }
    }

    /// Where `rtt` sits between the fastest and the slowest reply, from 0 to 1.
    pub fn ratio(&self, rtt: Duration) -> f64 {
        let (lo, hi) = (self.forward(self.min_ms), self.forward(self.max_ms));
        if hi - lo <= f64::EPSILON {
            return 0.0;
        }
        let t = self.forward(rtt.as_secs_f64() * 1000.0);
        ((t - lo) / (hi - lo)).clamp(0.0, 1.0)
    }

    /// The RTT in ms at `ratio` along the axis; the inverse of [`RttScale::ratio`].
    pub fn at(&self, ratio: f64) -> f64 {
        let (lo, hi) = (self.forward(self.min_ms), self.forward(self.max_ms));
        self.inverse(lo + ratio * (hi - lo))
    }
}

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// The cell for a position on the RTT scale: bar height in a sparkline, a full block in a heatmap.
pub fn symbol(ratio: f64, mode: VisualizationMode) -> char {
    match mode {
        VisualizationMode::Sparkline => BARS[(ratio.clamp(0.0, 1.0) * 7.0).round() as usize],
        VisualizationMode::Heatmap => '█',
    }
}

/// One cell per probe in the hop's history, newest last, at most `width` of them.
pub fn history_spans(
    hop: &HopStats,
    scale: Option<&RttScale>,
    mode: VisualizationMode,
    colors: ColorSupport,
    width: usize,
) -> Vec<Span<'static>> {
    let skip = hop.history.len().saturating_sub(width);
    hop.history
        .iter()
        .skip(skip)
        .map(|record| match record.outcome {
            PacketOutcome::Received(rtt) => {
                let ratio = scale.map_or(0.0, |s| s.ratio(rtt));
                Span::styled(symbol(ratio, mode).to_string(), fg(gradient(ratio, colors)))
            }
            PacketOutcome::Lost => Span::styled("·", fg(loss_color(colors))),
            PacketOutcome::Pending => Span::styled("?", fg(pending_color(colors))),
        })
        .collect()
}

fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

/// Colour for a position on the RTT scale: blue (fast) through light blue to orange (slow).
pub fn gradient(ratio: f64, colors: ColorSupport) -> Color {
    let ratio = ratio.clamp(0.0, 1.0);
    match colors {
        ColorSupport::None => Color::Reset,
        ColorSupport::Basic => match ratio {
            r if r < 0.17 => Color::Green,
            r if r < 0.33 => Color::Cyan,
            r if r < 0.50 => Color::Blue,
            r if r < 0.67 => Color::Yellow,
            r if r < 0.83 => Color::Magenta,
            _ => Color::Red,
        },
        ColorSupport::Extended => {
            const STEPS: [u8; 10] = [21, 27, 33, 39, 51, 46, 226, 214, 208, 196];
            Color::Indexed(STEPS[((ratio * 10.0) as usize).min(STEPS.len() - 1)])
        }
        ColorSupport::TrueColor => {
            let (from, to, t) = if ratio < 0.5 {
                ((0.0, 50.0, 150.0), (150.0, 220.0, 255.0), ratio * 2.0)
            } else {
                (
                    (150.0, 220.0, 255.0),
                    (220.0, 120.0, 0.0),
                    (ratio - 0.5) * 2.0,
                )
            };
            let mix = |a: f64, b: f64| (a + (b - a) * t).round() as u8;
            Color::Rgb(mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
        }
    }
}

/// Colour of the responder lettered `index` (A is 0) in the flow matrix and the
/// hop details.
pub fn letter_color(index: usize, colors: ColorSupport) -> Color {
    const BASIC: [Color; 6] = [
        Color::Cyan,
        Color::Yellow,
        Color::Magenta,
        Color::Green,
        Color::Blue,
        Color::White,
    ];
    const EXTENDED: [u8; 6] = [45, 220, 207, 119, 75, 252];
    match colors {
        ColorSupport::None => Color::Reset,
        ColorSupport::Basic => BASIC[index % BASIC.len()],
        ColorSupport::Extended | ColorSupport::TrueColor => {
            Color::Indexed(EXTENDED[index % EXTENDED.len()])
        }
    }
}

/// Replies per bucket, in `buckets` equal slices from the fastest RTT to the slowest.
pub fn histogram(rtts: &[Duration], buckets: usize) -> Vec<usize> {
    let mut counts = vec![0; buckets];
    let (Some(min), Some(max)) = (rtts.iter().min(), rtts.iter().max()) else {
        return counts;
    };
    let span = (*max - *min).as_secs_f64();
    for rtt in rtts {
        let at = match span > 0.0 {
            true => ((*rtt - *min).as_secs_f64() / span * buckets as f64) as usize,
            false => 0,
        };
        if let Some(count) = counts.get_mut(at.min(buckets.saturating_sub(1))) {
            *count += 1;
        }
    }
    counts
}

/// A histogram as bars, coloured by where each bucket sits between fastest and
/// slowest. Empty buckets stay blank, so a lone outlier stands apart.
pub fn histogram_spans(counts: &[usize], colors: ColorSupport) -> Vec<Span<'static>> {
    let most = counts.iter().copied().max().unwrap_or(0).max(1);
    let last = counts.len().saturating_sub(1).max(1);
    counts
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            let bar = match count {
                0 => ' ',
                _ => BARS[(count * (BARS.len() - 1)).div_ceil(most)],
            };
            let ratio = i as f64 / last as f64;
            Span::styled(bar.to_string(), fg(gradient(ratio, colors)))
        })
        .collect()
}

pub fn loss_color(colors: ColorSupport) -> Color {
    match colors {
        ColorSupport::None => Color::Reset,
        ColorSupport::Basic => Color::Red,
        ColorSupport::Extended => Color::Indexed(196),
        ColorSupport::TrueColor => Color::Rgb(255, 0, 0),
    }
}

/// Coastlines on the map: a mid grey, as clear on a dark background as on a light
/// one and still behind the path. The 16 basic colours have no mid grey.
pub fn coast_color(colors: ColorSupport) -> Color {
    match colors {
        ColorSupport::None => Color::Reset,
        ColorSupport::Basic => Color::DarkGray,
        ColorSupport::Extended => Color::Indexed(244),
        ColorSupport::TrueColor => Color::Rgb(128, 128, 128),
    }
}

fn pending_color(colors: ColorSupport) -> Color {
    match colors {
        ColorSupport::None => Color::Reset,
        ColorSupport::Basic => Color::Blue,
        ColorSupport::Extended => Color::Indexed(27),
        ColorSupport::TrueColor => Color::Rgb(100, 100, 255),
    }
}

/// Colour support from the environment. `NO_COLOR` (any value) and `TERM=dumb` turn
/// colour off; see <https://no-color.org>.
pub fn detect_color_support() -> ColorSupport {
    let var = |name| std::env::var(name).ok();
    color_support_from(var("NO_COLOR"), var("COLORTERM"), var("TERM"))
}

fn color_support_from(
    no_color: Option<String>,
    colorterm: Option<String>,
    term: Option<String>,
) -> ColorSupport {
    let term = term.unwrap_or_default();
    if no_color.is_some_and(|v| !v.is_empty()) || term == "dumb" {
        ColorSupport::None
    } else if colorterm.is_some_and(|c| c.contains("truecolor") || c.contains("24bit")) {
        ColorSupport::TrueColor
    } else if term.contains("256") {
        ColorSupport::Extended
    } else {
        ColorSupport::Basic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop_with(rtts_us: &[u64]) -> HopStats {
        let mut hop = HopStats::new(1);
        for &us in rtts_us {
            hop.increment_sent(0);
            hop.add_rtt(Duration::from_micros(us));
        }
        hop
    }

    #[test]
    fn log_scale_spreads_sub_millisecond_rtts() {
        // A 0.3 ms gateway next to a 20 ms hop used to flatten every bar to ▁
        let hops = [hop_with(&[300, 600]), hop_with(&[20_000])];
        let scale = RttScale::from_hops(&hops, SparklineScale::Logarithmic).unwrap();
        assert_eq!(scale.ratio(Duration::from_micros(300)), 0.0);
        assert!(scale.ratio(Duration::from_micros(600)) > 0.1);
        assert_eq!(scale.ratio(Duration::from_millis(20)), 1.0);
    }

    #[test]
    fn labels_use_the_same_mapping_as_bars() {
        let hops = [hop_with(&[300, 5_000, 80_000])];
        for kind in [SparklineScale::Linear, SparklineScale::Logarithmic] {
            let scale = RttScale::from_hops(&hops, kind).unwrap();
            for ms in [0.3, 5.0, 80.0] {
                let back = scale.at(scale.ratio(Duration::from_secs_f64(ms / 1000.0)));
                assert!((back - ms).abs() < 1e-6, "{kind:?} {ms} -> {back}");
            }
        }
    }

    #[test]
    fn history_shows_the_newest_samples_that_fit() {
        let hop = hop_with(&[1_000, 2_000, 3_000, 4_000]);
        let scale = RttScale::from_hops([&hop], SparklineScale::Linear);
        let spans = history_spans(
            &hop,
            scale.as_ref(),
            VisualizationMode::Sparkline,
            ColorSupport::None,
            2,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "▆█");
    }

    #[test]
    fn histogram_spreads_replies_from_fastest_to_slowest() {
        let ms = |v: &[u64]| {
            v.iter()
                .map(|&m| Duration::from_millis(m))
                .collect::<Vec<_>>()
        };
        assert_eq!(histogram(&ms(&[10, 10, 11, 19, 20]), 4), [3, 0, 0, 2]);
        assert_eq!(histogram(&ms(&[7, 7]), 3), [2, 0, 0], "one value");
        assert_eq!(histogram(&[], 2), [0, 0]);

        let bars: String = histogram_spans(&[4, 0, 1, 2], ColorSupport::None)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(bars, "█ ▃▅", "empty buckets blank, any reply at least ▂");
    }

    #[test]
    fn no_color_and_dumb_terminals_get_no_colour() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            color_support_from(s("1"), s("truecolor"), s("xterm-256color")),
            ColorSupport::None
        );
        assert_eq!(
            color_support_from(None, None, s("dumb")),
            ColorSupport::None
        );
        assert_eq!(
            color_support_from(s(""), None, s("xterm-256color")),
            ColorSupport::Extended
        );
        assert_eq!(
            color_support_from(None, s("truecolor"), s("xterm")),
            ColorSupport::TrueColor
        );
    }
}
