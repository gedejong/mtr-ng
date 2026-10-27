# mtr-ng

Rust reimplementation of `mtr` (traceroute + ping) with a ratatui TUI and a `--report` mode. One crate: library in `src/lib.rs`, binary in `src/main.rs`.

## Layout
- `src/session.rs`: `MtrSession`. Target resolution, probe scheduling, sequence table, matching replies to hops, simulation mode.
- `src/probe.rs`: `ProbeEngine`. Raw sockets, packet building and parsing.
- `src/hop_stats.rs`: per-hop statistics (loss, RTT, jitter, EMA, multipath `AlternatePath`).
- `src/report.rs`: non-interactive `--report` output.
- `src/ui/`: TUI. `main.rs` event/render loop, `widgets.rs` table and column selector, `visualization.rs` sparklines and colours, `state.rs`, `events.rs`.
- `src/args.rs`: clap CLI. `src/utils.rs`: math, formatting and timing helpers.

## Commands
- CI gate, run before every commit: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
- Smoke test without privileges: `cargo run -- --simulate --count 3 --report example.com`
- Real network on macOS needs no root (unprivileged ICMP): `./target/debug/mtr-ng -r -c 3 -n 1.1.1.1`. Use it to verify probing changes.
- TUI changes: verify with the `tui-check` skill (headless tmux capture; `MTR_REAL=1` for real probes).
- Linux needs root or CAP_NET_RAW (`scripts/setup-capabilities.sh`). Claude cannot sudo; ask the user to run `! sudo ...`.
- `just --list` shows convenience recipes.

## Gotchas
- Never fall back to simulated data. If probing is impossible, return an error; simulation only with `--simulate`.
- Logging: report mode writes warnings to stderr; interactive mode logs only with `--log-file PATH`. `RUST_LOG` sets the level.
- `scripts/bump-version.sh` edits `Cargo.toml`, commits, pushes and tags a release. Only run it when the user asks.
- `.cursor/*.md`, `DEVELOPMENT.md`, `TESTING.md` hold older long-form notes; the code wins where they disagree.
- Open work and known bugs: `BACKLOG.md`.
