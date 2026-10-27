// Individual modules import what they need

/// Time conversion utilities
pub mod time {
    use std::time::Duration;

    /// Convert Duration to milliseconds as f64
    pub fn duration_to_ms_f64(duration: Duration) -> f64 {
        duration.as_secs_f64() * 1000.0
    }

    /// Convert Duration to milliseconds as u64
    pub fn duration_to_ms_u64(duration: Duration) -> u64 {
        (duration.as_secs_f64() * 1000.0) as u64
    }

    /// Convert Duration to microseconds for high precision
    pub fn duration_to_us_f64(duration: Duration) -> f64 {
        duration.as_secs_f64() * 1_000_000.0
    }

    /// Format duration as milliseconds with one decimal place
    pub fn format_duration_ms(duration: Duration) -> String {
        format!("{:.1}", duration_to_ms_f64(duration))
    }

    /// Format duration with high precision (microseconds)
    pub fn format_duration_us(duration: Duration) -> String {
        let us = duration_to_us_f64(duration);
        if us < 1000.0 {
            format!("{:.1}μs", us)
        } else {
            format!("{:.1}ms", us / 1000.0)
        }
    }

    /// Format optional duration as milliseconds with one decimal place, or "???" if None
    pub fn format_optional_duration_ms(duration: Option<Duration>) -> String {
        duration
            .map(format_duration_ms)
            .unwrap_or_else(|| "???".to_string())
    }

    /// Calculate timing jitter between consecutive measurements
    pub fn calculate_timing_jitter(current: Duration, previous: Duration) -> Duration {
        current.abs_diff(previous)
    }

    /// Exponential moving average for timing with configurable alpha
    pub fn calculate_timing_ema(
        current: Duration,
        previous_ema: Option<Duration>,
        alpha: f64,
    ) -> Duration {
        match previous_ema {
            None => current,
            Some(prev) => {
                let current_ns = current.as_nanos() as f64;
                let prev_ns = prev.as_nanos() as f64;
                let ema_ns = alpha * current_ns + (1.0 - alpha) * prev_ns;
                Duration::from_nanos(ema_ns as u64)
            }
        }
    }
}

/// Mathematical utilities
pub mod math {
    /// Clamp a value between min and max
    pub fn clamp_f64(value: f64, min: f64, max: f64) -> f64 {
        value.clamp(min, max)
    }

    /// Safe array indexing with bounds checking
    pub fn safe_array_index(ratio: f64, array_len: usize) -> usize {
        let index = (ratio * (array_len - 1) as f64).round() as usize;
        index.min(array_len - 1)
    }

    /// Calculate logarithmic ratio for scaling
    pub fn calculate_log_ratio(value: f64, min_value: f64, max_value: f64) -> f64 {
        if min_value <= 0.0 || max_value <= min_value || value <= 0.0 {
            return 0.0;
        }

        let log_value = value.ln();
        let log_min = min_value.ln();
        let log_max = max_value.ln();

        ((log_value - log_min) / (log_max - log_min)).clamp(0.0, 1.0)
    }

    /// Clamp ratio to 0.0-1.0 range (common pattern)
    pub fn clamp_ratio(ratio: f64) -> f64 {
        ratio.clamp(0.0, 1.0)
    }

    /// Ensure value meets a minimum threshold
    pub fn max_with_minimum<T: Ord>(value: T, min_value: T) -> T {
        value.max(min_value)
    }
}

/// Network address utilities
pub mod network {
    use std::net::IpAddr;

    /// Format IP address as string, or "???" if None
    pub fn format_optional_ip(addr: Option<IpAddr>) -> String {
        addr.map(|a| a.to_string())
            .unwrap_or_else(|| "???".to_string())
    }

    /// Format hostname with fallback to IP address
    pub fn format_hostname_with_fallback(hostname: Option<String>, addr: Option<IpAddr>) -> String {
        hostname.unwrap_or_else(|| format_optional_ip(addr))
    }

    /// Truncate hostname to at most `max_width` terminal columns, ending in "..." when
    /// shortened. Counts display width, so multi-byte and wide characters are safe.
    pub fn truncate_hostname(hostname: &str, max_width: usize) -> String {
        use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

        if hostname.width() <= max_width {
            return hostname.to_string();
        }
        let budget = max_width.saturating_sub(3); // Reserve space for "..."
        let mut used = 0;
        let mut truncated: String = hostname
            .chars()
            .take_while(|c| {
                used += c.width().unwrap_or(0);
                used <= budget
            })
            .collect();
        truncated.push_str(&"..."[..max_width.min(3)]);
        truncated
    }
}

/// Layout and sizing utilities
pub mod layout {
    /// Calculate constrained width with min/max bounds
    pub fn constrain_width(width: u16, min_width: u16, max_width: u16) -> u16 {
        width.saturating_sub(4).max(min_width).min(max_width)
    }

    /// Calculate popup dimensions constrained by area
    pub fn calculate_popup_dimensions(
        area_width: u16,
        area_height: u16,
        preferred_width: u16,
        preferred_height: u16,
    ) -> (u16, u16) {
        let width = preferred_width.min(area_width.saturating_sub(4));
        let height = preferred_height.min(area_height.saturating_sub(4));
        (width, height)
    }

    /// Calculate centered position for popup
    pub fn center_popup(
        area_width: u16,
        area_height: u16,
        popup_width: u16,
        popup_height: u16,
    ) -> (u16, u16) {
        let x = area_width.saturating_sub(popup_width) / 2;
        let y = area_height.saturating_sub(popup_height) / 2;
        (x, y)
    }
}

