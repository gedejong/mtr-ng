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
# ↑ ↓ PgUp PgDn Home End  Select and scroll hops
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
```

## Advanced Features

### Simulation Mode (No Sudo Required!)
For development, testing, and demo purposes, MTR-NG includes a comprehensive simulation mode:
```bash
# Test without requiring administrator privileges
mtr-ng google.com --simulate

# Quick test in report mode
mtr-ng google.com --simulate --count 3 --report
```

**Perfect for:**
- Development and debugging
- CI/CD pipelines  
- Testing on restricted systems
- Educational demonstrations
- Validating UI and statistics logic

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
