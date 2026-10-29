#!/bin/bash
#
# The inside of the netlab container, started by run.sh: build mtr-ng, wire up
# a small routed network out of network namespaces and veth pairs, and run a
# command in the client namespace.
#
#   client ── gw ── r1 ──┬── r2a ──┬── r3 ══ target
#             NAT        ├── r2b ──┤   MTU 1400
#                        └── r2c ──┘   (only with --wide)
#
# Link N joins two namespaces on 10.0.N.0/24 and fd00:N::/64. The node nearer
# the target is .1 (::1) and the other .2 (::2), so the hop a probe reaches over
# link N answers from 10.0.N.1. IPv6 uses ULA space (fd00::/8), the IPv6
# counterpart of 10/8: mtr-ng's asn::is_public counts it as private, so -z
# never asks Team Cymru about lab addresses. It counts 2001:db8::/32 as public.
#
# Each run starts from a fresh container, so nothing here needs cleaning up.

set -euo pipefail

NAMESPACES="client gw r1 r2a r2b r3 target"
ROUTERS="gw r1 r2a r2b r3"

# One link per line: number, the node nearer the client, the node nearer the
# target, delay in ms towards the target (none back), MTU.
LINKS="
1 client gw     2  1500
2 gw     r1     10 1500
3 r1     r2a    4  1500
4 r1     r2b    8  1500
5 r2a    r3     4  1500
6 r2b    r3     4  1500
7 r3     target 20 1400
"

log() { printf 'netlab: %s\n' "$*" >&2; }
die() {
    log "$*"
    exit 2
}

RUN_AS=root
PING_GROUP="0 2147483647" # what systemd sets on most distributions
SILENT=""
BLACK_HOLE=0
LOSS=""
HASH_POLICY=1
ICMP_ECMP=0
WIDE=0
SPIN=0
SHELL_MODE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --user)
            [ $# -ge 2 ] || die "--user needs a name (probe or root)"
            case "$2" in
                probe | root) RUN_AS=$2 ;;
                *) die "--user $2: the lab has users probe and root" ;;
            esac
            shift
            ;;
        --no-ping-group)
            # The kernel's own default: no group may open ping sockets.
            PING_GROUP="1 0"
            ;;
        --silent)
            [ $# -ge 2 ] || die "--silent needs a router name"
            SILENT="$SILENT ${2//,/ }" # checked below, once --wide is known
            shift
            ;;
        --black-hole)
            BLACK_HOLE=1
            ;;
        --loss)
            [ $# -ge 2 ] || die "--loss needs a percentage"
            [[ $2 =~ ^[0-9]+(\.[0-9]+)?$ ]] || die "--loss $2: not a percentage"
            LOSS=$2
            shift
            ;;
        --hash-policy)
            [ $# -ge 2 ] || die "--hash-policy needs 0, 1, 2 or 3"
            case "$2" in
                0 | 1 | 2 | 3) HASH_POLICY=$2 ;;
                *) die "--hash-policy $2: the kernel knows 0, 1, 2 and 3" ;;
            esac
            shift
            ;;
        --icmp-ecmp)
            ICMP_ECMP=1
            ;;
        --wide)
            WIDE=1
            ;;
        --spin)
            SPIN=1
            ;;
        --shell)
            SHELL_MODE=1
            ;;
        --)
            shift
            break
            ;;
        -*)
            die "unknown option $1 (see scripts/netlab/run.sh --help)"
            ;;
        *)
            break
            ;;
    esac
    shift
done

# --wide: a third branch, r2c, makes the diamond three ways wide.
if [ "$WIDE" -eq 1 ]; then
    NAMESPACES="$NAMESPACES r2c"
    ROUTERS="$ROUTERS r2c"
    LINKS="$LINKS
8 r1     r2c    6  1500
9 r2c    r3     4  1500
"
fi
for router in $SILENT; do
    case " $ROUTERS " in
        *" $router "*) ;;
        *) die "--silent $router: not a router (one of: $ROUTERS)" ;;
    esac
done

