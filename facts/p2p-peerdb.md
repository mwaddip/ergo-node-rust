# Peer Database Contract

## Module: `peer_db`

In-memory peer registry that backs `GetPeers` responses, populates
candidates for the outbound manager, and feeds `P2pNode::all_peers`.
A separate `PeerStorage` trait owns persistence so the store crate
can back it with redb without p2p knowing about disks.

## Types

### `PeerRecord`
- `address: SocketAddr` — declared address (deduplication key).
- `last_seen_ms: u64` — Unix epoch ms of most recent successful
  interaction (handshake, gossip from a third party, manual add).
- `agent_name: String` — peer's agent string (`PeerSpec.agent`).
- `node_name: String` — peer's friendly name (`PeerSpec.name`).
- `version: (u8, u8, u8)` — peer's protocol version.
- `features: Vec<(u8, Vec<u8>)>` — preserved opaque features.

A `PeerRecord` originates from either:
1. **Our own handshake** with the peer (authoritative — `last_seen_ms = now`).
2. **A `Peers` message** from another peer (hearsay — `last_seen_ms = now`
   at time of receipt; address may be unreachable).

The PeerDb does not distinguish; both feed the same table.

## Module: `peer_db::PeerDb`

### `record(record: PeerRecord)`
- **Precondition**: `record.address` is not on the blacklist.
- **Postcondition**: An entry for `record.address` exists with
  `last_seen_ms` set to the maximum of any prior value and the new
  value. Other fields are overwritten from the new record.
- **Postcondition**: If insertion would exceed the soft cap, the
  entry with the smallest `last_seen_ms` is evicted before insertion.
- **Side effect**: `PeerStorage::put` is called with the resulting
  record. Eviction calls `PeerStorage::delete` for the displaced entry.

### `forget(addr: SocketAddr)`
- **Postcondition**: No entry for `addr` exists.
- **Side effect**: `PeerStorage::delete(addr)`.

### `get(addr: SocketAddr) -> Option<PeerRecord>`
- Returns the entry if present.

### `recent(limit: usize, exclude_addrs: &HashSet<SocketAddr>) -> Vec<PeerRecord>`
- Returns up to `limit` entries with the largest `last_seen_ms`,
  excluding any address in `exclude_addrs` and any blacklisted address.
- Used to build `Peers` responses and to pick dial candidates.

### `all() -> Vec<PeerRecord>`
- Returns every entry. Used by `/peers/all`.

### `count() -> usize`
- Number of entries.

### `cap: usize`
- Soft cap on entries. Default 1000. Configurable via p2p config.

## Trait: `PeerStorage`

```rust
pub trait PeerStorage: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn load_all(&self) -> Result<Vec<PeerRecord>, Self::Error>;
    fn put(&self, record: &PeerRecord) -> Result<(), Self::Error>;
    fn delete(&self, addr: SocketAddr) -> Result<(), Self::Error>;
}
```

- **`load_all`**: Called once at `PeerDb` construction to repopulate
  the in-memory table. Returns every persisted record. Order does not
  matter — PeerDb sorts on demand.
- **`put`**: Write-through. Called on every `record()` (including
  updates). Implementations should be fast (single redb write) and
  must not block longer than a few ms.
- **`delete`**: Called on `forget()` and on eviction.

The trait is implemented in the main crate by an adapter over the
store crate's `ModifierStore::put_peer` / `delete_peer` / `list_peers`
methods.

### Failure handling
- `put` failures are logged and silently swallowed by `PeerDb`. A
  failed write demotes the in-memory state to ephemeral but does not
  abort the gossip path. Operators see the failure in logs.
- `load_all` failure on startup is fatal — wired by the main crate
  (let it crash; operator restarts).

## Blacklist integration

`PeerDb` holds a reference to the blacklist (`Arc<Blacklist>` from
`p2p/src/blacklist.rs`). On every `record()`:
1. If the address is currently blacklisted, the record is dropped
   silently (no side effect, no error).
2. If a peer becomes blacklisted later, its entry stays in the DB
   but is filtered from `recent()` results. Optional pruning can be
   wired separately.

## Invariants

- `PeerDb::count() <= cap`.
- For every in-memory entry, `PeerStorage::put` has been called at
  least once since startup (modulo `put` failures, which are logged).
- `recent()` never returns blacklisted addresses or addresses in the
  exclusion set.
- A successful `forget()` followed by `record()` for the same
  address produces a fresh entry with the new `last_seen_ms`.
