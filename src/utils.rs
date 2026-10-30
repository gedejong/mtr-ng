/// Text layout
pub mod text {
    use unicode_width::UnicodeWidthStr;

    /// `value` with backslashes, quotes and line breaks escaped, as Prometheus
    /// label values and Graphviz strings want them.
    pub fn escape(value: &str) -> String {
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    }

    /// `text` cut to at most `max_width` terminal columns, ending in "..." when
    /// shortened. Counts display width, so multi-byte and wide characters are safe.
    pub fn truncate(text: &str, max_width: usize) -> String {
        use unicode_width::UnicodeWidthChar;

        if text.width() <= max_width {
            return text.to_string();
        }
        let budget = max_width.saturating_sub(3); // Reserve space for "..."
        let mut used = 0;
        let mut truncated: String = text
            .chars()
            .take_while(|c| {
                used += c.width().unwrap_or(0);
                used <= budget
            })
            .collect();
        truncated.push_str(&"..."[..max_width.min(3)]);
        truncated
    }

    /// `items` joined by `sep`, wrapped between items to lines of at most `width`
    /// columns. An item wider than that gets a line to itself.
    pub fn pack(items: &[String], sep: &str, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for item in items {
            match lines.last_mut() {
                Some(line) if line.width() + sep.width() + item.width() <= width => {
                    line.push_str(sep);
                    line.push_str(item);
                }
                _ => lines.push(item.clone()),
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::text::{escape, pack, truncate};

    #[test]
    fn truncation_counts_display_columns() {
        let long = "very-long-hostname-that-should-be-truncated";
        assert_eq!(truncate(long, 20), "very-long-hostnam...");
        // Multi-byte names must not panic and must respect display width
        assert_eq!(truncate("rüter-ünïcode.example", 8), "rüter...");
        assert_eq!(truncate("路由器.例子.中国", 9), "路由器...");
        assert_eq!(truncate("short", 20), "short");
    }

    #[test]
    fn packed_lines_break_between_items() {
        let key = ["●n hop n", "◆ --origin", "not placed: 1, 3-9"].map(String::from);
        assert_eq!(
            pack(&key, " · ", 80),
            ["●n hop n · ◆ --origin · not placed: 1, 3-9"]
        );
        assert_eq!(
            pack(&key, " · ", 30),
            ["●n hop n · ◆ --origin", "not placed: 1, 3-9"],
            "the ● and ◆ are one column each"
        );
        assert_eq!(pack(&key, " · ", 5), key, "too wide: one per line");
    }

    #[test]
    fn escapes_backslashes_quotes_and_line_breaks() {
        assert_eq!(escape("a\\b \"c\"\nd"), "a\\\\b \\\"c\\\"\\nd");
    }
}