if [ "$SHELL_MODE" -eq 1 ]; then
    [ $# -eq 0 ] || die "--shell takes no command"
elif [ $# -eq 0 ]; then
    die "no command given (see scripts/netlab/run.sh --help)"
fi

# sysctl in one namespace. /proc/sys/net shows the namespace of the process
# reading it, and most net.* settings are per namespace.
nsysctl() {
    local ns=$1
    shift
    ip netns exec "$ns" sysctl -qw "$@"
}

build() {
    log "building mtr-ng (cargo build --locked; registry and target dir are Docker volumes)"
    CARGO_TARGET_DIR=/target cargo build --locked --quiet --manifest-path /src/Cargo.toml
    # Namespaces share the container's file system, so one copy serves them all.
    ln -sf /target/debug/mtr-ng /usr/local/bin/mtr-ng
}

add_namespaces() {
    local ns
    for ns in $NAMESPACES; do
        ip netns add "$ns"
        ip -n "$ns" link set lo up
        # Answer every probe. Linux limits ICMP errors per destination
        # (icmp_ratelimit: one per second after a burst of 6) and machine-wide
        # (icmp_msgs_per_sec: 1000/s, burst 50; on this 6.8 kernel that one is
        # global and cannot be set from a namespace). Types left out of the
        # ratemask skip both limits, so clear the masks. Echo replies were
        # never limited.
        nsysctl "$ns" net.ipv4.icmp_ratelimit=0 net.ipv4.icmp_ratemask=0 \
            net.ipv6.icmp.ratelimit=0 net.ipv6.icmp.ratemask=
        # Who may open ping sockets (ICMP datagram sockets, no privileges). It
        # covers ICMPv6 ping sockets too.
        nsysctl "$ns" "net.ipv4.ping_group_range=$PING_GROUP"
        # New namespaces inherit IPv4 settings from the VM (Ubuntu sets
        # rp_filter=2) but not IPv6 ones; spell out what the lab relies on.
        nsysctl "$ns" net.ipv4.conf.all.rp_filter=0 net.ipv4.conf.default.rp_filter=0 \
            net.ipv6.conf.all.disable_ipv6=0 net.ipv6.conf.default.disable_ipv6=0
        # No duplicate address detection, which keeps new addresses unusable
        # for a second or so.
        nsysctl "$ns" net.ipv6.conf.all.accept_dad=0 net.ipv6.conf.default.accept_dad=0
    done
    for ns in $ROUTERS; do
        nsysctl "$ns" net.ipv4.ip_forward=1 net.ipv6.conf.all.forwarding=1
        # How r1 (the only multipath route) picks a branch of the diamond.
        # Policy 1 hashes the 5-tuple, so UDP probes with different ports
        # split. No policy looks at the ICMP identifier or checksum, so ICMP
        # echo never splits (see --icmp-ecmp). The IPv6 flow label counts in
        # policies 0 and 2, and in 3 with field 0x0008; 3 here hashes every
        # outer field (addresses, protocol, flow label, ports).
        nsysctl "$ns" net.ipv4.fib_multipath_hash_policy="$HASH_POLICY" \
            net.ipv6.fib_multipath_hash_policy="$HASH_POLICY"
        if [ "$HASH_POLICY" = 3 ]; then
            nsysctl "$ns" net.ipv4.fib_multipath_hash_fields=0x003f \
                net.ipv6.fib_multipath_hash_fields=0x003f
        fi
        # Name the incoming interface in ICMP errors (RFC 5837), which -e
        # shows. Kernels from late 2025 on can; older ones, such as Docker
        # Desktop's 6.12, lack the setting.
        if ip netns exec "$ns" test -e /proc/sys/net/ipv4/icmp_errors_extension_mask; then
            nsysctl "$ns" net.ipv4.icmp_errors_extension_mask=1 \
                net.ipv6.icmp.errors_extension_mask=1
        fi
    done
}

add_link() {
    local n=$1 near=$2 far=$3 delay=$4 mtu=$5
    # Each end is named after the namespace at the other end: in r1, the
    # interface "r2a" leads to r2a.
    ip -n "$near" link add name "$far" type veth peer name "$near" netns "$far"
    # Up first, addresses second. An IPv6 address added while the link is down
    # ignores neighbour solicitations until the kernel's address work runs
    # after link-up, a random delay of up to rtr_solicit_delay (1 s) later,
    # even without DAD; the first IPv6 trace would then come back a second late.
    ip -n "$near" link set "$far" mtu "$mtu" up
    ip -n "$far" link set "$near" mtu "$mtu" up
    ip -n "$near" addr add "10.0.$n.2/24" dev "$far"
    ip -n "$far" addr add "10.0.$n.1/24" dev "$near"
    ip -n "$near" -6 addr add "fd00:$n::2/64" dev "$far"
    ip -n "$far" -6 addr add "fd00:$n::1/64" dev "$near"
    # veth offloads UDP and TCP checksums to "hardware" that never computes
    # them, so packets would cross the lab carrying only the pseudo-header sum.
    # ICMP errors then quote that, and NAT adjusts the quoted copy as if it
    # were a real checksum, so the client gets back a checksum it never sent.
    # Checksum in software, as a real network would deliver them.
    ip netns exec "$near" ethtool -K "$far" tx off >/dev/null
    ip netns exec "$far" ethtool -K "$near" tx off >/dev/null
    # netem delays what leaves an interface. Only the direction towards the
    # target gets a delay: every netem stage is a timer wait, and on an idle
    # colima VM each wait runs late (see --spin), so fewer stages, less error.
    tc -n "$near" qdisc add dev "$far" root netem delay "${delay}ms"
}

add_links() {
    local n near far delay mtu
    while read -r n near far delay mtu; do
        [ -n "$n" ] || continue
        add_link "$n" "$near" "$far" "$delay" "$mtu"
    done <<<"$LINKS"
}

# Ping across every link once in both families, so ARP and neighbour caches are
# filled and the first probes of a trace don't queue behind address resolution
# (which adds tens of ms to a traceroute's first burst). An entry turns stale
# after about 30 s but is still used without waiting.
warm_up() {
    local n near _
    while read -r n near _; do
        [ -n "$n" ] || continue
        ip netns exec "$near" ping -c 1 -W 2 "10.0.$n.1" >/dev/null &
        ip netns exec "$near" ping -6 -c 1 -W 2 "fd00:$n::1" >/dev/null &
    done <<<"$LINKS"
    wait
}

add_loss() {
    [ -n "$LOSS" ] || return 0
    # On the uplink, gw towards r1: probes to hop 2 and beyond get lost,
    # replies don't, so every hop from r1 on shows about LOSS percent.
    local delay
    delay=$(awk '$1 == 2 { print $4 }' <<<"$LINKS")
    tc -n gw qdisc change dev r1 root netem delay "${delay}ms" loss "$LOSS%"
}

add_routes() {
    # The client and its home router.
    ip -n client route add default via 10.0.1.1
    ip -n client -6 route add default via fd00:1::1
    ip -n gw route add default via 10.0.2.1
    ip -n gw -6 route add default via fd00:2::1
    # IPv4 leaves home through NAT, like behind a home router; IPv6 is routed.
    # So upstream routers know fd00:1::/64 but not 10.0.1.0/24.
    ip netns exec gw iptables -t nat -A POSTROUTING -s 10.0.1.0/24 -o r1 -j MASQUERADE

    # r1: the diamond. Two next hops for everything behind it, three with --wide.
    local p via4=(nexthop via 10.0.3.1 nexthop via 10.0.4.1)
    local via6=(nexthop via fd00:3::1 nexthop via fd00:4::1)
    if [ "$WIDE" -eq 1 ]; then
        via4+=(nexthop via 10.0.8.1)
        via6+=(nexthop via fd00:8::1)
    fi
    for p in 5 6 7; do
        ip -n r1 route add "10.0.$p.0/24" "${via4[@]}"
        ip -n r1 -6 route add "fd00:$p::/64" "${via6[@]}"
    done
    ip -n r1 -6 route add fd00:1::/64 via fd00:2::2

    # r2a and r2b: forward to r3, back to r1.
    ip -n r2a route add 10.0.6.0/24 via 10.0.5.1
    ip -n r2a route add 10.0.7.0/24 via 10.0.5.1
    ip -n r2a route add 10.0.2.0/24 via 10.0.3.2
    ip -n r2b route add 10.0.5.0/24 via 10.0.6.1
    ip -n r2b route add 10.0.7.0/24 via 10.0.6.1
    ip -n r2b route add 10.0.2.0/24 via 10.0.4.2
    for p in 6 7; do ip -n r2a -6 route add "fd00:$p::/64" via fd00:5::1; done
    for p in 5 7; do ip -n r2b -6 route add "fd00:$p::/64" via fd00:6::1; done
    for p in 1 2; do
        ip -n r2a -6 route add "fd00:$p::/64" via fd00:3::2
        ip -n r2b -6 route add "fd00:$p::/64" via fd00:4::2
    done
    if [ "$WIDE" -eq 1 ]; then
        for p in 5 6 7; do
            ip -n r2c route add "10.0.$p.0/24" via 10.0.9.1
            ip -n r2c -6 route add "fd00:$p::/64" via fd00:9::1
        done
        ip -n r2c route add 10.0.2.0/24 via 10.0.8.2
        for p in 1 2; do ip -n r2c -6 route add "fd00:$p::/64" via fd00:8::2; done
    fi

    # r3 sends everything back through r2a, whichever branch a probe came in
    # by, and answers from one address so the diamond visibly closes at hop 4.
    # The kernel picks the source of an ICMP error differently per family:
    # IPv4 takes the interface the error leaves by (unless
    # icmp_errors_use_inbound_ifaddr=1), IPv6 the one the probe came in by
    # (unless the route back has a preferred source). Hence "src" on IPv6 only.
    ip -n r3 route add default via 10.0.5.2
    ip -n r3 -6 route add default via fd00:5::2 src fd00:5::1
    ip -n target route add default via 10.0.7.2
    ip -n target -6 route add default via fd00:7::2
}

# --icmp-ecmp: make r1 split ICMP echo requests the way many hardware routers
# do, by hashing the first word of the transport header, which for ICMP holds
# type, code and checksum. That is what Paris traceroute's fixed checksum per
# flow is for; Linux itself never looks there. The "hash" is the checksum's
# low bit, so flows whose checksums differ by one alternate between branches:
# even to r2a, odd to r2b.
add_icmp_ecmp() {
    [ "$ICMP_ECMP" -eq 1 ] || return 0
    ip netns exec r1 nft -f - <<'EOF'
table inet icmp_ecmp {
    chain prerouting {
        type filter hook prerouting priority mangle; policy accept;
        iifname "gw" icmp type echo-request @th,31,1 0 meta mark set 3
        iifname "gw" icmp type echo-request @th,31,1 1 meta mark set 4
        iifname "gw" icmpv6 type echo-request @th,31,1 0 meta mark set 3
        iifname "gw" icmpv6 type echo-request @th,31,1 1 meta mark set 4
    }
}
EOF
    # Mark 3 leaves over link 3 (r2a), mark 4 over link 4 (r2b). Echo requests
    # to r1 itself still match the local table first.
    local family
    for family in -4 -6; do
        ip -n r1 "$family" rule add fwmark 3 table 3
        ip -n r1 "$family" rule add fwmark 4 table 4
    done
    ip -n r1 route add default via 10.0.3.1 table 3
    ip -n r1 route add default via 10.0.4.1 table 4
    ip -n r1 -6 route add default via fd00:3::1 table 3
    ip -n r1 -6 route add default via fd00:4::1 table 4
}

silence_routers() {
    local router
    for router in $SILENT; do
        # Drop the Time Exceeded messages the router itself sends; it still
        # forwards, so the hop shows as unanswered and later hops are fine.
        ip netns exec "$router" iptables -A OUTPUT -p icmp --icmp-type time-exceeded -j DROP
        ip netns exec "$router" ip6tables -A OUTPUT -p ipv6-icmp --icmpv6-type time-exceeded -j DROP
    done
    if [ "$BLACK_HOLE" -eq 1 ]; then
        # r3 can't forward more than 1400 bytes to the target and drops larger
        # packets with Don't Fragment set, but keeps quiet about it, as a router
        # behind a filter that eats ICMP does: a path MTU black hole.
        ip netns exec r3 iptables -A OUTPUT -p icmp --icmp-type fragmentation-needed -j DROP
        ip netns exec r3 ip6tables -A OUTPUT -p ipv6-icmp --icmpv6-type packet-too-big -j DROP
    fi
}

# Names for every lab address, served to the client from /etc/hosts. glibc reads
# the file directly; mtr-ng's resolver (hickory) reads it too but sends reverse
# lookups it can't answer to the nameserver, so a dnsmasq without upstream
# servers answers those at once instead of letting them time out.
add_names() {
    local dir=/etc/netns/client n near far
    mkdir -p "$dir"
    {
        echo "127.0.0.1 localhost"
        echo "::1 localhost"
        while read -r n near far _; do
            [ -n "$n" ] || continue
            echo "10.0.$n.2 $near"
            echo "fd00:$n::2 $near"
            echo "10.0.$n.1 $far"
            # "target" is IPv4 only, "target6" IPv6 only.
            if [ "$far" = target ]; then
                echo "fd00:$n::1 target6"
            else
                echo "fd00:$n::1 $far"
            fi
        done <<<"$LINKS"
    } >"$dir/hosts"
    # `ip netns exec client` bind-mounts these over /etc/hosts and /etc/resolv.conf.
    printf 'nameserver 127.0.0.1\noptions timeout:1 attempts:1\n' >"$dir/resolv.conf"
    ip netns exec client dnsmasq --conf-file=/dev/null --no-resolv --no-poll \
        --listen-address=127.0.0.1 --bind-interfaces --user=nobody --group=nogroup \
        --pid-file=/run/netlab-dnsmasq.pid
}

# --spin: an idle vCPU on colima (macOS virtualization) wakes up for a timer
# late, by roughly a sixth of the wait, so every netem delay runs long and
# spaced probes read 10-20% high; bursts, which find the CPU awake, don't.
# Busy vCPUs take timer interrupts on time. The spinners run at the lowest
# priority, so the lab never waits for them, and die with the container.
spin() {
    [ "$SPIN" -eq 1 ] || return 0
    local _
    for _ in $(seq "$(nproc)"); do
        nice -n 19 sh -c 'while :; do :; done' &
    done
}

print_map() {
    local loss_note="" silent_note="" ecmp_note=""
    [ -n "$LOSS" ] && loss_note=", ${LOSS}% loss gw->r1"
    [ -n "$SILENT" ] && silent_note="; silent (no Time Exceeded):$SILENT"
    [ "$BLACK_HOLE" -eq 1 ] && silent_note="$silent_note; r3 sends no Fragmentation Needed or Packet Too Big"
    [ "$ICMP_ECMP" -eq 1 ] && ecmp_note="; echo requests by checksum bit: even r2a, odd r2b"
    local wide_note=""
    [ "$WIDE" -eq 1 ] && wide_note="
  --wide: a third branch, link 8 r1 .2 -> r2c .1 (10.0.8.1, 6 ms), link 9 r2c .2 -> r3 .1 (4 ms)"
    [ "$SPIN" -eq 1 ] && silent_note="$silent_note; CPUs kept busy (--spin)"
    cat >&2 <<EOF

  client ──2ms── gw ──10ms── r1 ──┬──4ms── r2a ──4ms──┬── r3 ══20ms══ target
                 NAT              └──8ms── r2b ──4ms──┘   MTU 1400

  link  IPv4 / IPv6                 near end -> far end   delay towards target
  1     10.0.1.0/24  fd00:1::/64    client .2 -> gw .1    2 ms  (gw masquerades IPv4)
  2     10.0.2.0/24  fd00:2::/64    gw .2     -> r1 .1    10 ms${loss_note}
  3     10.0.3.0/24  fd00:3::/64    r1 .2     -> r2a .1   4 ms  (ECMP branch)
  4     10.0.4.0/24  fd00:4::/64    r1 .2     -> r2b .1   8 ms  (ECMP branch)
  5     10.0.5.0/24  fd00:5::/64    r2a .2    -> r3 .1    4 ms  (all return traffic)
  6     10.0.6.0/24  fd00:6::/64    r2b .2    -> r3 .1    4 ms
  7     10.0.7.0/24  fd00:7::/64    r3 .2     -> target .1  20 ms, MTU 1400

  hops: 1 gw 10.0.1.1 2 ms | 2 r1 10.0.2.1 12 ms | 3 r2a 10.0.3.1 16 ms or r2b 10.0.4.1 20 ms
        4 r3 10.0.5.1 20/24 ms | 5 target 10.0.7.1 40/44 ms (target6 fd00:7::1; IPv6 fd00:N::1)
  r1 multipath hash policy $HASH_POLICY${ecmp_note}${wide_note}
  user $RUN_AS, ping_group_range "$PING_GROUP"${silent_note}

EOF
}

build
add_namespaces
add_links
add_routes
warm_up
add_loss
add_icmp_ecmp
silence_routers
add_names
spin
print_map

if [ "$RUN_AS" = root ]; then
    cd /root
    as_user=()
else
    cd "/home/$RUN_AS"
    # Dropping to a non-zero uid clears every capability, so probe has no CAP_NET_RAW.
    as_user=(setpriv --reuid="$RUN_AS" --regid="$RUN_AS" --init-groups
        env HOME="/home/$RUN_AS" USER="$RUN_AS" LOGNAME="$RUN_AS")
fi
if [ "$SHELL_MODE" -eq 1 ]; then
    set -- bash --norc -i
    export PS1="[netlab client] \u:\w\$ "
fi
exec ip netns exec client "${as_user[@]}" "$@"
