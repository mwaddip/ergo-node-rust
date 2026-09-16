# Peer Database Contract

## Module: `peer_db`

In-memory peer registry that backs `GetPeers` responses, populates
candidates for the outbound manager, and feeds `P2pNode::all_peers`.
A separate `PeerStorage` trait owns persistence so the store crate
can back it with redb without p2p knowing about disks.

## Types

### `PeerRecord`
- `address: SocketAddr` — declared address (deduplication key).
- `last_seen_ms: u64` — Unix epoch ms of the most recent *mention* of
  this address: a handshake we completed, a `Peers` entry from a third
  party, a manual add.
- `last_handshake_ms: u64` — Unix epoch ms of the most recent handshake
  **we completed with this address**. `0` means never: the entry is
  hearsay. Only the handshake path writes it; gossip cannot raise it.
- `agent_name: String` — peer's agent string (`PeerSpec.agent`).
- `node_name: String` — peer's friendly name (`PeerSpec.name`).
- `version: (u8, u8, u8)` — peer's protocol version.
- `features: Vec<(u8, Vec<u8>)>` — preserved opaque features.

## Observed and hearsay

A `PeerRecord` originates from either:
1. **Our own handshake** with the peer — *observed*, `last_seen_ms = now`
   and `last_handshake_ms = now`, for the address the connection proves
   listens. On an **outbound** connection that is the address we dialed,
   always; the declared address is observed too if its IP equals the
   dialed IP (a port claim on a host we just talked to), and hearsay
   otherwise. On an **inbound** connection it is the declared address, if
   its IP equals the connection's remote IP; a declared address on a
   different host is hearsay, since the connection proves nothing about
   that host; and no declared address records nothing — the remote socket
   is an ephemeral port, not a listening address (the JVM stores nothing
   for that case either).
2. **A `Peers` message** from another peer — *hearsay*. `last_seen_ms =
   now` at receipt; `last_handshake_ms` is left as it was (`0` for a new
   entry). The address may be unreachable, or may not be a node at all.

Both feed the same table, and the table keeps the distinction: a peer's
claim that an address exists is a label the node cannot recompute
(`facts/receive-path.md`), so it never counts as an observation. Hearsay is
what the node *dials*; only observation is what it *tells others about*
and what it *keeps* under pressure.

## Module: `peer_db::PeerDb`

### Constructor

`PeerDb::new(storage, blacklist, cap, self_addresses)` takes a
`HashSet<SocketAddr>` of addresses considered "self" — populated by
the main session from each listener's declared address (post-UPnP,
post-IPv6-auto-detect). Used to drop self-loop candidates that
peers gossip back to us. The set is captured by value at
construction; the PeerDb does not track address changes after
that.

### `record(record: PeerRecord)`
- **Precondition**: `record.address` is not on the blacklist.
- **Postcondition**: If `record.address ∈ self_addresses` or is on
  the blacklist, the call is a no-op (no in-memory insert, no
  storage write).
- **Postcondition**: Otherwise, an entry for `record.address`
  exists with `last_seen_ms` and `last_handshake_ms` each set to the
  maximum of any prior value and the new value — independently, so a
  gossip record (`last_handshake_ms = 0`) never lowers a handshake
  timestamp. Other fields are overwritten from the new record.
- **Postcondition**: If insertion would exceed the soft cap, one entry
  is evicted before insertion: the hearsay entry
  (`last_handshake_ms == 0`) with the smallest `last_seen_ms`; only when
  no hearsay entry exists, an observed entry — from the address group
  (below) holding the most observed entries if that count exceeds one,
  the smallest `last_handshake_ms` within it (groups tied for most: the
  smallest `last_handshake_ms` among the tied groups); otherwise the
  smallest `last_handshake_ms` overall. Gossip can fill the table but never push
  out a peer we have actually talked to, and a single host or site that
  mints observed entries by handshaking under many declared ports only
  ever evicts its own.
- **Side effect**: `PeerStorage::put` is called with the resulting
  record. Eviction calls `PeerStorage::delete` for the displaced entry.

### `forget(addr: SocketAddr)`
- **Postcondition**: No entry for `addr` exists.
- **Side effect**: `PeerStorage::delete(addr)`.

### `get(addr: SocketAddr) -> Option<PeerRecord>`
- Returns the entry if present.

### `observed(limit: usize, exclude_addrs: &HashSet<SocketAddr>) -> Vec<PeerRecord>`
- Returns up to `limit` entries with `last_handshake_ms > 0`, largest
  `last_handshake_ms` first, excluding any address in `exclude_addrs` and
  any blacklisted address. Hearsay entries are never returned.
- Used to build `Peers` responses. JVM parity: `PeerManager.SeenPeers`
  propagates only peers with a completed handshake; an address we have
  only heard of is never vouched for to a third party.

### `dial_candidate(exclude_addrs: &HashSet<SocketAddr>, connected: &[SocketAddr], eligible: impl Fn(&PeerRecord) -> bool) -> Option<PeerRecord>`
- Picks one entry uniformly at random from all entries — hearsay
  included — that are not in `exclude_addrs`, not blacklisted, and for
  which `eligible` returns true (the caller's bogus-address filter, per
  `facts/p2p-routing.md`), preferring entries whose address group (below)
  matches no address in `connected`; when no such entry exists, from all
  eligible entries. `None` if nothing is eligible.
- Used by the outbound-fill dialer. JVM parity:
  `PeerManager.RandomPeerExcluding` — random choice with IP-group
  diversity. That is what makes eclipse-by-gossip expensive: an attacker
  controls which addresses we hear about, not which one we pick, nor how
  many of our slots its group may hold.

### `all() -> Vec<PeerRecord>`
- Returns every entry. Used by `/peers/all`.

### `count() -> usize`
- Number of entries.

### `cap: usize`
- Soft cap on entries. Default 1000. Configurable via p2p config.

### Address group

The unit both dial diversity and eviction crowding reason in: the first
two bytes of an IPv4 address (a /16) and the first eight of an IPv6
address (a /64, one customer allocation). The JVM's `getIpGroup` takes the
first two bytes of either family; for IPv6 that is a /16 and groups most
of a continent, so it would make diversity meaningless and crowding
unbounded on the family this node is built for. The divergence is
deliberate.

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
  matter — PeerDb sorts on demand. `PeerDb::new` filters loaded
  records against `self_addresses` before populating the in-memory
  set; the disk rows are NOT deleted (a self-address today may
  legitimately be a different host tomorrow — e.g. when an IPv6
  prefix changes).
- **`put`**: Write-through. Called on every `record()` (including
  updates). Implementations should be fast (single redb write) and
  must not block longer than a few ms.
- **`delete`**: Called on `forget()` and on eviction.
- **Record encoding** is the main crate's `PeerStorageAdapter`
  (`src/peer_storage_adapter.rs`), the encode/decode bridge between this
  schema and the store's opaque bytes (`facts/store.md` `put_peer`). It
  writes `last_handshake_ms` as a trailing field; rows written before
  that field existed decode with `last_handshake_ms = 0` — every
  previously known peer starts as hearsay after upgrade and is promoted
  on its next handshake.

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
- `observed()` and `dial_candidate()` never return blacklisted addresses
  or addresses in the exclusion set.
- `observed()` never returns an entry with `last_handshake_ms == 0`.
- Eviction never removes an observed entry while a hearsay entry exists.
- A successful `forget()` followed by `record()` for the same
  address produces a fresh entry with the new `last_seen_ms`.
