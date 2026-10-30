# Testing mtr-ng

Most of mtr-ng can be tested without privileges or a network. DEVELOPMENT.md says how to probe the real network on each platform and what CI runs.

## Tests

```bash
cargo test
```

The unit tests sit next to the code. The probing loop runs against a `FakeProber` (`src/tracer.rs`), packets are parsed from hand-built bytes (`src/probe.rs`, `src/icmp_ext.rs`), the report table is compared with golden output (`src/report.rs`), and the geolocation tests build tiny MMDB files by hand (`src/geo/mod.rs`). `tests/man_page.rs` and `tests/completions.rs` fail when `install/mtr-ng.1` or `install/completions/` are stale; `cargo run --example man > install/mtr-ng.1` and `cargo run --example completions` regenerate them.

The CI gate, run before every commit, is `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`, or `just ci`.

## Simulation

`--simulate` runs the real tracer over a made-up path (`src/simulate.rs`) with loss, a load-balanced diamond (`--flows 2` shows it), MPLS labels, an interface named by RFC 5837 and a NAT, without sockets or privileges. It is for UI work, demos and smoke tests; mtr-ng never falls back to it on its own.

```bash
cargo run -- --simulate -r -c 3 example.com     # a report
cargo run -- --simulate example.com             # the TUI; q quits
```

## The TUI without a terminal

`.claude/skills/tui-check/capture.sh` runs the TUI in a detached tmux session and prints the screen as text, at a given size and after given keys:

```bash
cargo build
.claude/skills/tui-check/capture.sh 120x30 3                # size, seconds to wait
.claude/skills/tui-check/capture.sh 120x30 3 Enter          # then open the detail pane
.claude/skills/tui-check/capture.sh 120x30 3 resize=40x10   # then shrink the window
```

It simulates unless `MTR_REAL=1`; `MTR_TARGET` and `MTR_ARGS` set the target and extra flags.

## Recorded traces

`--record PATH` saves a trace as it runs, and `--replay PATH` plays it back without sending anything. A trace from a network you can't reach again, or one attached to a bug report, can then be watched in the TUI or turned into any report format:

```bash
./target/debug/mtr-ng -r -c 10 --record trace.ndjson 1.1.1.1
./target/debug/mtr-ng --replay trace.ndjson              # the TUI, at the recorded pace
./target/debug/mtr-ng --replay trace.ndjson -r --json    # the report, at once
```

## The real network

macOS probes without privileges, and so does Linux for ICMP where ping sockets are allowed; UDP probes and MPLS labels need root or `CAP_NET_RAW` on Linux. DEVELOPMENT.md has the details. `just compare TARGET` runs mtr-ng and mtr side by side.

```bash
cargo build && ./target/debug/mtr-ng -r -c 3 -n 1.1.1.1
```

## Linux network lab (Docker)

`scripts/netlab/run.sh`, or `just netlab`, tests Linux probing from a Mac. It builds mtr-ng for Linux inside Docker (colima or Docker Desktop, no sudo on the host) and runs a command on a small routed network of namespaces in one privileged container. The first run pulls `rust:1.89-bookworm` and compiles for a few minutes. The cargo registry and target dir are Docker volumes (`mtr-ng-netlab-cargo`, `mtr-ng-netlab-target`), so later runs rebuild incrementally and nothing lands in `./target`. Every run starts a fresh lab, and the command's exit status is passed through.

```
client ── gw ── r1 ──┬── r2a ──┬── r3 ══ target   (target6 for IPv6)
          NAT        └── r2b ──┘   MTU 1400
```

