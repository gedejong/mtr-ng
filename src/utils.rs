// Individual modules import what they need

/// Time conversion utilities
pub mod time {
    use std::time::Duration;

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

    /// Clamp ratio to 0.0-1.0 range (common pattern)
    pub fn clamp_ratio(ratio: f64) -> f64 {
        ratio.clamp(0.0, 1.0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_math_utils() {
        assert_eq!(math::clamp_f64(1.5, 0.0, 1.0), 1.0);
        assert_eq!(math::clamp_ratio(0.5), 0.5);
        assert_eq!(math::clamp_ratio(-0.1), 0.0);
        assert_eq!(math::clamp_ratio(1.5), 1.0);
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
}