/// Visualization utilities
pub mod visualization {
    /// Get sparkline character based on ratio (0.0 to 1.0)
    pub fn get_sparkline_char(ratio: f64) -> char {
        let level = (super::math::clamp_ratio(ratio) * 8.0).round() as usize;
        let chars = ['▁', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        chars[level.min(chars.len() - 1)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_time_utils() {
        let duration = Duration::from_millis(1500);
        assert_eq!(time::duration_to_ms_f64(duration), 1500.0);
        assert_eq!(time::duration_to_ms_u64(duration), 1500);
        assert_eq!(time::format_duration_ms(duration), "1500.0");
    }

    #[test]
    fn test_math_utils() {
        assert_eq!(math::clamp_f64(1.5, 0.0, 1.0), 1.0);
        assert_eq!(math::safe_array_index(0.5, 10), 5); // (0.5 * 9).round() = 4.5.round() = 5
    }

    #[test]
    fn test_network_utils() {
        use std::net::{IpAddr, Ipv4Addr};
        let addr = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(network::format_optional_ip(addr), "192.168.1.1");
        assert_eq!(network::format_optional_ip(None), "???");

        let long_hostname = "very-long-hostname-that-should-be-truncated";
        assert_eq!(
            network::truncate_hostname(long_hostname, 20),
            "very-long-hostnam..."
        );
        // Multi-byte names must not panic and must respect display width
        assert_eq!(
            network::truncate_hostname("rüter-ünïcode.example", 8),
            "rüter..."
        );
        assert_eq!(
            network::truncate_hostname("路由器.例子.中国", 9),
            "路由器..."
        );
        assert_eq!(network::truncate_hostname("short", 20), "short");
    }

    #[test]
    fn test_layout_utils() {
        assert_eq!(layout::constrain_width(100, 20, 60), 60);
        assert_eq!(layout::constrain_width(10, 20, 60), 20);

        let (width, height) = layout::calculate_popup_dimensions(100, 50, 80, 40);
        assert_eq!(width, 80);
        assert_eq!(height, 40);
    }

    #[test]
    fn test_visualization_utils() {
        // Test basic sparkline character functionality
        assert_eq!(visualization::get_sparkline_char(0.0), '▁');
        assert_eq!(visualization::get_sparkline_char(0.5), '▄'); // Middle character
        assert_eq!(visualization::get_sparkline_char(1.0), '█'); // Max character
        assert_eq!(visualization::get_sparkline_char(1.5), '█'); // Should clamp to 1.0
        assert_eq!(visualization::get_sparkline_char(-0.1), '▁'); // Should clamp to 0.0
    }

    #[test]
    fn test_additional_math_utils() {
        // Test clamp_ratio
        assert_eq!(math::clamp_ratio(0.5), 0.5);
        assert_eq!(math::clamp_ratio(-0.1), 0.0);
        assert_eq!(math::clamp_ratio(1.5), 1.0);

        assert_eq!(math::max_with_minimum(5, 10), 10);
        assert_eq!(math::max_with_minimum(15, 10), 15);
    }

    #[test]
    fn test_high_precision_time_utils() {
        // Test microsecond conversion
        let duration = Duration::from_micros(1500);
        assert_eq!(time::duration_to_us_f64(duration), 1500.0);

        // Test high-precision formatting
        let small_duration = Duration::from_micros(500);
        assert_eq!(time::format_duration_us(small_duration), "500.0μs");

        let large_duration = Duration::from_millis(1500);
        assert_eq!(time::format_duration_us(large_duration), "1500.0ms");
    }

    #[test]
    fn test_timing_jitter_calculation() {
        let current = Duration::from_millis(120);
        let previous = Duration::from_millis(100);
        let jitter = time::calculate_timing_jitter(current, previous);
        assert_eq!(jitter, Duration::from_millis(20));

        // Test reverse order
        let jitter_reverse = time::calculate_timing_jitter(previous, current);
        assert_eq!(jitter_reverse, Duration::from_millis(20));
    }

    #[test]
    fn test_timing_ema() {
        let first = Duration::from_millis(100);
        let ema1 = time::calculate_timing_ema(first, None, 0.1);
        assert_eq!(ema1, first);

        let second = Duration::from_millis(200);
        let ema2 = time::calculate_timing_ema(second, Some(ema1), 0.1);
        // EMA should be closer to first value due to low alpha
        assert!(ema2 > first && ema2 < second);
    }

    #[test]
    fn test_timing_precision_improvements() {
        // Test high-precision microsecond timing
        let ultra_fast = Duration::from_micros(100); // 100μs - typical for localhost
        assert_eq!(time::duration_to_us_f64(ultra_fast), 100.0);
        assert_eq!(time::format_duration_us(ultra_fast), "100.0μs");

        let sub_ms = Duration::from_micros(750); // 0.75ms
        assert_eq!(time::format_duration_us(sub_ms), "750.0μs");

        let ms_range = Duration::from_millis(5); // 5ms - show ms formatting
        assert_eq!(time::format_duration_us(ms_range), "5.0ms");

        // Test that we no longer have 10ms quantization
        let precise_timing = Duration::from_nanos(123_456); // 123.456μs
        assert_eq!(time::duration_to_us_f64(precise_timing), 123.456);

        // Verify sub-millisecond precision detection
        assert!(time::duration_to_us_f64(ultra_fast) < 1000.0); // Should use μs format
        assert!(time::duration_to_us_f64(ms_range) >= 1000.0); // Should use ms format
    }
}
