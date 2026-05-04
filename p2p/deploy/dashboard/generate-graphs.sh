#!/bin/bash
# Generate latency graphs from RRD data.
# Run via cron every 30 seconds or as needed.
# Usage: generate-graphs.sh [output_dir]

set -euo pipefail

RRD="/var/lib/ergo-proxy/latency.rrd"
OUT="${1:-/var/www/ergo-proxy}"

mkdir -p "$OUT"

if [ ! -f "$RRD" ]; then
    echo "RRD not found: $RRD"
    exit 1
fi

# Common graph options
OPTS=(
    --width 800 --height 200
    --color "BACK#1a1a2e"
    --color "CANVAS#16213e"
    --color "FONT#e0e0e0"
    --color "GRID#333355"
    --color "MGRID#555577"
    --color "AXIS#e0e0e0"
    --color "ARROW#e0e0e0"
    --border 0
    --font "DEFAULT:10:monospace"
    --imgformat PNG
)

# Check if latency data exists (not all NaN)
has_latency_data() {
    local last
    last=$(rrdtool lastupdate "$RRD" 2>/dev/null | tail -1 | awk '{print $2}')
    [ -n "$last" ] && [ "$last" != "U" ]
}

# Only generate latency graphs if we have actual data
if has_latency_data; then
    for period in "6h:6 hours" "24h:24 hours" "7d:7 days"; do
        start="${period%%:*}"
        title="${period#*:}"
        rrdtool graph "$OUT/latency-${start}.png" \
            "${OPTS[@]}" \
            --vertical-label "ms" \
            --title "Peer Latency (${title})" \
            --start "-${start}" \
            --alt-autoscale \
            "DEF:rmin=$RRD:min:AVERAGE" \
            "DEF:ravg=$RRD:avg:AVERAGE" \
            "DEF:rmax=$RRD:max:AVERAGE" \
            "AREA:rmax#4a1942:Max" \
            "AREA:ravg#2b6777:Avg" \
            "LINE1:ravg#52d681:Avg " \
            "LINE1:rmax#c84b6a:Max " \
            "LINE1:rmin#4bc8c8:Min " \
            "GPRINT:rmin:LAST:Min\: %5.1lf ms" \
            "GPRINT:ravg:LAST:Avg\: %5.1lf ms" \
            "GPRINT:rmax:LAST:Max\: %5.1lf ms\\n" \
            > /dev/null
    done
else
    # Generate placeholder "no data" graphs
    for start in 6h 24h 7d; do
        rrdtool graph "$OUT/latency-${start}.png" \
            "${OPTS[@]}" \
            --vertical-label "ms" \
            --title "Peer Latency — no data yet" \
            --start "-${start}" \
            --lower-limit 0 --upper-limit 1 --rigid \
            "DEF:ravg=$RRD:avg:AVERAGE" \
            "COMMENT:Waiting for modifier request/response traffic\\n" \
            > /dev/null
    done
fi

# Peer count — use LAST consolidation, integer Y axis
rrdtool graph "$OUT/peers-24h.png" \
    "${OPTS[@]}" \
    --height 100 \
    --vertical-label "peers" \
    --title "Connected Outbound Peers (24 hours)" \
    --start -24h \
    --lower-limit 0 \
    --rigid \
    --units-exponent 0 \
    --y-grid 1:1 \
    "DEF:peers=$RRD:peers:LAST" \
    "AREA:peers#2b6777:Peers" \
    "LINE1:peers#52d681:" \
    "GPRINT:peers:LAST:Current\: %2.0lf" \
    > /dev/null
