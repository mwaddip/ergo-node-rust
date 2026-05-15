#!/usr/bin/env bash
# Generate example PNG graphs from chain.rrd and p2p.rrd.
#
# Four graphs:
#   sync-height.png   — block height over time
#   sync-rate.png     — derived sync rate (blocks/sec)
#   traffic-count.png — stacked-area inbound traffic by bucket (msg/s)
#   traffic-bytes.png — stacked-area inbound traffic by bucket (bytes/s)
#
# rrdtool's right-axis is only an affine relabel of the left axis, so two
# series with different scales need two PNGs. The dashboard layer (HTML,
# whatever) stacks them.
#
# Env:
#   RRD_DIR    rrd file directory (default ./rrd-data)
#   OUT_DIR    output directory for PNGs (default ./rrd-graphs)
#   SPAN       graph time range (default -6h)
#   WIDTH      width in pixels (default 800)
#   HEIGHT     height in pixels (default 250)

set -euo pipefail

RRD_DIR="${RRD_DIR:-./rrd-data}"
OUT_DIR="${OUT_DIR:-./rrd-graphs}"
SPAN="${SPAN:--6h}"
WIDTH="${WIDTH:-800}"
HEIGHT="${HEIGHT:-250}"

CHAIN="$RRD_DIR/chain.rrd"
P2P="$RRD_DIR/p2p.rrd"

for path in "$CHAIN" "$P2P"; do
    if [[ ! -e "$path" ]]; then
        echo "rrd file not found: $path" >&2
        exit 1
    fi
done

mkdir -p "$OUT_DIR"

# 1a. Block height over time.
rrdtool graph "$OUT_DIR/sync-height.png" \
    --start "$SPAN" --end now \
    --width "$WIDTH" --height "$HEIGHT" \
    --title "Block height" \
    --vertical-label "height" \
    DEF:height="$CHAIN":full_height:AVERAGE \
    LINE2:height#0066cc:"full height" \
    > /dev/null

# 1b. Derived sync rate (blocks per second).
rrdtool graph "$OUT_DIR/sync-rate.png" \
    --start "$SPAN" --end now \
    --width "$WIDTH" --height "$HEIGHT" \
    --title "Sync rate" \
    --vertical-label "blocks / sec" \
    DEF:height="$CHAIN":full_height:AVERAGE \
    'CDEF:rate=height,PREV(height),-,60,/' \
    LINE2:rate#cc3333:"blocks / sec" \
    > /dev/null

# 2. Inbound message counts, stacked by bucket.
rrdtool graph "$OUT_DIR/traffic-count.png" \
    --start "$SPAN" --end now \
    --width "$WIDTH" --height "$HEIGHT" \
    --title "P2P inbound traffic by message type (msg/s)" \
    --vertical-label "msg / sec" \
    DEF:headers="$P2P":headers_in_count:AVERAGE \
    DEF:blocks="$P2P":blocks_in_count:AVERAGE \
    DEF:tx="$P2P":tx_in_count:AVERAGE \
    DEF:pd="$P2P":discovery_in_count:AVERAGE \
    DEF:sync="$P2P":sync_info_in_count:AVERAGE \
    DEF:snap="$P2P":snapshot_in_count:AVERAGE \
    DEF:ctrl="$P2P":control_in_count:AVERAGE \
    AREA:headers#3366cc:"headers" \
    STACK:blocks#dc3912:"block sections" \
    STACK:tx#ff9900:"transactions" \
    STACK:pd#109618:"peer discovery" \
    STACK:sync#990099:"sync info" \
    STACK:snap#0099c6:"snapshot" \
    STACK:ctrl#dd4477:"control/other" \
    > /dev/null

# 3. Inbound bytes, stacked by bucket. Base 1024 for KB/MB labels.
rrdtool graph "$OUT_DIR/traffic-bytes.png" \
    --start "$SPAN" --end now \
    --width "$WIDTH" --height "$HEIGHT" \
    --title "P2P inbound traffic by message type (bytes/s)" \
    --vertical-label "bytes / sec" \
    --base 1024 \
    DEF:headers="$P2P":headers_in_bytes:AVERAGE \
    DEF:blocks="$P2P":blocks_in_bytes:AVERAGE \
    DEF:tx="$P2P":tx_in_bytes:AVERAGE \
    DEF:pd="$P2P":discovery_in_bytes:AVERAGE \
    DEF:sync="$P2P":sync_info_in_bytes:AVERAGE \
    DEF:snap="$P2P":snapshot_in_bytes:AVERAGE \
    DEF:ctrl="$P2P":control_in_bytes:AVERAGE \
    AREA:headers#3366cc:"headers" \
    STACK:blocks#dc3912:"block sections" \
    STACK:tx#ff9900:"transactions" \
    STACK:pd#109618:"peer discovery" \
    STACK:sync#990099:"sync info" \
    STACK:snap#0099c6:"snapshot" \
    STACK:ctrl#dd4477:"control/other" \
    > /dev/null

echo "Wrote graphs to $OUT_DIR"
