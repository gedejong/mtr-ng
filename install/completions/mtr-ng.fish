complete -c mtr-ng -s c -l count -d 'Number of rounds to run (default: until quit; 10 in report mode)' -r
complete -c mtr-ng -s i -l interval -d 'Time between rounds in milliseconds (at least 100, except with --simulate)' -r
complete -c mtr-ng -s M -l max-hops -d 'Maximum number of hops (1-255)' -r
complete -c mtr-ng -l geoip-mmdb -d 'Geolocation database in MaxMind\'s format: DB-IP City Lite or GeoLite2 City. Default: the newest .mmdb in ~/.local/share/mtr-ng, then the usual system places' -r -F
complete -c mtr-ng -l privacy-hops -d 'Hide the first N hops: their addresses, names, ASes and locations are neither shown, in the TUI or any report, nor looked up. For screenshots and reports that shouldn\'t give away the network they were taken from' -r
complete -c mtr-ng -l origin -d 'Where you are, in degrees, for distances and the plausibility check. Default: the first hop the database places in a city, with its uncertainty' -r
complete -c mtr-ng -l flows -d 'Probe this many flows in turn, one per round. All probes of a flow look alike to load balancers (Paris traceroute), so a flow follows one path; several flows reveal the paths a load balancer spreads traffic over. With auto, new flows until every branch is found with 95% confidence (MDA-Lite), then the fewest flows that pass every branch' -r
complete -c mtr-ng -l sparkline-scale -d 'Sparkline scaling mode: linear or logarithmic (default: logarithmic)' -r -f -a "linear\t''
logarithmic\t''"
complete -c mtr-ng -l ema-alpha -d 'Smoothing factor for the EMA column, above 0 and at most 1. Higher follows recent RTTs faster' -r
complete -c mtr-ng -l fields -d 'Select which columns to display (default: hop,host,loss,sent,last,avg,ema,best,worst,graph)' -r -f -a "hop\t'Hop number'
host\t'Hostname/IP address'
asn\t'Origin AS number (looked up with --aslookup)'
as-name\t'The AS\'s name, its handle as the registry has it (looked up with the ASN)'
prefix\t'The announced prefix the address is in (looked up with the ASN)'
geo\t'Where the hop is, from the geolocation database (see --geo)'
dist\t'Distance from the origin in km (see --origin)'
stretch\t'The best RTT past the origin\'s over light in fibre\'s for the distance: 1 is a straight fibre'
reach\t'Farthest from here a reply can come in the best RTT, in km'
back\t'Routers the reply crossed, from its TTL; +N or -N when the way there was shorter or longer'
loss\t'Packet loss percentage'
sent\t'Number of packets sent'
last\t'Last RTT measurement'
avg\t'Average RTT'
ema\t'Exponential moving average RTT'
jitter\t'Last jitter value'
jitter-avg\t'Average jitter'
best\t'Best (minimum) RTT'
worst\t'Worst (maximum) RTT'
stdev\t'Standard deviation of the RTT'
graph\t'RTT sparkline graph'"
complete -c mtr-ng -s P -l protocol -d 'Probe protocol (tcp is not implemented yet)' -r -f -a "icmp\t'ICMP Echo (ping) - default and most common'
udp\t'UDP datagrams to port 33434 and up, one port per flow'
tcp\t'TCP SYN probes (not implemented yet)'"
complete -c mtr-ng -l log-file -d 'Append debug logs to this file (level can be overridden with RUST_LOG)' -r -F
complete -c mtr-ng -s 4 -d 'Use IPv4 only'
complete -c mtr-ng -s 6 -d 'Use IPv6 only'
complete -c mtr-ng -s r -l report -d 'Enable report mode (non-interactive)'
complete -c mtr-ng -s j -l json -d 'Report as JSON, with mtr\'s field names (implies --report)'
complete -c mtr-ng -s C -l csv -d 'Report as CSV, one row per hop (implies --report)'
complete -c mtr-ng -l dot -d 'Print the routes as a Graphviz graph instead of a table (implies --report). Most useful with --flows'
complete -c mtr-ng -l geojson -d 'Print the located hops and the path between them as GeoJSON, for map tools (implies --report; needs a geolocation database, see --geo)'
complete -c mtr-ng -s n -l numeric -d 'Show IP addresses instead of hostnames'
complete -c mtr-ng -s z -l aslookup -d 'Show the AS number of each hop. Asks Team Cymru\'s DNS about every public address'
complete -c mtr-ng -s g -l geo -d 'Show where each hop is (Location and Dist columns), from a local geolocation database: nothing is sent anywhere. A location farther away than light in fibre gets in the hop\'s best RTT is marked with !'
complete -c mtr-ng -s e -l mpls -d 'Show the MPLS labels routers report (RFC 4950) under their hop'
complete -c mtr-ng -l pmtu -d 'Find the path MTU and where it narrows: each round one more probe, as large as the path seems to allow, with Don\'t Fragment set'
complete -c mtr-ng -l show-all -d 'Display all available columns'
complete -c mtr-ng -l simulate -d 'Show fake data instead of probing (for demos and testing); marked SIMULATED'
complete -c mtr-ng -s q -l quiet -d 'Report mode: print errors only, no warnings'
complete -c mtr-ng -s h -l help -d 'Print help (see more with \'--help\')'
complete -c mtr-ng -s V -l version -d 'Print version'
