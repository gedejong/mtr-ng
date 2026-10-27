# mtr-ng

Rust reimplementation of `mtr` (traceroute + ping) with a ratatui TUI and a `--report` mode. One crate: library in `src/lib.rs`, binary in `src/main.rs`.

## Layout
- `src/tracer.rs`: the one probing loop (report and TUI). Sends a round per interval, matches replies by seq, expires probes after 2 s, runs reverse DNS off the probe path. Tested against a `FakeProber`.
- `src/probe.rs`: `Prober` trait and `ProbeEngine`. ICMP sockets (unprivileged datagram on macOS, raw elsewhere), one receive thread per address family that timestamps replies on arrival, packet building and parsing.
- `src/session.rs`: `MtrSession`. Target resolution, the hop table, applying replies and timeouts to hops, the reverse DNS cache, simulation mode.
- `src/hop_stats.rs`: per-hop statistics (loss excluding probes in flight, RTT, Welford stddev, jitter, EMA) and per-address `ResponderStats` for multipath.
- `src/report.rs`: `--report` text (golden-tested `format_table`), `--json` (mtr field names) and `--csv`.
- `src/columns.rs`: header, chooser label and value of every `Column`, shared by TUI, report and man page. `src/asn.rs`: `-z` lookups via Team Cymru DNS, public addresses only.
- `src/ui/`: TUI. `main.rs` event loop (input thread, coalesced redraws from a session snapshot), `render.rs` all drawing incl. column layout, `events.rs` keymap tables (also generate help and man page), `state.rs` modes and column chooser, `visualization.rs` RTT scale, sparkline cells, colours.
- `src/args.rs`: clap CLI plus `validate()` for cross-flag rules. `src/utils.rs`: small formatting helpers.
- `examples/man.rs` generates `install/mtr-ng.1`; `tests/man_page.rs` fails when it is stale. After changing flags, keys or columns: `cargo run --example man > install/mtr-ng.1`.

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
- `DEVELOPMENT.md`, `TESTING.md` hold older long-form notes; the code wins where they disagree.
- MSRV is 1.89 (`rust-version`), set by hickory-resolver's dependencies; CI checks it.
- Open work and known bugs: `BACKLOG.md`.
