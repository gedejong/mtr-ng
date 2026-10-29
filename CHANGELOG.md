# Changelog

Releases up to 0.3.0 are at [edejong-dbc/mtr-ng](https://github.com/edejong-dbc/mtr-ng/releases).

## Unreleased

- AS lookups (`-z`) also find the prefix each router's address is announced in and the AS's name. The detail pane (Enter) shows both, `--fields ...,as-name,prefix` adds them as columns, and the JSON and CSV carry them. A name is asked for once per AS.
- `--privacy-hops N` hides the first N hops, usually your own network and your provider's first routers, so a screenshot or a pasted report doesn't give them away: their addresses, names, ASes and locations, and the local host name, are left out of the TUI and every report format, and they are never looked up. Load-balanced branches among them stay apart.
- The `stretch` column (`--fields ...,stretch`) shows how much longer a hop's best RTT is than light in fibre would take for its distance, counting from the origin hop so your own line doesn't count: 1.0 is a straight fibre, and a hop well above its neighbours sits behind a detour. The report and the map's key also say how much longer the path through the located hops is than the great circle. JSON, CSV and GeoJSON carry both.
- On the map, an arc that passes over hops not on the map says how many, as `+2` halfway along.
- Shell completions for bash, zsh and fish, installed by Homebrew and the `.deb`, and in the release archives.
- ICMPv6 sockets ask the kernel only for the message types a trace reads, so the receiving thread no longer wakes for every neighbour solicitation and router advertisement on the link.
- `g` draws the routes the flows took, TTL by TTL, with the branches of each load balancer side by side and dashed lines over hops that didn't answer. The report draws it too, under the flow matrix, when the path branches. `--mermaid` prints the routes as a Mermaid flowchart, which Markdown on GitHub and GitLab draws.
- `--metrics [ADDR:]PORT` serves the trace for Prometheus at `/metrics`: per hop, counters of probes sent, received and lost, the loss ratio, an RTT summary with quantiles over the last 600 probes, the last, best and worst RTT, and which addresses answered with their names and ASes. OpenMetrics goes to scrapers that ask for it, the Prometheus text format to the rest. It runs alongside the TUI, or with `-r` and no `-c` until stopped. A bare port listens on 127.0.0.1 only.
- `--record PATH` keeps a log of the trace, one JSON object per line: each probe sent, each reply or timeout, and each name and AS that arrived. `--replay PATH` plays it back in the TUI, at the recorded pace or `--replay-speed` times faster, or with `-r` and the other report formats prints its report at once, without sending anything. Hops hidden by `--privacy-hops` are recorded hidden.
- A report stopped with Ctrl-C, or SIGTERM, prints what it has so far instead of nothing.
- `a` opens an About pane: what the running trace sends where (DNS to your resolver, AS lookups to Team Cymru, nothing for locations), and the data sources with their licences.

## 0.5.0 - 2026-10-29

IPv6, UDP probes, and less need for sudo on Linux. `--flows auto` finds every branch of the load balancers on the path, and `--pmtu` finds the path MTU, where it narrows, and where its black holes are.

### Load balancers

- `--flows auto` finds every branch of the load balancers on the path, then follows one flow per branch (MDA-Lite). Each round sends a new flow until every hop has answered enough flows to be 95% sure that no branch was missed: 6 flows where one address answered, 11 where two did. Then the fewest flows that together pass every address take turns, so each branch keeps its own loss and RTT, and every ten minutes the search starts over. A note under each balanced hop says how many branches it has; a report without `-c` runs until the search is done and ends with what it found, including where branches cross, which can hide one.
- `-P udp` sends UDP datagrams to port 33434 and up, one port per flow. Load balancers spread UDP by port far more often than they spread ICMP, so `--flows` over UDP shows paths that ICMP doesn't. It needs no sudo on macOS, and root or `CAP_NET_RAW` on Linux.
- IPv6 probes carry a flow label per flow, which is what Linux and many routers hash IPv6 on.

### IPv6 and the way back

- IPv6. A name with both kinds of address is traced over IPv6 when this machine has an IPv6 route, else over IPv4; `-4` and `-6` choose. MPLS labels and stable flows work over IPv6 too.
- The Back column (`--fields hop,host,back,loss,avg`) counts the routers a reply crossed on its way back, from the TTL it arrived with, and shows `+2` when that is two more than the probe crossed on the way there: the reply came back another way. It works for IPv6 replies too, whose hop limit is now read on every socket kind.

### Path MTU

- `--pmtu` finds the path MTU and the hop where it narrows: each round one more probe, as large as the path seems to allow, with Don't Fragment set. Routers that can't forward it say how much their next link takes; when large probes vanish without a word, a black hole, the size is found by halving, and then the hop past which they vanish, by sending probes of that size to nearer hops.

### Privileges and platforms

- On Linux, ICMP no longer needs root or `CAP_NET_RAW` where `net.ipv4.ping_group_range` allows ping sockets, as it does by default on systemd 243 and newer. Ping sockets don't pass on MPLS labels.
- On Linux, the `.deb` gives mtr-ng `CAP_NET_RAW` on install, so UDP probes and MPLS labels work without sudo (with `libcap2-bin`, which it recommends). mtr-ng opens its raw sockets first and then gives up the capability, setuid root, and as root every capability but reading files, before it resolves the target or reads a packet.
- Windows can probe: without administrator rights, through Windows' ICMP API, as `tracert` does. ICMP only, over IPv4 and IPv6; the API builds the packets itself, so `--flows`, `-P udp`, `--pmtu` and MPLS labels don't work there. Tested over loopback only, so there's no Windows download yet.

### Smaller things

- A trace in which no hop answers says so, in the table and as a warning after a report, instead of showing an empty table. UDP probes to the machine itself are the common case: checksum offload leaves their checksums unfinished.
- The man page's privileges section and bug link were out of date.

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
