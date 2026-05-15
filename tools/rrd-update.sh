#!/usr/bin/env bash
# Production updater — polls /stats/p2p and /info, writes to RRDs.
# Run from cron at the sample interval (default 60s).
#
# Bucket order MUST match rrd-create.sh and rrd-demo-fill.sh:
#   headers, blocks, tx, peer_discovery, sync_info, snapshot, control
#
# Env:
#   RRD_DIR     directory containing chain.rrd / p2p.rrd (default ./rrd-data)
#   STATS_URL   stats endpoint URL (default http://127.0.0.1:9055/stats/p2p)
#   INFO_URL    info endpoint URL (default http://127.0.0.1:9053/info)

set -euo pipefail

RRD_DIR="${RRD_DIR:-./rrd-data}"
STATS_URL="${STATS_URL:-http://127.0.0.1:9055/stats/p2p}"
INFO_URL="${INFO_URL:-http://127.0.0.1:9053/info}"

CHAIN_RRD="$RRD_DIR/chain.rrd"
P2P_RRD="$RRD_DIR/p2p.rrd"

if [[ ! -e "$CHAIN_RRD" || ! -e "$P2P_RRD" ]]; then
    echo "rrd files not found in $RRD_DIR; run rrd-create.sh first" >&2
    exit 1
fi

T=$(date +%s)

# /info → chain.rrd
INFO=$(curl -sf --max-time 5 "$INFO_URL")
read -r FULL_HEIGHT HEADERS_HEIGHT PEERS_COUNT MEMPOOL_COUNT \
    < <(jq -r '[.fullHeight, .headersHeight, .peersCount, .unconfirmedCount] | @tsv' <<< "$INFO")

rrdtool update "$CHAIN_RRD" "$T:$FULL_HEIGHT:$HEADERS_HEIGHT:$PEERS_COUNT:$MEMPOOL_COUNT"

# /stats/p2p → p2p.rrd
STATS=$(curl -sf --max-time 5 "$STATS_URL")

# Compute bucket sums per (direction, metric). Order must match the DS order
# in rrd-create.sh: for each bucket, emit in_count, in_bytes, out_count,
# out_bytes; iterate buckets in the fixed order.
SUMS=$(jq -r '
    def at(p; d; m): try (p[d][m] // 0) catch 0;
    def mt(m; t; d; mtr): at(m.inv[t]; d; mtr) + at(m.modifier_request[t]; d; mtr) + at(m.modifier_response[t]; d; mtr);
    def bucket(d; mtr):
        .messages as $m | [
            mt($m; "header"; d; mtr),
            (mt($m; "block_transactions"; d; mtr) + mt($m; "ad_proofs"; d; mtr) + mt($m; "extension"; d; mtr)),
            mt($m; "transaction"; d; mtr),
            (at($m.get_peers; d; mtr) + at($m.peers; d; mtr)),
            at($m.sync_info; d; mtr),
            (($m.snapshot // {}) | to_entries | map(.value[d][mtr] // 0) | (add // 0)),
            (at($m.handshake; d; mtr) + at($m.unknown; d; mtr))
        ];

    [bucket("in"; "count"), bucket("in"; "bytes"), bucket("out"; "count"), bucket("out"; "bytes")]
    | transpose | flatten | join(":")
' <<< "$STATS")

rrdtool update "$P2P_RRD" "$T:$SUMS"
