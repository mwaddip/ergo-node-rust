# Routing Layer Contract

## Module: `routing::inv_table`

### `record(modifier_id, peer)`
- **Postcondition**: `lookup(modifier_id) == Some(peer)`.

### `lookup(modifier_id) -> Option<PeerId>`
- Returns the peer that most recently announced this modifier.

### `purge_peer(peer)`
- **Postcondition**: No entry maps to `peer`.
- **Invariant**: The Inv table never contains entries for disconnected peers.

## Module: `routing::tracker`

### RequestTracker

#### `record(modifier_id, requester)`
- **Postcondition**: `lookup(modifier_id) == Some(requester)`.

#### `fulfill(modifier_id) -> Option<PeerId>`
- **Postcondition**: Entry is removed. Returns the requester.

#### `purge_peer(peer)`
- **Postcondition**: No entry maps to `peer`.

### SyncTracker

#### `pair(inbound, outbound)`
- **Postcondition**: `outbound_for(inbound) == Some(outbound)` AND `inbound_for(outbound) == Some(inbound)`.
- **Invariant**: Pairings are bidirectionally consistent.

#### `purge_peer(peer)`
- **Postcondition**: No pairing references `peer` on either side.

## Module: `routing::router`

### `handle_event(event) -> Vec<Action>`
- **Precondition**: Peer IDs in events are registered (or being disconnected).
- **Postcondition**: Actions target only registered peers.
- Mode filtering: Light mode drops SyncInfo and block-related ModifierRequests.
- GetPeers: parsed (body must be empty), then PeerDb is queried for
  up to 8 recently-seen non-blacklisted peers, excluding the requester's
  own address. The selection is serialized as a `Peers` message and
  sent back to the source. An empty selection produces a `Peers` body
  with VLQ count = 0 (one byte: `0x00`).
- Peers: body parsed per `p2p-protocol.md::Peers wire format`. For
  each entry, if the entry's declared address is present and not
  blacklisted, it is recorded into PeerDb with `last_seen_ms = now`.
  Malformed Peers (cap exceeded, truncated body, invalid shortString)
  triggers a permanent ban of the source via the blacklist module.
- PeerConnected: when a peer transitions to Active, its handshake
  `PeerSpec` is recorded into PeerDb if it has a declared address.
- Inv: not forwarded — recorded into the inv table for routing only.
- ModifierRequest: routed via Inv table to the announcing peer if known. If no Inv entry exists, forwarded to any available outbound peer as fallback (enables chain sync, where modifier IDs come from SyncInfo rather than Inv).
- ModifierResponse: routed via request tracker to the requester.
- SyncInfo: routed via sync tracker (inbound↔outbound pairing).
- Unknown: forwarded to all peers of opposite direction.

## Invariant
Inv table, request tracker, and sync tracker are consistent with the peer registry. No references to unregistered peers. PeerDb may contain addresses that are not currently registered — that is its purpose.
