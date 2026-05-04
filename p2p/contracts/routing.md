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
- GetPeers: responded to directly from peer registry.
- Inv: forwarded to all peers of opposite direction.
- ModifierRequest: routed via Inv table to the announcing peer.
- ModifierResponse: routed via request tracker to the requester.
- SyncInfo: routed via sync tracker (inbound↔outbound pairing).
- Unknown: forwarded to all peers of opposite direction.

## Invariant
Inv table, request tracker, and sync tracker are consistent with the peer registry. No references to unregistered peers.
