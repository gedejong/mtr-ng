//! Visualization utilities for RTT data
//!
//! This module provides sparkline generation, color management, and RTT calculation
//! utilities for the terminal user interface.

use crate::utils;
use crate::{HopStats, SparklineScale};
use ratatui::{style::Style, text::Span};

// ========================================
// Public Types
// ========================================

#[derive(Debug, Clone, Copy)]
pub enum ColorSupport {
    None,      // No color support
    Basic,     // 16 colors
    Extended,  // 256 colors
    TrueColor, // 24-bit RGB
}

#[derive(Debug, Clone, Copy)]
pub enum VisualizationMode {
    Sparkline, // Variable height characters (▁▂▃▄▅▆▇█)
    Heatmap,   // Full height blocks (█) with colors only
}

// ========================================
// Color Management
// ========================================

/// Color scheme functions for RTT visualization
pub mod colors {
    use super::ColorSupport;
    use crate::utils;
    use ratatui::style::Color;

    pub fn get_rtt_color(ratio: f64, color_support: ColorSupport) -> (char, Color) {
        let level = (utils::math::clamp_ratio(ratio) * 8.0).round() as usize;
        let chars = ['▁', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let char = chars[utils::math::safe_array_index(level as f64, chars.len())];

        let color = match color_support {
            ColorSupport::None => Color::White,
            ColorSupport::Basic => {
                // Use standard 16-color ANSI palette based on level
                let colors = [
                    Color::Green,   // Level 0 - Very low RTT
                    Color::Green,   // Level 1 - Low RTT
                    Color::Cyan,    // Level 2 - Low-medium RTT
                    Color::Blue,    // Level 3 - Medium-low RTT
                    Color::Yellow,  // Level 4 - Medium RTT
                    Color::Yellow,  // Level 5 - Medium-high RTT
                    Color::Magenta, // Level 6 - High RTT
                    Color::Red,     // Level 7 - Very high RTT
                    Color::Red,     // Level 8 - Maximum RTT
                ];
                colors[utils::math::safe_array_index(level as f64, colors.len())]
            }
            ColorSupport::Extended => {
                // Use a broader range of 256-color palette: blue → cyan → green → yellow → orange → red
                let colors = [
                    17,  // Dark blue (low RTT)
                    21,  // Medium blue
                    33,  // Bright blue
                    39,  // Cyan-blue
                    51,  // Cyan
                    87,  // Light cyan
                    118, // Green-cyan
                    154, // Green
                    190, // Light green
                    226, // Yellow-green
                    220, // Yellow
                    214, // Light yellow
                    208, // Orange-yellow
                    202, // Orange
                    196, // Red-orange
                    160, // Red
                    124, // Dark red
                    88,  // Very dark red (high RTT)
                ];
                let index = utils::math::safe_array_index(level as f64, colors.len());
                Color::Indexed(colors[index])
            }
            ColorSupport::TrueColor => {
                let colors = [
                    (0, 50, 150),
                    (0, 100, 200),
                    (50, 150, 255),
                    (100, 200, 255),
                    (150, 220, 255),
                    (255, 200, 100),
                    (255, 150, 50),
                    (220, 120, 0),
                    (150, 80, 0),
                ];
                let (r, g, b) = colors[utils::math::safe_array_index(level as f64, colors.len())];
                Color::Rgb(r, g, b)
            }
        };

        (char, color)
    }

    pub fn get_smooth_gradient_color(ratio: f64, color_support: ColorSupport) -> Color {
        let ratio = utils::math::clamp_ratio(ratio);

        match color_support {
            ColorSupport::None => Color::White,
            ColorSupport::Basic => {
                // Use only standard 16-color ANSI palette

                if ratio < 0.17 {
                    Color::Green // Low RTT (fast)
                } else if ratio < 0.33 {
                    Color::Cyan // Low-medium RTT
                } else if ratio < 0.50 {
                    Color::Blue // Medium RTT
                } else if ratio < 0.67 {
                    Color::Yellow // Medium-high RTT
                } else if ratio < 0.83 {
                    Color::Magenta // High RTT
                } else {
                    Color::Red // Maximum RTT (slow)
                }
            }
            ColorSupport::Extended => {
                // Full 256-color gradient: blue → cyan → green → yellow → orange → red
                let color_index = if ratio < 0.1 {
                    21 // Dark blue
                } else if ratio < 0.2 {
                    27 // Medium blue
                } else if ratio < 0.3 {
                    33 // Light blue
                } else if ratio < 0.4 {
                    39 // Cyan-blue
                } else if ratio < 0.5 {
                    51 // Cyan
                } else if ratio < 0.6 {
                    46 // Green
                } else if ratio < 0.7 {
                    226 // Yellow
                } else if ratio < 0.8 {
                    214 // Orange-yellow
                } else if ratio < 0.9 {
                    208 // Orange
                } else {
                    196 // Red
                };
                Color::Indexed(color_index)
            }
            ColorSupport::TrueColor => {
                let (r, g, b) = if ratio < 0.5 {
                    let t = ratio * 2.0;
                    interpolate_rgb((0.0, 50.0, 150.0), (150.0, 220.0, 255.0), t)
                } else {
                    let t = (ratio - 0.5) * 2.0;
                    interpolate_rgb((150.0, 220.0, 255.0), (220.0, 120.0, 0.0), t)
                };
                Color::Rgb(r.round() as u8, g.round() as u8, b.round() as u8)
            }
        }
    }

    pub fn get_loss_color(color_support: ColorSupport) -> Color {
        match color_support {
            ColorSupport::None => Color::White,
            ColorSupport::Basic => Color::Red,
            ColorSupport::Extended => Color::Indexed(196),
            ColorSupport::TrueColor => Color::Rgb(255, 0, 0),
        }
    }

    pub fn get_pending_color(color_support: ColorSupport) -> Color {
        match color_support {
            ColorSupport::None => Color::White,
            ColorSupport::Basic => Color::Blue,
            ColorSupport::Extended => Color::Indexed(27),
            ColorSupport::TrueColor => Color::Rgb(100, 100, 255),
        }
    }

    fn interpolate_rgb(
        start: (f64, f64, f64),
        end: (f64, f64, f64),
        ratio: f64,
    ) -> (f64, f64, f64) {
        let ratio = utils::math::clamp_ratio(ratio);
        (
            start.0 + (end.0 - start.0) * ratio,
            start.1 + (end.1 - start.1) * ratio,
            start.2 + (end.2 - start.2) * ratio,
        )
    }
}

// ========================================
// RTT Calculation Utilities
// ========================================

/// Calculate ratio for RTT visualization scaling
pub fn calculate_rtt_ratio(
    rtt_ms: u64,
    global_min: u64,
    global_max: u64,
    scale: SparklineScale,
) -> f64 {
    if global_min == global_max || rtt_ms == 0 {
        return 0.0;
    }

    match scale {
        SparklineScale::Linear => {
            // Properly normalize based on the actual RTT range (min to max)
            let normalized =
                (rtt_ms.saturating_sub(global_min)) as f64 / (global_max - global_min) as f64;
            normalized.clamp(0.0, 1.0)
        }
        SparklineScale::Logarithmic => {
            utils::math::calculate_log_ratio(rtt_ms as f64, global_min as f64, global_max as f64)
        }
    }
}

// ========================================
// Sparkline Generation
// ========================================

/// Generate colored sparkline spans for RTT visualization
pub fn create_sparkline_spans(
    hop: &HopStats,
    global_min_rtt: u64,
    global_max_rtt: u64,
    scale: SparklineScale,
    color_support: ColorSupport,
    max_width: usize,
) -> Vec<Span<'static>> {
    if hop.sent == 0 || max_width == 0 {
        return vec![];
    }

