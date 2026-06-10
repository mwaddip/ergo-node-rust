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
  blacklisted — and, when bogus-address filtering is enabled (see
  `[network].filter_bogus_addresses` below), not bogus — it is
  recorded into PeerDb with `last_seen_ms = now`.
  Malformed Peers (cap exceeded, truncated body, invalid shortString)
  triggers a permanent ban of the source via the blacklist module —
  a genuine protocol violation, mirroring JVM
  `PeerSynchronizer.penalizeMaliciousPeer → PermanentPenalty`.
  **Bogus addresses in a `Peers` body do NOT penalize the source.**
  JVM 6.0.3 filters bogus addresses out of intake but does not ban
  the gossiper — relaying a peer list that contains CGNAT/private
  addresses is normal on a NAT'd network, not misbehavior. When
  filtering is enabled, bogus entries are silently dropped; non-bogus
  entries from the same body are recorded regardless.
- Bogus-address filtering is governed by
  `[network].filter_bogus_addresses` (default `true`). When `true`,
  bogus addresses (per the network-conditional classification below)
  are dropped from `Peers` intake and from GetPeers response
  selection — JVM 6.0.3 parity. When `false`, no address-sanity
  filtering is applied: every syntactically-valid address is ingested,
  may be selected for outbound fill, and is gossiped onward. The flag
  does not affect the malformed-Peers ban (unconditional) or the
  self-address filter (separate; the node never records or dials its
  own declared addresses).
- GetPeers response selection (the producer side) drops bogus
  addresses defensively before serialization when filtering is
  enabled — we never gossip a bogus address that ended up in PeerDb
  (legacy rows, or addresses ingested while the filter was off).

### Bogus address classification

Classification is **network-conditional**. The router is constructed
with a `Network` (mainnet or testnet), and the public
`is_bogus_address(addr, network)` entry point combines two
sub-predicates:

**Always-bogus** — never a legitimate peer, regardless of network. For
IPv4: loopback (127/8), link-local (169.254/16), multicast (224/4),
broadcast (255.255.255.255), unspecified (0.0.0.0), benchmark
(198.18/15, RFC 2544), reserved Class E (240/4, RFC 1112). For IPv6:
loopback (::1), unspecified (::), multicast (ff00::/8), link-local
(fe80::/10), IPv4-mapped (::ffff:0:0/96).

**Mainnet-only-bogus** — may legitimately appear on a testnet running
inside a private network (e.g. a developer's LAN), but never on
mainnet. For IPv4: RFC 1918 private ranges (10/8, 172.16/12,
192.168/16), CGN (100.64/10, RFC 6598), documentation (192.0.2/24,
198.51.100/24, 203.0.113/24, RFC 5737). For IPv6: unique-local
(fc00::/7), documentation (2001:db8::/32, RFC 3849).

A testnet-configured router treats the mainnet-only set as routable —
private LAN addresses can be ingested via `Peers` gossip and selected
as outbound fill candidates. A mainnet-configured router rejects them.

The router builds the classifier from `std::net::Ipv4Addr` /
`Ipv6Addr` predicates where they exist on stable Rust (`is_loopback`,
`is_link_local`, `is_multicast`, `is_broadcast`, `is_unspecified`,
`is_private`) and hand-rolls the rest (CGN, documentation, benchmark,
reserved 240/4, IPv6 link-local / ULA / mapped / documentation) with
bit-mask checks. The unstable `is_global` family is NOT used.
- PeerConnected: when a peer transitions to Active, its handshake
  `PeerSpec` is recorded into PeerDb if it has a declared address.
- Inv: not forwarded — recorded into the inv table for routing only.
- ModifierRequest: **local serve hook first** — the router is constructed
  with an optional store-blind callback
  `local_serve: Option<Arc<dyn Fn(u8, &[u8; 32]) -> Option<Vec<u8>> + Send + Sync>>`
  injected by the integrator (main crate wires it to the modifier store).
  For each requested id: callback `Some(bytes)` → emit
  `Action::Send { target: source, ModifierResponse(type, [(id, bytes)]) }`
  directly and do NOT relay that id. Callback `None` (or no callback
  configured, e.g. pure-proxy deployments) → legacy relay: routed via Inv
  table to the announcing peer if known, else forwarded to any available
  outbound peer as fallback (enables chain sync, where modifier IDs come
  from SyncInfo rather than Inv). Serve and relay are per-id exclusive —
  one request id never produces both a local response and a relayed
  request. Batching: locally-served ids within one incoming request MAY be
  grouped into a single ModifierResponse.
  History (2026-06-08): before the hook, a node with a complete block
  store relayed every request to other peers — rust-to-rust sync
  impossible (no JVM peer in the middle = no data). Discovered by the
  digest side-instance syncing against the live node.
- ModifierResponse: routed via request tracker to the requester.
- SyncInfo: routed via sync tracker (inbound↔outbound pairing).
- Unknown: forwarded to all peers of opposite direction.

## Invariant
Inv table, request tracker, and sync tracker are consistent with the peer registry. No references to unregistered peers. PeerDb may contain addresses that are not currently registered — that is its purpose.
