# P2P Node API Contract

## Module: `node::P2pNode`

The handle to a running P2P layer. Created by `P2pNode::start()`. The P2P layer runs as background tokio tasks — the caller owns the runtime.

### `start(config, modifier_sink, peer_storage) -> Result<P2pNode>`
- **Precondition**: Called within a tokio runtime.
- **Postcondition**: Listeners, outbound connections, keepalive,
  outbound-fill dialer, and event loop are spawned as background tasks.
  Returns immediately.
- If `modifier_sink` is `Some`, every modifier from a `ModifierResponse` is sent to the channel as `(modifier_type, id, data, peer_id)` via the `Action::Validate` mechanism. `peer_id` is `Option<u64>` — `Some(id)` for peer-delivered modifiers, `None` for locally-ingested ones. The P2P layer never blocks on validation.
- `peer_storage: Box<dyn PeerStorage>` provides persistent backing
  for the in-memory PeerDb. `start` calls `peer_storage.load_all()`
  to repopulate the table, then constructs the PeerDb and hands it
  to the router and outbound manager. On `load_all` failure, `start`
  returns the error.

### `peer_count() -> usize`
- Returns the number of currently connected peers (inbound + outbound).

### `outbound_peers() -> Vec<PeerId>`
- Returns IDs of currently connected outbound peers.

### `inbound_peers() -> Vec<PeerId>`
- Returns IDs of currently connected inbound peers.

### `latency_stats() -> Option<LatencyStats>`
- Returns latency statistics for modifier responses, if any data collected.

### `send_to(peer, message) -> Result<()>`
- **Precondition**: `peer` is a currently connected peer.
- **Postcondition**: Message is serialized and queued for delivery.
- Returns error if peer is unknown or disconnected.
- Does not guarantee delivery — the peer may disconnect before the message is sent.

### `broadcast_outbound(message)`
- **Postcondition**: Message is queued for delivery to all currently connected outbound peers.
- Best-effort: silently skips peers whose send channels are full or disconnected.

### `subscribe() -> Receiver<ProtocolEvent>`
- Returns a channel receiver that receives a copy of every protocol event (incoming messages, peer connect/disconnect) before the router processes it.
- Bounded channel (256): if the subscriber falls behind, events are dropped (not blocking the event loop).
- The subscriber sees raw events — the router may subsequently drop, reroute, or transform them.

### `all_peers() -> Vec<PeerEntry>` (async)
- Returns information about all peers in the PeerDb, with connection
  state overlaid from the live router state.
- Each `PeerEntry` includes:
  - `address: SocketAddr` — peer's declared address (the PeerDb key).
  - `agent_name: Option<String>` — `Some` whenever the PeerDb has an
    agent string for the entry. May come from our own handshake or
    from a gossiped Peers entry.
  - `last_seen_ms: Option<u64>` — Unix epoch ms of `PeerRecord.last_seen_ms`.
    `None` only for entries with no recorded contact time (currently
    none in practice — PeerDb sets it on every `record`).
  - `connection_type: Option<ConnectionType>` — `Outgoing` / `Incoming`
    for addresses currently connected per the router; `None` for
    PeerDb entries we are not currently connected to.
- The set is the PeerDb union with the currently-connected addresses
  (the latter rare-but-possible case is an inbound peer whose
  declared address differs from its observed socket — both addresses
  appear: the declared one in the PeerDb entry, the observed one in
  the connection overlay).
- Used by the API layer for `GET /peers/all`.

### `network_status() -> NetworkStatus` (async)
- Returns:
  - `last_incoming_message_ms: Option<u64>` — Unix epoch ms of the last received protocol message. `None` if no messages have arrived since startup. Reads from a tracker that updates on every incoming message in the event loop.
  - `current_network_time_ms: u64` — current Unix epoch ms (`SystemTime::now()`).
- Used by the API layer for `GET /peers/status`.

### `blacklisted_peers() -> Vec<SocketAddr>` (async)
- Returns the addresses of all peers currently penalty-banned by this node.
- Reads from the penalty store. Does NOT include temporarily rate-limited peers.
- Used by the API layer for `GET /peers/blacklisted`.

### `queue_outbound_connection(addr: SocketAddr) -> Result<(), String>` (async)
- Fire-and-forget request to initiate an outbound connection to `addr`.
- Returns `Ok(())` when the request is successfully queued (not when the connection completes).
- Returns `Err(reason)` for:
  - Unroutable / loopback addresses (when policy forbids)
  - Blacklisted peer
  - Already connected to this address (no-op)
  - Outbound queue full (rare under normal operation)
- The outbound manager picks up the queued request asynchronously.
- Used by the API layer for `POST /peers/connect`.

## Router: Action::Validate

The router emits `Action::Validate { modifier_type, id, data, peer_id }` for each modifier in a `ModifierResponse`. `peer_id` identifies which peer sent the modifier, enabling penalty attribution when validation fails downstream. The event loop dispatches these to the `modifier_sink` channel as `(modifier_type, id, data, Some(peer_id.0))` via `try_send` (non-blocking). If no sink is provided, validate actions are dropped (pure proxy mode).

The router does NOT validate modifiers. It routes them, emits them for external validation, and forwards to requesters. Validation is the pipeline's job.

## Outbound Manager

The outbound manager runs as a background task. Two distinct phases:

### Floor phase (current behaviour, preserved)
- While `connected_outbound < min_peers`, dial seeds aggressively
  with the existing retry/backoff behaviour. The PeerDb is not
  consulted in this phase — seeds are the bootstrap source.

### Fill phase (new in v0.6.0+)
- Once `connected_outbound >= min_peers` and `< max_peers`, the
  manager enters a slow-trickle mode:
  - Every `outbound_fill_interval` (default **30s**), it queries
    `PeerDb::recent(N, exclude=currently_connected_addrs)` where
    `N = max_peers - connected_outbound`.
  - If the result is non-empty, it dials the **first** entry
    (most-recently-seen). One dial per tick, not N.
  - If `connected_outbound >= max_peers`, the manager sleeps the tick.
  - If the PeerDb is empty or fully exhausted (every fresh candidate
    has been tried and failed within the last `fill_retry_cooldown`),
    the manager remains idle until new peers arrive (via gossip).
- The fill phase respects the blacklist and the address-already-connected
  check. It never re-dials a peer disconnected in the last
  `outbound_redial_cooldown` (default 60s).

### Failure handling
- A dial failure (TCP refused, handshake timeout, version mismatch)
  marks the address with a transient cooldown but does not remove it
  from PeerDb. A peer that repeatedly fails may still gossip back
  through other peers; the PeerDb has no concept of "gave up" — that
  belongs to the cooldown set inside the outbound manager.
- A `PermanentPenalty` from the blacklist removes the entry from
  PeerDb via `forget`.

## Invariants

- Background tasks live until the tokio runtime shuts down.
- `send_to` and `broadcast_outbound` never block on delivery — they queue and return.
- The event subscriber is a read-only tap. It does not affect routing behavior.
- Messages sent via `send_to` bypass the router — they go directly to the peer's write channel. The router does not see them and does not track them.
- The event loop never blocks on validation — `Action::Validate` dispatch is non-blocking.
