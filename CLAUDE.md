# mtr-ng

Rust reimplementation of `mtr` (traceroute + ping) with a ratatui TUI and a `--report` mode. One crate: library in `src/lib.rs`, binary in `src/main.rs`.

## Layout
- `src/tracer.rs`: the one probing loop (report and TUI). Sends a round per interval, each round in one flow (round mod `--flows`), matches replies by seq, expires probes after 2 s, runs reverse DNS off the probe path. With `--pmtu` one more probe per round for `src/pmtu.rs`, the path MTU search (Fragmentation Needed / Packet Too Big, bisection for black holes). Tested against a `FakeProber`.
- `src/probe.rs`: `Prober` trait and `ProbeEngine`. ICMP echo requests, or with `-P udp` datagrams to port 33434 + flow. One ICMP or ICMPv6 socket for the target's family reads every answer (unprivileged datagram on macOS, raw elsewhere); a receive thread timestamps replies on arrival. Packet building and parsing. Paris flows: ICMP per flow a fixed checksum and identifier, two payload bytes absorb the seq; UDP fixed ports, the seq is the UDP checksum; IPv6 adds a flow label per flow. `routable()` asks the routing table. `src/probe/ping_socket.rs`: Linux without privilege, a ping socket whose errors come from `MSG_ERRQUEUE` (no UDP, no MPLS). `src/probe/recvmsg.rs`: `recvmsg` with control messages (TTL, IPv6 hop limit, the error queue), socket options, interface MTUs. `src/icmp_ext.rs`: RFC 4884 extensions (both families), MPLS label stacks.
- `src/simulate.rs`: `SimProber`, the `--simulate` path behind the `Prober` trait, so simulation runs through the real tracer.
- `src/session.rs`: `MtrSession`. Target resolution, `open_probe_engine` (real or simulated), the hop table, applying replies and timeouts to hops, the reverse DNS cache.
- `src/hop_stats.rs`: per-hop statistics (loss excluding probes in flight, RTT, Welford stddev, jitter, EMA), a ring of the last 600 `ProbeRecord`s, per-flow `FlowStats`, per-address `ResponderStats` (reply TTL, MPLS).
- `src/topology.rs`: routes per flow: responder letters, the flow matrix, route-graph edges, DOT.
- `src/geo/`: `mod.rs` the `GeoSource` trait, MMDB reader (memory-mapped, attribution per database), discovery, great-circle distance and arcs, the light-speed `assess`; tests build tiny MMDB files by hand, `FakeGeo` for others. `names.rs` reads city codes in router names per network (`name_codes.rs`, from their looking glasses); `session.set_hostname` lets them replace the database's location. `session.origin()` picks `--origin` or the first located hop.
- `src/report.rs`: `--report` text (golden-tested `format_table`, flow matrix when `--flows` > 1), `--json` (mtr field names, plus `geo`), `--csv`, `--dot` and `--geojson` (RFC 7946, [lon, lat]).
- `src/columns.rs`: header, chooser label and value of every `Column`, shared by TUI, report and man page. `src/asn.rs`: `-z` lookups via Team Cymru DNS, public addresses only.
- `src/ui/`: TUI. `main.rs` event loop (input thread, coalesced redraws from a session snapshot), `render.rs` all drawing (table, flow matrix, detail pane, overlays, column layout), `map.rs` the world map view (coastlines in `coastline.bin`, written by `examples/coastline.rs` from Natural Earth), `events.rs` keymap tables (also generate help and man page), `state.rs` modes, views, selection and column chooser, `visualization.rs` RTT scale, sparkline and histogram cells, colours.
- `src/args.rs`: clap CLI plus `validate()` for cross-flag rules. `src/utils.rs`: small formatting helpers.
- `examples/man.rs` generates `install/mtr-ng.1`; `tests/man_page.rs` fails when it is stale. After changing flags, keys or columns: `cargo run --example man > install/mtr-ng.1`.

## Commands
- CI gate, run before every commit: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
- Smoke test without privileges: `cargo run -- --simulate --count 3 --report example.com`
- Real network on macOS needs no root (unprivileged ICMP): `./target/debug/mtr-ng -r -c 3 -n 1.1.1.1`. Use it to verify probing changes.
- TUI changes: verify with the `tui-check` skill (headless tmux capture; `MTR_REAL=1` for real probes).
- Linux needs root or CAP_NET_RAW (`scripts/setup-capabilities.sh`) for raw sockets and `-P udp`; ICMP without them uses a ping socket. Claude cannot sudo; ask the user to run `! sudo ...`, or use `just netlab`.
- Linux probing from the Mac, no sudo: `just netlab [--user probe] -- CMD` builds mtr-ng in Docker and runs CMD on a namespace lab (NAT, ECMP diamond, IPv6, MTU 1400; `target`/`target6`). See TESTING.md.
- `just --list` shows convenience recipes.

## Gotchas
- Never fall back to simulated data. If probing is impossible, return an error; simulation only with `--simulate`.
- Logging: report mode writes warnings to stderr; interactive mode logs only with `--log-file PATH`. `RUST_LOG` sets the level.
- `scripts/bump-version.sh X.Y.Z` bumps, commits, tags and (after a prompt) pushes, which publishes a release. Only run it when the user asks.
- Releases are built by dist from `dist-workspace.toml`. `release.yml` is generated: edit the config and run `dist generate`, never the workflow. `build-deb.yml` and `publish-homebrew.yml` are hand-written jobs it calls; `publish-crates.yml` waits until gedejong owns the crate (BACKLOG C10).
- `DEVELOPMENT.md`, `TESTING.md` hold older long-form notes; the code wins where they disagree.
- MSRV is 1.89 (`rust-version`), set by hickory-resolver's dependencies; CI checks it.
- Open work and known bugs: `BACKLOG.md`.