- Link N is `10.0.N.0/24` and `fd00:N::/64`. The hop reached over link N answers from `10.0.N.1` (`fd00:N::1`); r3 always answers from `10.0.5.1`. Hops sit at 2, 12, 16 (r2a) or 20 (r2b), 20/24 and 40/44 ms. On colima an idle VM fires timers late, so spaced probes read 10–20% higher; `--spin` keeps the VM's CPUs busy while the command runs and brings them back to within half a millisecond.
- gw masquerades IPv4 like a home router; IPv6 is routed.
- r1 has a multipath route over r2a and r2b with hash policy 1 (the 5-tuple). UDP probes with different ports split there. ICMP echo never does, because no Linux hash policy looks at the ICMP identifier or checksum.
- UDP checksums are computed in software on every link. veth leaves them to an offload that never happens, routers would quote the unfinished sum, and mtr-ng, which carries the sequence number in the checksum, couldn't match the error. The same happens on loopback, so UDP probes to the machine itself go unanswered.
- The r3–target link has MTU 1400, so `tracepath` shows `pmtu 1400` and DF probes get Frag Needed or Packet Too Big.
- ICMP error rate limits are off. Names (`target`, `target6`, `gw`, `r1`, ...) come from the client's `/etc/hosts` and a local dnsmasq, so reverse lookups answer at once.

```bash
just netlab -- mtr-ng -r -c 3 -n target                  # as root
just netlab --user probe -- ping -c 1 target              # works through a ping socket
just netlab --user probe -- mtr-ng -r -c 3 -n target6    # so does mtr-ng, IPv6 too
just netlab --user probe --no-ping-group -- ping -c 1 target   # ping sockets disallowed: fails
just netlab --silent r3 -- traceroute -n target           # hop 4 shows * * *
just netlab --spin -- mtr-ng -r -c 10 -n target           # accurate RTTs
just netlab --icmp-ecmp -- mtr-ng -r -c 8 -n --flows 4 target  # ICMP flows split at hop 3
just netlab -- mtr-ng -P udp -r -c 8 -n --flows 8 target  # so do UDP flows, by port
just netlab --hash-policy 0 -- mtr-ng -r -c 8 -n --flows 8 target6  # and IPv6, by flow label
just netlab --wide -- mtr-ng -P udp -r -n --flows auto target  # 3 branches at hop 3, 16 flows
just netlab -- mtr-ng --pmtu -r -c 5 -n target           # MTU 1500 → 1400 past hop 4
just netlab --black-hole -- mtr-ng --pmtu -i 300 -r -c 150 -n target  # found by halving, past hop 4
just netlab --stateless-nat -- mtr-ng -P udp -r -c 3 -n target  # NAT before hop 2, from 10.0.2.2
just netlab --shell                                       # shell in the client namespace
```

`--user probe` runs the command as a user without capabilities. `--no-ping-group` sets `net.ipv4.ping_group_range` to `1 0` instead of `0 2147483647`. `--silent ROUTER` drops that router's Time Exceeded messages (IPv4 and IPv6), `--black-hole` its Fragmentation Needed and Packet Too Big at r3, and `--loss PCT` loses packets from gw to r1. `--hash-policy N` sets r1's multipath hash policy; the IPv6 flow label counts in policies 0, 2 and 3, not in 1. `--icmp-ecmp` makes r1 send echo requests with an even ICMP checksum to r2a and odd ones to r2b, as routers that hash the first transport word do. `--wide` adds a third branch, r2c (10.0.8.1), for `--flows auto`. `--stateless-nat` makes gw translate addresses without connection tracking, so routers' ICMP errors keep quoting the probe from 10.0.2.2, as NATs that don't translate the quote back do; gw's usual NAT is Linux's, which translates it back. `scripts/netlab/run.sh --help` lists the options, and `scripts/netlab/lab.sh` explains each sysctl and route.

## Other platforms

CI tests on Linux (x86_64 and arm64), macOS and Windows, and type-checks FreeBSD. Locally, after `rustup target add`, `cargo clippy --target x86_64-unknown-freebsd --all-targets -- -D warnings` (or `x86_64-pc-windows-gnu`) checks the code behind `cfg` for that platform. ring, which `--ipmap` uses for TLS, compiles C in its build script, so this needs a C compiler for the target, or `CC_<target>` and `AR_<target>` pointing at stand-ins that create the files they are asked for: clippy never links.
