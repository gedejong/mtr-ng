#[path = "../examples/man.rs"]
#[allow(dead_code)]
mod man;

/// The title line carries the version, which changes with every release.
fn without_title(page: &str) -> Vec<&str> {
    page.lines().filter(|l| !l.starts_with(".TH")).collect()
}

#[test]
fn checked_in_man_page_is_current() {
    let mut generated = Vec::new();
    man::render(&mut generated).unwrap();
    let generated = String::from_utf8(generated).unwrap();
    assert!(
        without_title(&generated) == without_title(include_str!("../install/mtr-ng.1")),
        "install/mtr-ng.1 is stale: run `cargo run --example man > install/mtr-ng.1`"
    );
}
