#!/usr/bin/env bash
# Create RRD database files for ergo-node-rust observability.
# One-shot — run once before rrd-update.sh / rrd-graph.sh.
#
# Two RRDs:
#   chain.rrd  GAUGE DSes from /info       (heights, peers, mempool)
#   p2p.rrd    COUNTER DSes from /stats/p2p (7 buckets x 4 series)
#
# Bucket order (must match rrd-update.sh and rrd-demo-fill.sh):
#   headers, blocks, tx, discovery, sync_info, snapshot, control
# Series order within each bucket:
#   in_count, in_bytes, out_count, out_bytes
#
# Env:
#   RRD_DIR    output directory (default: ./rrd-data)
#   RRD_START  unix seconds for db start time (default: 24h ago)
#   RRD_STEP   sample interval in seconds (default: 60)

set -euo pipefail

RRD_DIR="${RRD_DIR:-./rrd-data}"
RRD_START="${RRD_START:-$(date -d '24 hours ago' +%s)}"
RRD_STEP="${RRD_STEP:-60}"

mkdir -p "$RRD_DIR"

if [[ -e "$RRD_DIR/chain.rrd" || -e "$RRD_DIR/p2p.rrd" ]]; then
    echo "rrd files already exist in $RRD_DIR; remove them first if you want a fresh start" >&2
    exit 1
fi

# chain.rrd — instantaneous gauges from /info
rrdtool create "$RRD_DIR/chain.rrd" \
    --start "$RRD_START" \
    --step "$RRD_STEP" \
    DS:full_height:GAUGE:120:0:U \
    DS:headers_height:GAUGE:120:0:U \
    DS:peers_count:GAUGE:120:0:U \
    DS:mempool_count:GAUGE:120:0:U \
    RRA:AVERAGE:0.5:1:1440 \
    RRA:AVERAGE:0.5:5:2016 \
    RRA:AVERAGE:0.5:30:1440 \
    RRA:AVERAGE:0.5:360:1460

# p2p.rrd — cumulative counters from /stats/p2p
BUCKETS=(headers blocks tx discovery sync_info snapshot control)

DS_ARGS=()
for bucket in "${BUCKETS[@]}"; do
    DS_ARGS+=("DS:${bucket}_in_count:COUNTER:120:0:U")
    DS_ARGS+=("DS:${bucket}_in_bytes:COUNTER:120:0:U")
    DS_ARGS+=("DS:${bucket}_out_count:COUNTER:120:0:U")
    DS_ARGS+=("DS:${bucket}_out_bytes:COUNTER:120:0:U")
done

rrdtool create "$RRD_DIR/p2p.rrd" \
    --start "$RRD_START" \
    --step "$RRD_STEP" \
    "${DS_ARGS[@]}" \
    RRA:AVERAGE:0.5:1:1440 \
    RRA:AVERAGE:0.5:5:2016 \
    RRA:AVERAGE:0.5:30:1440 \
    RRA:AVERAGE:0.5:360:1460

echo "Created RRDs in $RRD_DIR (step=${RRD_STEP}s, start=${RRD_START})"