    let packet_outcomes: Vec<_> = hop.packet_history.iter().collect();
    if packet_outcomes.is_empty() {
        return vec![Span::raw(" ".repeat(max_width))];
    }

    let data_to_show = if packet_outcomes.len() > max_width {
        &packet_outcomes[packet_outcomes.len() - max_width..]
    } else {
        &packet_outcomes[..]
    };

    let mut spans: Vec<Span<'static>> = data_to_show
        .iter()
        .map(|outcome| match outcome {
            crate::hop_stats::PacketOutcome::Received(rtt) => {
                let rtt_ms = utils::time::duration_to_ms_u64(*rtt);
                let ratio = calculate_rtt_ratio(rtt_ms, global_min_rtt, global_max_rtt, scale);
                // Get the character based on ratio (keep variable height)
                let char = utils::visualization::get_sparkline_char(ratio);
                // Use smooth gradient color like heatmap
                let color = colors::get_smooth_gradient_color(ratio, color_support);
                Span::styled(char.to_string(), Style::default().fg(color))
            }
            crate::hop_stats::PacketOutcome::Lost => {
                let color = colors::get_loss_color(color_support);
                Span::styled("·".to_string(), Style::default().fg(color))
            }
            crate::hop_stats::PacketOutcome::Pending => {
                let color = colors::get_pending_color(color_support);
                Span::styled("?".to_string(), Style::default().fg(color))
            }
        })
        .collect();

