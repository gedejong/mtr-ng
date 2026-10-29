//! Writes shell completions from the CLI definition into `install/completions/`,
//! so they can't drift from the code: `cargo run --example completions`.
//! `tests/completions.rs` fails when the checked-in ones are stale.

use clap::CommandFactory;
use clap_complete::{generate, Shell};
use mtr_ng::Args;
use std::path::Path;

/// Each shell's file, named as the shell looks for it.
pub const FILES: [(Shell, &str); 3] = [
    (Shell::Bash, "mtr-ng.bash"),
    (Shell::Zsh, "_mtr-ng"),
    (Shell::Fish, "mtr-ng.fish"),
];

pub fn render(shell: Shell) -> Vec<u8> {
    let mut out = Vec::new();
    generate(shell, &mut Args::command(), "mtr-ng", &mut out);
    out
}

fn main() -> std::io::Result<()> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("install/completions");
    std::fs::create_dir_all(&dir)?;
    for (shell, name) in FILES {
        std::fs::write(dir.join(name), render(shell))?;
    }
    Ok(())
}
