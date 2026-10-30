# Development Guide

This guide covers development workflows, how probing works, CI, and releases for mtr-ng. Where it disagrees with the code, the code wins.

## Prerequisites

- Rust 1.89 or newer (the MSRV, set as `rust-version` in `Cargo.toml`) with the `rustfmt` and `clippy` components
- Git
- Pre-commit (optional)
- `just` (optional): `just --list` shows the convenience recipes

## Development Setup

1. **Clone:**
   ```bash
   git clone https://github.com/gedejong/mtr-ng.git
   cd mtr-ng
   ```

2. **Install pre-commit hooks (optional):**
   ```bash
   pip install pre-commit
   pre-commit install
   ```

3. **Build and test:**
   ```bash
   cargo build --locked
   cargo test --locked
   ```

## Running Against the Real Network

- **macOS** needs no privileges: mtr-ng uses an unprivileged ICMP datagram socket.
  ```bash
  ./target/debug/mtr-ng -r -c 3 -n 1.1.1.1
  ```
- **Linux** probes ICMP without privilege through a ping socket where `net.ipv4.ping_group_range` includes your group (systemd 243 and newer include every group). Raw sockets, for MPLS labels and `-P udp`, need root or `CAP_NET_RAW`. Build as yourself and run only the binary as root:
  ```bash
  cargo build
  sudo ./target/debug/mtr-ng -r -c 3 -n 1.1.1.1
  ```
  Never `sudo cargo run`: it leaves root-owned files in `target/` and `~/.cargo`. To skip sudo, install a copy and give it the capability with `scripts/setup-capabilities.sh`. A capability on `target/debug/mtr-ng` is wiped by the next build.
- **Simulation** (`--simulate`) produces fake data for UI work and demos. mtr-ng never falls back to it on its own: if it can't probe, it exits with an error.

ICMP and UDP over IPv4 and IPv6 are implemented; `-P tcp` exits with an error. `just netlab` traces Linux paths from a Mac, with ECMP, NAT and IPv6 (TESTING.md).

## How a Trace Works

mtr-ng follows the original mtr algorithm.

- **Probing.** Every interval (`-i`, in milliseconds) it sends one ICMP echo request per TTL (with `-P udp` a UDP datagram to port 33434 + flow), from 1 up to the end of the path. Routers along the way answer with Time Exceeded, the target with Echo Reply (or Port Unreachable for UDP), and some hops with Destination Unreachable.
- **Matching.** An ICMP error quotes the IP header and first 8 bytes of the probe that caused it, so the quoted echo id and sequence number identify our probe; for UDP the quoted ports and checksum do, since two payload bytes make the checksum equal the sequence number. The id or source port filters out other processes' probes; the sequence number maps the reply to its hop and probe. A probe with no reply after the probe timeout (2 s) counts as lost.
- **RTT.** Each probe records its own send time, and a dedicated receive thread timestamps each reply the moment it arrives, so RTT = arrival − send for that probe. Never derive RTT from loop or batch timing: an early version measured from the start of a batch and reported RTTs about 8x too high.
- **Parsing.** Raw sockets, and datagram ICMP sockets on macOS, deliver the IP header too, so one parser serves both. It reads the header length from the IHL field and validates every length before indexing. A Linux ping socket delivers no headers: the kernel matches errors to the socket itself and queues them on its error queue with the router's address (`src/probe/ping_socket.rs`).
- **Reverse DNS** runs off the probing path, so slow lookups never delay probes. `-n` turns it off.
- **Report and TUI** share the same probing loop; report mode stops after `--count` rounds (10 by default).

`CLAUDE.md` has a map of the source files.

## Debugging

- Report mode logs warnings to stderr. Interactive mode owns the terminal, so it logs only with `--log-file PATH`. `RUST_LOG` sets the level in both:
  ```bash
  RUST_LOG=mtr_ng=debug ./target/debug/mtr-ng -r -c 3 -n 1.1.1.1
  ./target/debug/mtr-ng --log-file /tmp/mtr-ng.log 1.1.1.1
  ```
- Compare with the original mtr: `just compare <target>` (builds as you; sudo only where needed).
- Symptoms and usual causes:
  - RTTs close to the probe interval: replies matched to the wrong probe, or a timestamp taken in the wrong place.
  - No replies at all: privileges (see above), or the parser rejecting valid packets.
  - Wrong hops: TTL not set per probe, or replies accepted without checking the quoted id and sequence.
- TUI changes: the `tui-check` skill runs the TUI headless in tmux and captures the screen as text.

## Quality Assurance

The CI gate, run before every commit:

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

(`just ci` runs the same.) Ground rules:

- Fix clippy findings instead of silencing them with `#[allow]`.
- Delete dead code rather than hiding the warning.
- Name protocol numbers and magic constants.
- Validate every field of a received packet before using it.

### Coverage

```bash
cargo install cargo-llvm-cov
cargo llvm-cov --locked --lcov --output-path lcov.info
```

### Dependencies

CI runs `cargo deny check` against `deny.toml`: licences, bans and sources must pass, and advisories are reported without failing the build. Locally:

```bash
cargo install cargo-deny
cargo deny check            # or: just audit
```

## CI (.github/workflows/ci.yml)

Runs on pushes to `master` and on pull requests:

- **Lint**: `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` on stable.
- **Test**: `cargo test --locked` on ubuntu-latest (x86_64), ubuntu-24.04-arm, macos-15 and windows-latest.
- **Smoke test**: real probes to 127.0.0.1 (`mtr-ng -r -c 2 -n 127.0.0.1`), with sudo on Linux, without on macOS, and through the ICMP API on Windows. It fails unless hop 1 answers.
- **FreeBSD**: `cargo check --target x86_64-unknown-freebsd` (type-check only).
- **MSRV**: `cargo check --all-targets` with the toolchain named by `rust-version` in `Cargo.toml`.
- **cargo-deny**: advisories (allowed to fail), and bans, licences and sources.

Every job uses `--locked`, so `Cargo.lock` must be committed and current.

## Releases

`scripts/bump-version.sh X.Y.Z` runs the gate, bumps and tags; the tag starts the dist workflow in `.github/workflows/release.yml`. See DISTRIBUTION.md, "Cutting a release".

## Adding New Features

1. Create a branch: `git checkout -b feature/new-feature`
2. Implement with tests
3. Run the CI gate
4. Open a pull request and wait for CI
