#!/usr/bin/env python3
"""Backfill synthetic data into chain.rrd and p2p.rrd for demo graphs.

Run AFTER rrd-create.sh. Generates DEMO_HOURS of plausible synthetic
history ending now, so rrd-graph.sh has something to render against.

This is for development/demo only — production data comes from
rrd-update.sh polling a live node.

Env:
    RRD_DIR     directory containing chain.rrd and p2p.rrd (default ./rrd-data)
    DEMO_HOURS  hours of synthetic history to generate (default 6)
"""

import os
import random
import subprocess
import sys
import time

RRD_DIR = os.environ.get("RRD_DIR", "./rrd-data")
DEMO_HOURS = int(os.environ.get("DEMO_HOURS", "6"))
STEP = 60

# Bucket order MUST match rrd-create.sh and rrd-update.sh.
BUCKETS = ["headers", "blocks", "tx", "discovery", "sync_info", "snapshot", "control"]

# Per-step (count, bytes) for (in, out) per bucket. Rough plausible
# rates for a node syncing at ~50 blocks/min from ~30 peers.
PER_STEP = {
    "headers":        ((10, 2_500),   (5,  1_500)),
    "blocks":         ((10, 250_000), (2,  20_000)),
    "tx":             ((40, 4_000),   (40, 4_000)),
    "discovery": (( 1,    300),  ( 1,    300)),
    "sync_info":      ((30,    900),  (30,    900)),
    "snapshot":       (( 0,      0),  ( 0,      0)),
    "control":        (( 0,      0),  ( 0,      0)),
}

random.seed(42)

chain_rrd = f"{RRD_DIR}/chain.rrd"
p2p_rrd = f"{RRD_DIR}/p2p.rrd"

for path in (chain_rrd, p2p_rrd):
    if not os.path.exists(path):
        sys.stderr.write(f"RRD file not found: {path}\nRun rrd-create.sh first.\n")
        sys.exit(1)

end = int(time.time())
samples = DEMO_HOURS * 60
start = (end - samples * STEP) // STEP * STEP

counters = {(b, d, m): 0 for b in BUCKETS for d in ("in", "out") for m in ("count", "bytes")}

chain_updates = []
p2p_updates = []

height = 1_500_000
headers_ahead = 10

for i in range(samples):
    t = start + i * STEP

    # First 15 minutes: ramp-up phase (loading). After that, steady sync.
    if i < 15:
        peers = 1 + i * 24 // 15
        height_delta = 2
        traffic_factor = 0.1
    else:
        peers = max(0, 25 + random.randint(-5, 5))
        height_delta = random.randint(40, 60)
        traffic_factor = 1.0

    height += height_delta
    headers_height = height + headers_ahead
    mempool = max(0, 20 + random.randint(-15, 15))

    for bucket, ((inc, inb), (outc, outb)) in PER_STEP.items():
        counters[(bucket, "in",  "count")] += int(inc  * traffic_factor)
        counters[(bucket, "in",  "bytes")] += int(inb  * traffic_factor)
        counters[(bucket, "out", "count")] += int(outc * traffic_factor)
        counters[(bucket, "out", "bytes")] += int(outb * traffic_factor)

    chain_updates.append(f"{t}:{height}:{headers_height}:{peers}:{mempool}")

    p2p_vals = []
    for b in BUCKETS:
        p2p_vals.append(str(counters[(b, "in",  "count")]))
        p2p_vals.append(str(counters[(b, "in",  "bytes")]))
        p2p_vals.append(str(counters[(b, "out", "count")]))
        p2p_vals.append(str(counters[(b, "out", "bytes")]))
    p2p_updates.append(f"{t}:" + ":".join(p2p_vals))

subprocess.run(["rrdtool", "update", chain_rrd, *chain_updates], check=True)
subprocess.run(["rrdtool", "update", p2p_rrd, *p2p_updates], check=True)

print(f"Filled {samples} samples spanning {time.strftime('%Y-%m-%d %H:%M', time.localtime(start))}"
      f" to {time.strftime('%Y-%m-%d %H:%M', time.localtime(end))}")
