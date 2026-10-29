#[path = "../examples/completions.rs"]
#[allow(dead_code)]
mod completions;

#[test]
fn checked_in_completions_are_current() {
    for (shell, name) in completions::FILES {
        let path = format!("{}/install/completions/{name}", env!("CARGO_MANIFEST_DIR"));
        // Git on Windows may check the files out with CRLF line endings
        let checked_in = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        let generated = String::from_utf8(completions::render(shell)).unwrap();
        assert!(
            generated == checked_in,
            "{path} is stale: run `cargo run --example completions`"
        );
    }
}
