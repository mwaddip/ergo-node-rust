# Stats Endpoint Contract

Version: 1.0.0

Operator-facing time-series metrics endpoint. Cumulative counters
suitable for RRD `COUNTER`-style consumption, Prometheus exporters, or
ad-hoc `curl` inspection. Separate from the public REST API.

## Purpose

The public REST API (`facts/api.md`) serves wallets, light clients, and
chain consumers. Stats are different — operator-only, narrower in
scope, higher cardinality. Mixing them on the same bind invites
accidental exposure of operator internals to anyone reachable on the
node's public API port.

The stats endpoint exists to answer questions an operator graphs:

- How much of my upstream is `ModifierResponse` payload vs `Inv`
  churn?
- Did peer-discovery gossip volume change after v0.5.3's filter
  shipped?
- Is my snapshot-sync (codes 76–81) using bandwidth I forgot about?

Discrete events (reorgs, penalties, recovery phases) belong in the
journal-events contract (`facts/journal-events.md`), not here.

## Bind

Default: **`127.0.0.1:9055`**, opt-in via a `[stats]` section in
`ergo.toml`. With the section absent, the stats listener is **not
started** and the port stays closed.

```toml
[stats]
# Bind address for the operator stats endpoint.
# Default loopback-only. Setting a non-loopback bind grants
# network-wide read access to traffic counters — no auth.
bind_address = "127.0.0.1:9055"
```

A non-loopback bind logs a one-line WARN at startup so it's visible
in the journal: `stats endpoint exposed on non-loopback bind=...`.

The endpoint never binds to `0.0.0.0` by default. Ever.

## Versioning

The endpoint advertises its own schema version in every response:

```json
{ "statsVersion": "1.0", ... }
```

Clients that don't recognize the major version SHOULD refuse to
parse. Additive changes (new counter keys, new modifier types in
existing sections) do NOT bump the major.

## Endpoint: `GET /stats/p2p`

Cumulative counters since process start. No authentication (loopback
bind is the security mechanism).

**Response 200:**

```json
{
  "statsVersion": "1.0",
  "since": 1715792400,
  "messages": {
    "handshake":         { "in": { "count": N, "bytes": N }, "out": { "count": N, "bytes": N } },
    "get_peers":         { ... },
    "peers":             { ... },
    "sync_info":         { ... },
    "inv": {
      "header":              { ... },
      "transaction":         { ... },
      "block_transactions":  { ... },
      "ad_proofs":           { ... },
      "extension":           { ... }
    },
    "modifier_request":  { /* same modifier-type keys as inv */ },
    "modifier_response": { /* same modifier-type keys as inv */ },
    "snapshot": {
      "request_manifest":    { ... },
      "manifest":            { ... },
      "request_subtree":     { ... },
      "subtree":             { ... },
      "request_utxo_chunk":  { ... },
      "utxo_chunk":          { ... }
    },
    "unknown": { ... }
  }
}
```

### Field semantics

- `since`: Unix seconds at which the counters started accruing
  (process start). RRD consumers detect counter resets by comparing
  this against the previous sample's `since`.
- `count`: number of messages observed in this direction.
- `bytes`: total wire bytes of those messages, including the framing
  header (magic + code + length + checksum + body). Operators are
  graphing real link utilization, not application payload.
- `in` / `out`: direction relative to this node. Inbound = received,
  outbound = sent.

### Modifier-type keys (under `inv`, `modifier_request`,
`modifier_response`)

| Key | Modifier byte | Semantic |
|---|---|---|
| `header` | 1 | Block header |
| `transaction` | 2 | Mempool transaction |
| `block_transactions` | 3 | Block transactions section |
| `ad_proofs` | 4 | AVL+ authenticated data proofs |
| `extension` | 5 | Block extension section |

New modifier types defined upstream get new keys in additive minor
versions.

### Snapshot keys

The six snapshot-sync codes (76–81) map to descriptive names rather
than numeric labels. Mapping documented in
`facts/p2p-protocol.md`. Stable across versions.

### `unknown`

Catches any message code this node doesn't recognize. A non-zero
`unknown.in.count` means peers are sending codes from a newer protocol
revision than this build supports. Useful as an upgrade-pressure
signal.

## Companion tooling

A reference rrdtool wrapper script ships under `tools/rrd-update.sh`.
It is operator-editable — bucket aggregation, sample interval, and
RRD schema are policy decisions that don't belong in the node binary.
The contract here defines what counters the node exposes; the wrapper
decides which to graph.

## Stability

| Aspect | Stability |
|---|---|
| Field naming (snake_case keys above) | Stable across minor versions |
| Counter semantics (cumulative since process start) | Stable across major versions |
| Loopback-only default | Stable across major versions |
| Set of message-code keys | Additive only; removals bump major |
| `statsVersion` field present | Stable across major versions |
| Sub-second sample alignment | Not promised. Operator polls at their own cadence. |

## Out of scope

- Authentication, TLS — loopback is the security boundary.
- Histograms, percentiles, derived rates — operators compute these
  from cumulative deltas. The node does not pre-aggregate.
- Per-peer breakdowns — `/peers/all` exists for that. The stats
  endpoint is aggregate-only.
- Other counter families (chain, mempool, mining) — out of scope for
  v1.0; may be added as `/stats/<family>` endpoints in later
  versions, each with the same `statsVersion` contract.
