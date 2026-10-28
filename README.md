# MTR-NG - Modern My Traceroute in Rust

A modern, feature-rich implementation of My Traceroute (MTR) built in Rust, offering real-time network path visualization with enhanced sparklines, jitter analysis, and advanced column customization.

## Features

### Core Network Analysis
- **Real-time Path Tracing**: Live network path discovery and monitoring
- **RTT Statistics**: Min/Max/Average/EMA (Exponential Moving Average) calculations  
- **Packet Loss Detection**: Accurate loss percentage tracking per hop
- **Jitter Analysis**: Last jitter and average jitter measurements for network stability

### Advanced Visualization
- **Unicode Sparklines**: Beautiful real-time RTT history visualization (`▁▂▂▆█▄▂▃▂▁`)
- **Color-coded Metrics**: Green (good) → Yellow (warning) → Red (problematic)
- **Customizable Columns**: Select exactly which metrics to display
- **Scalable Display**: Auto-scaling sparklines with manual override options

### Load Balancers and MPLS
- **Stable flows**: `--flows N` probes N flows in turn. All probes of one flow carry the same ICMP checksum and identifier (Paris traceroute), so a per-flow load balancer keeps each on one path, and the flows together show the paths it spreads traffic over
- **Flow matrix**: `f` shows, per hop, a letter for the address each flow met; `--report` prints the same matrix after the table
- **Route graph**: `--dot` prints the routes as Graphviz, with silent hops as dashed edges
- **MPLS labels**: `-e` (or `e` while running) shows the label stacks routers report in ICMP extensions (RFC 4950)
- **Hop details**: `Enter` opens every responder of a hop with its share, RTTs, routers on the way back and flows, plus RTT percentiles and a histogram

### Where Is It
- **Locations**: `-g` adds a Location and a Dist column, from a local geolocation database (DB-IP City Lite or MaxMind GeoLite2 City). Lookups never leave the machine; private addresses are never looked up
- **Light-speed check**: a reply can't come from farther than light in fibre travels in its round trip, about 100 km per ms. A location the hop's best RTT rules out is marked `!`, which catches routers placed at their owner's head office and anycast addresses placed at their registrant's
- **Router names**: backbones name routers after their city, in codes of their own (`adm-b3-link.ip.twelve99.net` is Arelion in Amsterdam). For Arelion, Cogent, Hurricane Electric and Lumen that city goes before the database, which often places routers at a head office
- **Map**: `m` draws the located hops on a world map, joined along great circles. Locations the RTT rules out stay off it
- **Origin**: distances are measured from `--origin LAT,LON`, or else from the first hop placed in a city

### Column Selection System
- **Flexible Fields**: Choose from 14 columns
  - `hop`, `host`, `asn`, `loss`, `sent`, `last`, `avg`, `ema`
  - `jitter`, `jitter-avg`, `best`, `worst`, `stdev`, `graph`
- **Quick Presets**: `--show-all` for complete metrics, custom combinations via `--fields`
- **Interactive Chooser**: `o` opens a column chooser to show, hide and reorder columns
- **Report Mode**: Same column selection works for both interactive and report output

### Interface Modes
- **Interactive UI**: Real-time terminal interface with keyboard controls
- **Report Mode**: Clean output for automation and scripting
- **Colour Depth**: Detected from `COLORTERM`/`TERM` (none, 16, 256 or true colour); `c` cycles through them

### Performance & Compatibility
- **Efficient Async**: Built with Tokio for high-performance networking
- **Platforms**: macOS (no sudo needed) and Linux (root or `CAP_NET_RAW`). Windows builds but can't probe yet; FreeBSD is only type-checked.
- **IPv4 only**: IPv6 targets and `-P udp`/`-P tcp` exit with an error for now. Reverse DNS for hop names (`-n` turns it off).

## Installation

