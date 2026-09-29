# Routing Layer Contract

The router decides what the node does with each message a peer sends. It
answers peer gossip and locally served modifier requests itself, and emits
received modifiers for validation. Everything else reaches its consumer —
sync, the mempool task, the snapshot and NiPoPoW handlers — through the event
subscriber (`facts/p2p-node.md` § `subscribe()`), which sees every event
before the router does.

**The router never forwards a message from one peer to another.** Every
`Action::Send` it emits answers the peer whose message triggered it. The node
is not a proxy.

## Module: `routing::router`

### `handle_event(event) -> Vec<Action>`
- **Precondition**: Peer IDs in events are registered (or being disconnected).
- **Postcondition**: Actions target only registered peers, and every
  `Action::Send` targets the source of the message being handled.

| Message | Router's action |
|---|---|
| `GetPeers` | Reply `Peers` to the source |
| `Peers` | Record the entries into PeerDb as hearsay |
| `ModifierRequest` | Answer what the local-serve hook has; nothing for the rest |
| `ModifierResponse` | `Action::Validate` for each modifier, carrying the source's peer id |
| `Inv`, `SyncInfo` | None: sync and the mempool task read them from the subscriber |
| Any other code | None: dropped, not penalized |

- GetPeers: parsed (body must be empty), then PeerDb is queried for
  up to 8 *observed* non-blacklisted peers (`PeerDb::observed` — peers
  we have completed a handshake with; hearsay is never gossiped onward),
  excluding the requester's own address. The selection is serialized as a `Peers` message and
  sent back to the source. An empty selection produces a `Peers` body
  with VLQ count = 0 (one byte: `0x00`).
- Peers: body parsed per `p2p-protocol.md::Peers wire format`. For
  each entry, if the entry's declared address is present and not
  blacklisted — and, when bogus-address filtering is enabled (see
  `[network].filter_bogus_addresses` below), not bogus — it is
  recorded into PeerDb as hearsay: `last_seen_ms = now`,
  `last_handshake_ms` untouched (`facts/p2p-peerdb.md` § Observed and
  hearsay).
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
  may be selected for outbound fill, and — once we have handshaked with
  it — is gossiped onward. The flag
  does not affect the malformed-Peers ban (unconditional) or the
  self-address filter (separate; the node never records or dials its
  own declared addresses).
- GetPeers response selection (the producer side) drops bogus
  addresses defensively before serialization when filtering is
  enabled — we never gossip a bogus address that ended up in PeerDb
  (legacy rows, or addresses ingested while the filter was off).

- ModifierRequest: a peer connected through a **light** listener gets
  nothing for a block-related type (101, 102, 104, 108): light listeners are
  gossip-only. Otherwise every requested id goes to the local-serve hook, a
  store-blind callback
  `local_serve: Option<Arc<dyn Fn(u8, &[u8; 32]) -> Option<Vec<u8>> + Send + Sync>>`
  that the integrator injects. The main crate answers block sections from the
  modifier store and transactions from the mempool. Hits go back to the source
  in `ModifierResponse` messages, one per request unless the encoded body
  would exceed the serve batch cap. A miss gets no answer, as in the JVM,
  which serves what it has and ignores the rest
  (`ErgoNodeViewSynchronizer.modifiersReq`); the requester asks another peer.
  The parse-layer object cap bounds the hook's cost: at most 400 lookups per
  request.
- ModifierResponse: each modifier becomes an `Action::Validate` with the
  source's peer id (`facts/p2p-node.md` § Router: Action::Validate). Nothing
  is sent back or onward.
- Unknown code: dropped. It is neither forwarded nor penalized, because a
  newer protocol version may send codes we don't know (the input-block
  messages of the JVM's 6.5.0 line, for one). The JVM's `MessageSerializer`
  throws on an unknown code, which stalls that connection for good; dropping
  the frame keeps it. Codes the node handles outside the typed codec — UTXO
  snapshot 76–81, NiPoPoW 90–91 — reach their handlers through the
  subscriber, like every other event.
- PeerConnected: when a peer transitions to Active, its handshake
  `PeerSpec` is recorded into PeerDb per `facts/p2p-peerdb.md`
  § Observed and hearsay: outbound — the dialed address is observed, the
  declared address observed if on the same IP and hearsay otherwise;
  inbound — the declared address is observed if on the remote IP and
  hearsay otherwise, and nothing is recorded without a declared address.
- PeerDisconnected: the peer's registry entry is removed.

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

## Invariant

The router's only per-peer state is the peer registry, and the registry holds
exactly the registered peers. PeerDb may contain addresses that are not
currently registered — that is its purpose.
