# Image for scripts/netlab/run.sh: the Rust toolchain at the crate's MSRV plus
# the tools to build a routed topology of network namespaces and look at it.
# The lab script itself is not copied in; run.sh mounts the repository.
FROM rust:1.89-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        dnsmasq-base \
        ethtool \
        iproute2 \
        iptables \
        iputils-ping \
        iputils-tracepath \
        libcap2-bin \
        nftables \
        procps \
        tcpdump \
        traceroute \
    && rm -rf /var/lib/apt/lists/*

# Debian makes ping setuid root (or gives it cap_net_raw). Take both away, so
# that ping as a normal user works only through a ping socket, which
# net.ipv4.ping_group_range allows or forbids. That is what --no-ping-group tests.
RUN chmod u-s /usr/bin/ping \
    && { setcap -r /usr/bin/ping 2>/dev/null || true; } \
    && test -z "$(getcap /usr/bin/ping)"

# The unprivileged user for --user probe; it gets no capabilities.
RUN useradd --create-home --uid 1000 --user-group --shell /bin/bash probe
