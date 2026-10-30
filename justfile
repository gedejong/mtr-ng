# Justfile for mtr-ng development and testing
# Install with: cargo install just
# Run with: just <command>

# Real probes need root on Linux and FreeBSD; macOS uses unprivileged ICMP sockets.
sudo := if os() == "macos" { "" } else { "sudo" }

# Default recipe lists all available commands
default:
    @just --list

# Run all tests without sudo
test:
    @echo "🧪 Running all tests (no sudo required)"
    cargo test
    @echo "✅ All unit tests passed!"

# Quick simulation test
sim target="google.com":
    @echo "🔬 Running simulation for {{target}}"
    cargo run -- --simulate --count 3 --report {{target}}

# Interactive simulation (press 'q' to quit)
demo target="google.com":
    @echo "🎮 Starting interactive simulation (press 'q' to quit)"
    cargo run -- --simulate {{target}}

# Fast simulation test for CI/development
quick target="8.8.8.8":
    cargo run -- --simulate --count 1 --interval 100 --report {{target}}

# Test different output formats
formats:
    @echo "📊 Testing different output formats"
    cargo run -- --simulate --count 2 --report --fields hop,host,loss,avg google.com
    cargo run -- --simulate --count 2 --report --show-all google.com
    cargo run -- --simulate --count 2 --report --numeric google.com

# Development build and test cycle
dev:
    cargo build
    cargo test
    just sim

# Release build and test
release:
    cargo build --release
    cargo test
    @echo "🎯 Testing release build performance"
    time ./target/release/mtr-ng --simulate --count 10 --interval 50 --report google.com

# Check code quality (the lint job in CI)
lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

# Fix code formatting
fmt:
    cargo fmt

# Build documentation
docs:
    cargo doc --open

# Clean and rebuild everything
clean:
    cargo clean
    cargo build
    cargo test

# Compare real probes with mtr (builds as you; sudo only for the binaries that need it)
compare target="google.com":
    cargo build
    @echo "mtr-ng:"
    {{sudo}} ./target/debug/mtr-ng --count 3 --report {{target}}
    @echo ""
    @echo "mtr:"
    @command -v mtr >/dev/null || { echo "mtr is not installed"; exit 1; }
    sudo mtr -c 3 -r {{target}}

# Linux network lab in Docker (NAT, ECMP diamond, IPv6, PMTU): just netlab -- mtr-ng -r -n target
[positional-arguments]
netlab *args:
    @scripts/netlab/run.sh "$@"

# Show help for all CLI options
help:
    cargo run -- --help

# Run a specific test pattern
test-pattern pattern:
    cargo test {{pattern}}

# Performance benchmark in simulation mode
bench:
    @echo "⚡ Performance benchmark (simulation mode)"
    time cargo run --release -- --simulate --count 100 --interval 10 --report google.com > /dev/null
    @echo "✅ Benchmark completed"

# Check dependencies for advisories, licences and sources (the cargo-deny jobs in CI)
audit:
    cargo deny check

# The CI gate: formatting, clippy and tests
ci: lint test
    @echo "CI gate passed"

# Save unit test and simulation output to target/test-report.txt
report:
    @mkdir -p target
    cargo test 2>&1 | tee target/test-report.txt
    just quick 2>&1 | tee -a target/test-report.txt
    @echo "Test report saved to target/test-report.txt"
