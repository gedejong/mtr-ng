# Changelog

Releases up to 0.3.0 are at [edejong-dbc/mtr-ng](https://github.com/edejong-dbc/mtr-ng/releases).

## 0.4.0 - 2026-10-28

mtr-ng now lives at [gedejong/mtr-ng](https://github.com/gedejong/mtr-ng), and its Homebrew tap at `gedejong/tap`. If you installed from the old tap, switch with `brew uninstall mtr-ng && brew untap edejong-dbc/tap && brew install gedejong/tap/mtr-ng`. The crate on crates.io stays at 0.3.0 for now; `cargo install --locked --git https://github.com/gedejong/mtr-ng` installs this version.

### Measurements you can trust

- No more made-up traces. IPv6 targets and missing privileges used to switch silently to simulated data, and `-P udp` and `-P tcp` silently sent ICMP; all of these now exit with an error. Simulation runs only with `--simulate`.
- Real traces no longer stop at hop 10, and they end at the target.
- On macOS mtr-ng probes without sudo, using unprivileged ICMP sockets. Linux needs root or `CAP_NET_RAW`; `scripts/setup-capabilities.sh` now works with current libcap.
- Report and interactive mode share one probing loop. Replies are matched to their probe and timestamped on arrival, timeouts count as loss, and probes still in flight don't. RTT and loss beyond about 50 ms were wrong before: a 14-hop path to Japan showed about 1005 ms and 33% loss, and now shows 242-266 ms and 0%.
- Avg, StDev and jitter cover every reply, not only the last 100.
- A send error, such as a Wi-Fi blip, counts as a lost probe instead of ending the report.
- Reverse DNS uses the system's resolvers and no longer holds up probing.
- `--report` without `-c` stops after 10 cycles, as mtr does.
- A crash restores the terminal.
- Nonsense values such as `-i 0`, `-c 0` and `--ema-alpha NaN` are rejected. `--timing` is gone; `-q` limits report logging to errors.

### Load balancers and MPLS

- `--flows N` probes N flows in turn. Every probe of a flow carries the same ICMP checksum and identifier (Paris traceroute), so a per-flow load balancer keeps it on one path, and the flows together show the paths it spreads traffic over.
- `f` shows the flow matrix: per hop, a letter for the address each flow met. `--report` prints it after the table.
- `--dot` prints the routes as a Graphviz graph.
- `-e` (or `e` while running) shows MPLS label stacks from ICMP extensions.
- `Enter` opens a hop's details: every responder with its share, RTTs, the routers on the way back and its flows, plus RTT percentiles and a histogram.

### Where is it

- `-g` adds a Location and a Dist column from a local geolocation database, DB-IP City Lite or MaxMind GeoLite2 City (`--geoip-mmdb` picks the file). Lookups never leave the machine.
- The light-speed check: a reply can't come from farther than light in fibre travels in its round trip, about 100 km per ms. Locations the best RTT rules out are marked `!`. The Reach column shows that distance.
- Router names: Arelion, Cogent, Hurricane Electric and Lumen name their routers after the city they stand in, and those cities go before the database, which often places routers at their owner's head office.
- `m` draws the located hops on a world map, joined along great circles.
- Distances are measured from `--origin LAT,LON`, or else from the first hop placed in a city.

### Interface

- The host column fits the longest name, the graph takes the remaining width with the newest sample on the right, and columns that don't fit are dropped whole instead of squeezed.
- `o` opens the column chooser in the current order; Shift+↑/↓ or K/J reorder columns.
- The table scrolls with the selection, `Esc` closes overlays, and the status line shows the target's loss and when the trace is done.
- Colour depth is detected from `COLORTERM` and `TERM` and cycles with `c`; `NO_COLOR` and `TERM=dumb` are respected.
- The sparkline scale works on LANs too.
- The sixel renderer is gone.

### Output

- `-j`/`--json` uses mtr's field names, plus a `geo` object per hop with `-g`.
- `-C`/`--csv`.
- `--geojson` writes the located hops and the path between them as GeoJSON (RFC 7946) for map tools such as geojson.io and QGIS.
- `-z` looks up AS numbers via Team Cymru's DNS service, for public addresses only.
- Report columns fit their data, and `--fields` picks them.
- The man page is generated from the code and installed by Homebrew and the `.deb`.

### Install

- Binaries for macOS (x86_64, arm64) and Linux (x86_64 and arm64 with glibc 2.35 or newer, x86_64 static musl), with checksums, a shell installer, a Homebrew formula, and `.deb` packages for amd64 and arm64.
- Licensed MIT OR Apache-2.0; both texts ship with every package.
- Windows builds are no longer shipped: mtr-ng compiles there but can't probe yet.
- Building needs Rust 1.89 or newer.
