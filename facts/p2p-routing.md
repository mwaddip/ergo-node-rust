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
  blacklisted and is not bogus (see below), it is recorded into
  PeerDb with `last_seen_ms = now`.
  Malformed Peers (cap exceeded, truncated body, invalid shortString)
  triggers a permanent ban of the source via the blacklist module.
  **A `Peers` body containing one or more bogus addresses also
  triggers a permanent ban of the source** — there is no legitimate
  reason to gossip an unroutable address to other peers; doing so
  is either a buggy implementation or deliberate spam. Non-bogus
  entries from the same body are still recorded (we don't punish
  the gossiped peers, only the gossiper).
- GetPeers response selection (the producer side) drops bogus
  addresses defensively before serialization — we never gossip a
  bogus address even if it ended up in our PeerDb somehow (legacy
  rows from before the filter, or a future regression).

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
- ModifierRequest: routed via Inv table to the announcing peer if known. If no Inv entry exists, forwarded to any available outbound peer as fallback (enables chain sync, where modifier IDs come from SyncInfo rather than Inv).
- ModifierResponse: routed via request tracker to the requester.
- SyncInfo: routed via sync tracker (inbound↔outbound pairing).
- Unknown: forwarded to all peers of opposite direction.

## Invariant
Inv table, request tracker, and sync tracker are consistent with the peer registry. No references to unregistered peers. PeerDb may contain addresses that are not currently registered — that is its purpose.
