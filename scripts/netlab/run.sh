#!/bin/bash
#
# A Linux network lab for mtr-ng, in one privileged Docker container: builds
# mtr-ng for Linux, sets up a dual-stack topology of network namespaces (NAT,
# an ECMP diamond, a lower MTU, netem delays) and runs a command on the client.
# Works from macOS (colima, Docker Desktop) and Linux. The topology is in lab.sh.
#
# The repository is mounted read-only; the cargo registry and target dir live in
# the Docker volumes mtr-ng-netlab-cargo and mtr-ng-netlab-target, so rebuilds
# are incremental and nothing lands in ./target.

set -euo pipefail

show_help() {
    cat <<'EOF'
Usage: scripts/netlab/run.sh [OPTIONS] [--] CMD [ARGS...]
       scripts/netlab/run.sh [OPTIONS] --shell

Build mtr-ng for Linux, start the lab and run CMD in the client namespace with
mtr-ng on PATH. Exits with CMD's exit status. Lab messages go to stderr.

  client ── gw ── r1 ──┬── r2a ──┬── r3 ══ target   (target6 for IPv6)
            NAT        └── r2b ──┘   MTU 1400

Options:
  --user probe       Run CMD as the unprivileged user probe (default: root)
  --no-ping-group    Set net.ipv4.ping_group_range to "1 0", so nobody may open
                     ping sockets (default: "0 2147483647", as systemd does)
  --silent ROUTER    ROUTER (gw, r1, r2a, r2b, r3, r2c) sends no Time Exceeded;
                     repeat it or give a comma-separated list
  --black-hole       r3 sends no Fragmentation Needed or Packet Too Big, so
                     packets over 1400 bytes vanish there (a PMTU black hole)
  --loss PCT         Lose PCT percent of packets from gw to r1 (hops 2 and on)
  --hash-policy N    r1's multipath hash policy (default 1, the 5-tuple; 3 is
                     every outer field, IPv6 flow label included)
  --icmp-ecmp        r1 sends echo requests with an even ICMP checksum to r2a,
                     odd ones to r2b (Linux itself never splits ICMP)
  --wide             A third branch, r2c, makes the diamond three ways wide
                     (--icmp-ecmp still splits echo requests two ways)
  --spin             Keep the VM's CPUs busy while CMD runs, so netem delays are
                     accurate; idle, colima's timers make RTTs read 10-20% high
  --shell            Open a shell in the client namespace instead of CMD
  -h, --help         Show this help

Examples:
  scripts/netlab/run.sh -- mtr-ng -r -c 3 -n target
  scripts/netlab/run.sh --user probe -- ping -c 1 target
  scripts/netlab/run.sh --silent r3 -- traceroute -n target
  scripts/netlab/run.sh --shell
  just netlab -- tracepath -n target

Other namespaces: gw r1 r2a r2b r3 target. As root in the shell, reach them
with `ip netns exec r1 CMD`, e.g. `ip netns exec r1 tcpdump -ni r2a`.
EOF
}

# Options are parsed by lab.sh inside the container; only help is answered here.
for arg in "$@"; do
    case "$arg" in
        --) break ;;
        -h | --help)
            show_help
            exit 0
            ;;
    esac
done

if ! command -v docker >/dev/null 2>&1; then
    echo "netlab: docker not found" >&2
    exit 2
fi

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
image=mtr-ng-netlab

# Build the image every time (cached, so quick) to pick up Dockerfile changes.
if docker image inspect "$image" >/dev/null 2>&1; then
    docker build --quiet --tag "$image" "$here" >/dev/null
else
    echo "netlab: building the $image image (the first time pulls rust:1.89-bookworm)" >&2
    docker build --tag "$image" "$here" >&2
fi

# A terminal only when there is one, so the lab also works from scripts.
interactive=-i
if [ -t 0 ] && [ -t 1 ]; then
    interactive=-it
fi

# --privileged: creating namespaces and setting their sysctls needs it.
# --init: a real PID 1 that forwards signals (Ctrl-C) and reaps dnsmasq.
exec docker run --rm "$interactive" --init --privileged \
    --hostname netlab \
    --volume "$repo:/src:ro" \
    --volume mtr-ng-netlab-target:/target \
    --volume mtr-ng-netlab-cargo:/usr/local/cargo/registry \
    --env RUST_LOG \
    "$image" /src/scripts/netlab/lab.sh "$@"