    if spans.len() < max_width {
        spans.push(Span::raw(" ".repeat(max_width - spans.len())));
    }

    spans
}

/// Generate colored heatmap spans for RTT visualization (full-height blocks)
pub fn create_heatmap_spans(
    hop: &HopStats,
    global_min_rtt: u64,
    global_max_rtt: u64,
    scale: SparklineScale,
    color_support: ColorSupport,
    max_width: usize,
) -> Vec<Span<'static>> {
    if hop.sent == 0 || max_width == 0 {
        return vec![];
    }

    let packet_outcomes: Vec<_> = hop.packet_history.iter().collect();
    if packet_outcomes.is_empty() {
        return vec![Span::raw(" ".repeat(max_width))];
    }

    let data_to_show = if packet_outcomes.len() > max_width {
        &packet_outcomes[packet_outcomes.len() - max_width..]
    } else {
        &packet_outcomes[..]
    };

    let mut spans: Vec<Span<'static>> = data_to_show
        .iter()
        .map(|outcome| {
            match outcome {
                crate::hop_stats::PacketOutcome::Received(rtt) => {
                    let rtt_ms = utils::time::duration_to_ms_u64(*rtt);
                    let ratio = calculate_rtt_ratio(rtt_ms, global_min_rtt, global_max_rtt, scale);
                    // Use full-height block with color based on RTT ratio
                    let color = colors::get_smooth_gradient_color(ratio, color_support);
                    Span::styled("█".to_string(), Style::default().fg(color))
                }
                crate::hop_stats::PacketOutcome::Lost => {
                    let color = colors::get_loss_color(color_support);
                    Span::styled("·".to_string(), Style::default().fg(color))
                }
                crate::hop_stats::PacketOutcome::Pending => {
                    let color = colors::get_pending_color(color_support);
                    Span::styled("?".to_string(), Style::default().fg(color))
                }
            }
        })
        .collect();

    if spans.len() < max_width {
        spans.push(Span::raw(" ".repeat(max_width - spans.len())));
    }

    spans
}

// ========================================
// Terminal Capability Detection
// ========================================

/// Detect terminal color support capabilities
pub fn detect_color_support() -> ColorSupport {
    use std::env;

    // Check for explicit color support environment variables
    if let Ok(colorterm) = env::var("COLORTERM") {
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return ColorSupport::TrueColor;
        }
    }

    // Check TERM environment variable
    if let Ok(term) = env::var("TERM") {
        if term.contains("256color") || term.contains("256") {
            return ColorSupport::Extended;
        } else if term.contains("color") {
            return ColorSupport::Basic;
        }
    }

    // Default to basic color support
    ColorSupport::Basic
}