Each [release](https://github.com/edejong-dbc/mtr-ng/releases) after 0.3.0 has binaries for macOS and Linux, x86_64 and ARM, with checksums.

#### Shell installer (macOS, Linux)
```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/edejong-dbc/mtr-ng/releases/latest/download/mtr-ng-installer.sh | sh
```

#### Homebrew (macOS, Linux)
```bash
brew install edejong-dbc/tap/mtr-ng
```

#### Debian/Ubuntu (amd64, arm64)
Download the `.deb` from the release, then:
```bash
sudo dpkg -i mtr-ng_*.deb
```

#### Cargo
```bash
cargo install --locked mtr-ng
```

### From Source
```bash
git clone https://github.com/edejong-dbc/mtr-ng
cd mtr-ng
cargo build --release --locked
sudo cp target/release/mtr-ng /usr/local/bin/
sudo cp install/mtr-ng.1 /usr/local/share/man/man1/
```

### Privileges
- **macOS**: runs as a normal user (unprivileged ICMP sockets). No sudo.
- **Linux**: needs raw sockets, so root or `CAP_NET_RAW`. Either run it with `sudo`, or grant the capability to the installed binary once:
  ```bash
  sudo setcap cap_net_raw+ep "$(readlink -f "$(which mtr-ng)")"
  # or, with checks and a test run:
  scripts/setup-capabilities.sh
  ```
  The capability is narrower than sudo (raw sockets only, not full root). Reinstalling replaces the file and drops it, so set it again afterwards.
- **Windows**: compiles, but probing doesn't work yet.

mtr-ng never falls back to fake data: if it can't probe, it exits with an error. Use `--simulate` for a demo without network access.

## Usage Examples

### Basic Usage
```bash
# Standard trace with default columns
mtr-ng google.com

# Show all available metrics
mtr-ng google.com --show-all

# Custom column selection
mtr-ng google.com --fields hop,host,loss,last,avg,graph

# Focus on jitter analysis  
mtr-ng google.com --fields hop,host,jitter,jitter-avg,graph
```

### Interactive Mode
```bash
# Real-time monitoring with keyboard controls
mtr-ng google.com

# Keys (press ? for the full list, or see `man mtr-ng`):
# q / Ctrl-C  Quit
# o           Choose and order columns
# r           Reset statistics
# h           Host names or IP addresses
# s, v, c     RTT scale, sparkline/heatmap, colour mode
# f           Hop table or flow matrix
# m           Hop table or map
# e           Show or hide MPLS labels
# ↑ ↓ PgUp PgDn Home End  Select and scroll hops
# Enter       Details of the selected hop (↑ ↓ for the others)
```

Colours follow `NO_COLOR` and `TERM=dumb`.

### Report Mode
```bash
# Generate report output
mtr-ng google.com --report

# Fewer columns
mtr-ng google.com --report --fields hop,host,loss,avg

# Machine-readable: JSON with mtr's field names, or CSV
mtr-ng google.com --json
mtr-ng google.com --csv > trace.csv

# AS numbers via Team Cymru's DNS (public addresses only)
mtr-ng -z google.com

# Follow 8 flows: the report ends with which address each flow met
mtr-ng -r -c 40 --flows 8 google.com

# The routes as a picture
mtr-ng -c 40 --flows 8 --dot google.com | dot -Tsvg > routes.svg

# MPLS labels under their hop
mtr-ng -r -e google.com

# Where each hop is, and whether its RTT allows it
mtr-ng -r -g google.com
mtr-ng -r -g --origin 52.37,4.89 google.com
```

## Advanced Features

### Geolocation
`-g`, the `geo` and `dist` columns and the map need a database in MaxMind's format. mtr-ng uses `--geoip-mmdb PATH`, else the newest `.mmdb` in `~/.local/share/mtr-ng`, else `dbip-city-lite.mmdb` or `GeoLite2-City.mmdb` in `/usr/share/GeoIP`, `/var/lib/GeoIP` or Homebrew's `var/GeoIP`. DB-IP's free City Lite database (CC BY 4.0, about 130 MB) is updated monthly:
```bash
mkdir -p ~/.local/share/mtr-ng && \
curl -fL "https://download.db-ip.com/free/dbip-city-lite-$(date +%Y-%m).mmdb.gz" | \
gunzip > ~/.local/share/mtr-ng/dbip.part && \
mv ~/.local/share/mtr-ng/dbip.part ~/.local/share/mtr-ng/dbip-city-lite.mmdb
```
Both licences ask for credit wherever the data is shown, so the report, the map and the hop details name the database's source. The map's coastlines are Natural Earth's 1:50m set, which is in the public domain.

A database places a router where its owner registered it, often a head office a continent away, and anycast addresses wherever their registrant sits. mtr-ng therefore treats a location as a claim to test: a hop farther from the origin than its best RTT allows, granting both locations their accuracy, is marked `!`. Without `--origin` the origin is the first hop placed in a city, and the probing host is somewhere within that hop's reach, which widens the margin; with `--origin` the check is tighter. The check can only rule a location out: one without `!` is possible, not confirmed.

Router names are often better evidence. Large networks name their routers after the city they stand in, each in codes of its own: Arelion's `adm` is Amsterdam, `win` Vienna and `san` San Antonio, while to Cogent, which uses airport codes, `san` is San Diego. mtr-ng carries the codes of Arelion, Cogent, Hurricane Electric and Lumen, taken from the router lists in their looking glasses (and, for Cogent's other sites, OurAirports' list of large airports). When a hop's name has one, that city goes before the database, unless the RTT rules it out and not the database's location; the hop details say `adm in its name`. The names come from reverse DNS, so `-n` turns this off.

### Simulation Mode
`--simulate` answers probes from a made-up path instead of the network, through the same probing loop as real probes, and marks the screen SIMULATED. The path has a load-balanced pair of routers inside an MPLS tunnel, a router that never answers, some loss and the odd latency spike. Its addresses come from the documentation ranges and its names end in `.example`. It comes with made-up locations, including one the light-speed check rules out, so `-g` and the map work without a database.
```bash
# No network, no privileges
mtr-ng --simulate example.com

# The load balancer shows up with two flows or more
mtr-ng --simulate -r -c 8 --flows 4 -e example.com
```

### Sparkline Visualization
MTR-NG provides beautiful Unicode sparklines showing RTT history:
- `▁▂▃▄▅▆▇█` - Visual representation of network performance
- Bar height and colour place each reply on one RTT scale shared by all hops (shown under the table); `·` is a lost probe, `?` one still in flight
- Real-time updates as packets are sent/received

### Jitter Analysis  
Monitor network stability with comprehensive jitter metrics:
- **Last Jitter**: Most recent RTT variation
- **Average Jitter**: Long-term jitter trends
- Helps identify unstable network segments



## Column Reference

| Column      | Description                    | Example  |
|-------------|--------------------------------|----------|
| `hop`       | Hop number                     | `1`      |
| `host`      | Hostname/IP address            | `gateway.local` |
| `asn`       | Origin AS (with `-z`)          | `AS13335` |
| `loss`      | Packet loss percentage         | `2.0%`   |
| `sent`      | Packets sent                   | `10`     |
| `last`      | Most recent RTT               | `15.2ms` |
| `avg`       | Average RTT                   | `18.4ms` |
| `ema`       | Exponential moving average    | `17.8ms` |
| `jitter`    | Last jitter value             | `2.1ms`  |
| `jitter-avg`| Average jitter                | `1.8ms`  |
| `best`      | Minimum RTT observed          | `12.1ms` |
| `worst`     | Maximum RTT observed          | `45.2ms` |
| `stdev`     | RTT standard deviation        | `3.1ms`  |
| `graph`     | RTT sparkline visualization   | `▁▂▄▇▆▃▁` |

## Development Status

MTR-NG is actively developed with focus on:
- **Stable Core**: Reliable network tracing and statistics
- **Rich Visualization**: Unicode sparklines and color coding  
- **Column Flexibility**: Complete customization system
- **Interactive UI**: Real-time monitoring with controls
- **IPv6 Support**: Next-generation protocol support (planned)

## Development & Testing

### Easy Testing (No Sudo Required)
MTR-NG provides comprehensive testing tools that work without administrator privileges:

```bash
# Quick start - test simulation mode
cargo run -- --simulate --count 3 --report google.com

# Run all unit tests
cargo test

# Use automated testing tools
./scripts/test-dev.sh test-all     # Shell script
just ci                            # The CI gate: fmt, clippy, tests

# Real probes over loopback (macOS: no sudo; Linux: sudo)
cargo build && ./target/debug/mtr-ng -r -c 2 -n 127.0.0.1
```

**Testing Documentation**: See [TESTING.md](TESTING.md) for comprehensive testing guide, and [DEVELOPMENT.md](DEVELOPMENT.md) for how probing works and how CI is set up.

### Development Tools
The project includes several convenience tools:
- `justfile` - Task runner with development commands (`just --list`)
- `scripts/test-dev.sh` - Shell script for common testing tasks
- Simulation mode - Exercise the UI and statistics without network access
- Unit tests: `cargo test`

## Contributing

Contributions welcome! Areas of interest:
- Terminal graphics and visualization improvements
- IPv6 protocol support
- Additional statistical metrics
- Platform-specific optimizations

**Getting Started**: All development can be done without sudo using simulation mode - see [TESTING.md](TESTING.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
