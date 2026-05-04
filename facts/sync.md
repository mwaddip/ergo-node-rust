# Sync State Machine Contract

## Component: `ergo-sync` (workspace crate)

Owns the chain synchronization protocol. Drives the P2P layer to request data,
feeds validated results to the chain/state components, and tracks sync progress.

Primary dependencies: P2P layer (send/receive messages), chain (header state and validation),
pipeline (progress notifications).

## Traits (dependency inversion)

The sync crate does NOT depend on concrete P2P or chain types. It defines traits
that the main crate satisfies with the real implementations.

### `SyncTransport`

How the sync machine sends messages and observes the network.

#### `send_to(peer, message) -> Result<()>`
- Send a protocol message to a specific peer.

#### `outbound_peers() -> Vec<PeerId>`
- Currently connected outbound peers.

#### `next_event() -> Option<ProtocolEvent>`
- Receive the next incoming protocol event. Returns None if the stream ends.

### `SyncStore`

How the sync machine queries persistent storage.

#### `has_modifier(type_id, id) -> bool`
- Returns true if the modifier exists in the store.
- Used to determine which block sections need downloading.
- Must not block the async runtime (the bridge impl handles this).

#### `get_modifier(type_id, id) -> Option<Vec<u8>>`
- Retrieve raw modifier bytes from the store.
- Returns None if not found. Used during validation sweeps to load
  block sections (transactions, AD proofs, extensions) by type and ID.

#### `script_verified_height() -> Option<u32>`
- Read the persisted script_verified_height. Returns None if not set.
- Used on startup to detect the gap between script-verified and
  state-applied heights after an unclean shutdown.

#### `set_script_verified_height(height)`
- Persist the script_verified_height. Called every 100 blocks during
  the sweep's drain of deferred eval results.

### `SyncChain`

How the sync machine queries and updates chain state.

#### `chain_height() -> u32`
- Height of the validated chain tip.

#### `header_at(height: u32) -> Option<Header>`
- Return the header at a given height, if it exists in the chain.
- Used by the sync machine to compute block section IDs for download.

#### `build_sync_info() -> Vec<u8>`
- Build a V2 SyncInfo body from current chain state.
- Includes headers at offsets `[0, 16, 128, 512]` from tip, ordered tip-first.

#### `parse_sync_info(body: &[u8]) -> Result<SyncInfo>`
- Parse an incoming SyncInfo message.

#### `sync_info_heights(info: &SyncInfo) -> Vec<u32>`
- Extract heights from a parsed SyncInfo (V2 only).

## HeaderSync

### `HeaderSync::new(config, transport, chain, store, validator, progress, delivery_control, delivery_data, snapshot_tx, validator_rx, shared_downloaded_height, block_request_gate, peer_chain_tip) -> Self`
- Create the sync state machine with injected dependencies.
- `config`: `SyncConfig` with timing parameters, delivery settings, and `state_type`.
  The `state_type` field (`StateType::Utxo`, `StateType::Digest`, or
  `StateType::Light`) determines which block sections are queued for download
  via `required_section_ids`. `Light` returns an empty section list, so the
  download phase is a no-op without special-casing in the sync loop.
- `store`: `SyncStore` for checking modifier existence
- `validator`: `Option<BlockValidator>` for digest/UTXO-mode block validation.
  **`None` in `StateType::Light`** — the main crate's startup wiring branches
  on `state_type` and constructs no validator for light mode. The watermark
  scanner (`advance_state_applied_height`) is bypassed entirely when `validator`
  is `None`; `state_applied_height` is set once at install time to the proof's
  suffix tip and never advances from block validation thereafter.
- `progress`: `mpsc::Receiver<u32>` from the validation pipeline. Carries chain
  height after each validated batch. Used for stall detection and the two-batch
  SyncInfo pattern. In light mode, this channel is constructed but never
  produces values (no validator → no progress events) and the sync machine
  treats progress as "infrequent" rather than "broken."
- `delivery_control`: `mpsc::UnboundedReceiver<DeliveryControl>` from the pipeline.
  Carries `Reorg` and `NeedModifier` events. These are rare and critical — losing
  one is unrecoverable. Unbounded channel, never dropped.
- `delivery_data`: `mpsc::Receiver<DeliveryData>` from the pipeline. Carries
  `Received` and `Evicted` modifier notifications. High-volume, lossy — missing
  one just delays the watermark scan by one timer tick. Bounded channel (capacity 64),
  sent via `try_send`. In light mode this channel sees no traffic.
- `snapshot_tx`: `Option<oneshot::Sender<SnapshotData>>` for UTXO snapshot bootstrap.
  When present, the sync machine sends downloaded snapshot data to the main crate
  for state loading. None when not in bootstrap mode.
- `validator_rx`: `Option<oneshot::Receiver<V>>` to receive the validator back after
  snapshot loading completes.
- `shared_downloaded_height`: `Arc<AtomicU32>` published after each
  `advance_downloaded_height`. Read by the API (`/info` → `downloadedHeight`) and
  fastsync to determine which block sections are already in the store.
- `block_request_gate`: `Arc<AtomicBool>` controlling whether `request_announced`
  and the `handle_delivery_check` retry path actually send `ModifierRequest`.
  When `false` (closed), all block/header/tx modifier requests are skipped —
  the sync machine still processes incoming events (Inv, SyncInfo, modifier
  responses) but does not emit any outgoing requests. The main crate owns
  this flag: closed at construction, opened after the boot-time bootstrap
  decision resolves (see "Bootstrap Mode" section below). Once opened, stays
  open for the lifetime of the process.
- `peer_chain_tip`: `Arc<AtomicU32>` published on every incoming `SyncInfo`
  from any peer. Stores the maximum height observed in any peer's tip-first
  heights vector. Read by the main crate to compute the bootstrap gap
  (`peer_chain_tip - downloaded_height`). Initialized to 0; remains 0 until
  at least one `SyncInfo` is received.

### `HeaderSync::run() -> !`
- Long-running async task. Drives the sync loop until the runtime shuts down.

## Architecture: Event-driven loop

Four event sources via `tokio::select!` with `biased;` (control checked first):

1. **Control-plane events** — `DeliveryControl::Reorg` and `DeliveryControl::NeedModifier`
   from the pipeline. Checked with priority via `biased;` — these are never dropped.
2. **P2P events** — Inv (header IDs from peer), SyncInfo (peer's chain state), peer disconnect
3. **Data-plane events** — `DeliveryData::Received` and `DeliveryData::Evicted` from the
   pipeline. Lossy — the delivery timer provides fallback scanning.
4. **Pipeline progress** — `mpsc::Receiver<u32>` carrying chain height after each validated batch
5. **Sync timer** — fires every 20 seconds (matching JVM's `MinSyncInterval`)

## Sync cycle

```
pick_sync_peer() → sync_from_peer() → synced()
       ↑                  │
       └──── stall/disconnect ────┘
```

### sync_from_peer()

1. Send SyncInfo to peer (kick off exchange)
2. Event loop:
   - P2P Inv with header IDs → send ModifierRequest to announcing peer
   - Pipeline progress → one SyncInfo per 20s cycle (two-batch pattern)
   - 20s timer → send SyncInfo (scheduled cycle start)
   - Stall timeout (60s no progress) → rotate to different peer

### Peer rotation

`stalled_peers: HashSet<PeerId>` tracks peers that failed to produce progress.
On stall: add current peer, pick next outbound peer not in set.
On progress: clear the set (all peers eligible again).
If all peers stalled: clear set, retry.

### Multi-peer switching

When any peer's SyncInfo shows a chain tip more than 1 block ahead of ours,
the sync machine switches to syncing from that peer (`BehindPeer` event).
The "caught up" check only triggers from the current sync peer — other peers
reporting lower tips don't cause a false synced state.

### Deep reorg support

The pipeline handles fork detection and chain reorganization. When a fork header
arrives that creates a better chain (higher cumulative difficulty), the pipeline:

1. Stores the fork header with its score via the store's fork-aware tables.
2. Assembles the fork branch by walking parent links backward through the store.
3. Executes `HeaderChain::try_reorg_deep()` to atomically swap the best chain.
4. Sends `DeliveryControl::Reorg { fork_point, old_tip, new_tip }` to the sync machine.

The sync machine responds by draining in-flight eval results, clearing its section
queue, resetting all three watermarks (`downloaded_height`, `state_applied_height`,
and `script_verified_height`) to the fork point, resetting the block validator's
state root, re-queuing sections for the new branch, and re-scanning the download
watermark.

For incomplete fork chains (parent not in store), the pipeline sends
`DeliveryControl::NeedModifier` to request the missing parent header. Once it
arrives, the fork chain links backward and triggers the reorg if the score is
sufficient. See `facts/reorg.md` for the full contract.

### Two-batch pattern

The JVM gets ~800 headers per 20-second cycle by sending SyncInfo twice: once
from the scheduled timer, once after the first batch is processed. The pipeline
progress channel enables this — when a batch finishes, one progress-triggered
SyncInfo is allowed per cycle.

### Synced state

Periodic SyncInfo (30s) to detect new blocks. Reacts to Inv with ModifierRequest.
Receives pipeline progress for logging. The control channel is checked with
`biased;` priority in the synced loop too — a `Reorg` while synced must not be missed.

### At-tip Storage Reopen (v0.4.x)

Operators can reduce steady-state RSS at chain tip by reopening the
AVL state DB with a smaller redb cache once sync has caught up.
HeaderSync hosts the sync side of a one-shot handshake; the main
crate owns the storage lifecycle.

**Channels (set via `HeaderSync::set_at_tip_channels`, post-construction):**

- `at_tip_request_tx: oneshot::Sender<u32>` — sync sends the
  flushed validator's height when ready to swap.
- `at_tip_validator_rx: oneshot::Receiver<V>` — sync receives the
  rebuilt validator (built against the new storage handle) and
  resumes.

If both channels are unset, the at-tip path is a no-op and sync
runs the cold-sync settings indefinitely.

**`SyncConfig` overrides** (all `Option<...>`; `None` = keep
cold-sync value):

- `synced_flush_heap_threshold_mb: Option<u64>`
- `synced_flush_max_blocks: Option<u32>`
- `synced_flush_min_blocks: Option<u32>` — recommended ≥ 5; with
  per-block flushing (= 1) a remove-then-reinsert AVL pattern across
  two adjacent blocks can slip past the prover's dirty-node tracking
  and orphan a node on disk. The wider window lets the pattern
  cancel within prover state before any flush.

**Gate** (`AT_TIP_WINDOW = 16`): the handshake fires when
`validator.validated_height() + AT_TIP_WINDOW >= chain.chain_height()`.
Re-evaluated on each `section_ticker` tick (every 2s) so the
transition fires the moment the validator catches up — without
bouncing through `sync_from_peer` to re-enter `synced()`.
Idempotent via `Option::take`: once the handshake fires, the
channels are consumed and re-entries are no-ops.

**Sequence:**

1. `validator.flush()` to persist any in-memory write-tx state.
   Failure reinstates the old validator and skips the rebuild.
2. `drop(validator)` — releases the AVL storage `Arc<Database>`.
   All other holders (mempool, REST API, mining) must release
   their `Arc<SnapshotReader>`s in parallel for redb's exclusive
   file lock to free.
3. `at_tip_request_tx.send(height)` → main reopens with
   `synced_cache_mb`, builds new validator at `height`.
4. `at_tip_validator_rx.await` → receive new validator.
5. `debug_assert_eq!(new.validated_height(), height)`.

**At-tip flush settings** (the cheap part) are swapped at the same
time as the storage reopen; same gate, same idempotence.

## JVM peer behavior (observed)

Critical findings from debugging sync against JVM 6.0.3 peers:

- **MinSyncInterval**: 20 seconds per peer. Sending faster is silently dropped.
- **PerPeerSyncLockTime**: 100ms. Incoming SyncInfo within 100ms of previous is dropped as "spammy."
- **~12 batches per connection**: after ~12 SyncInfo exchanges, the JVM stops processing our SyncInfo on that connection. The message is received (visible in JVM debug logs) but not forwarded to `processSync`. Reconnection starts a new session.
- **Delivery tracker essential**: the JVM uses a 10-second delivery tracker to retry failed requests. Without it, lost responses are never recovered. Our sync relies on the 60-second stall timeout — 6x slower recovery.
- **Single peer per cycle**: the JVM syncs from one Older peer per 20-second cycle, not all peers simultaneously.
- **SyncInfo response**: the JVM responds to incoming SyncInfo with its own SyncInfo when `syncSendNeeded` (status changed, peer outdated, status=Older/Fork). We don't respond during active sync to avoid sending stale chain state.

## Light-Client Bootstrap (StateType::Light only)

When `config.state_type == StateType::Light` AND the chain is empty at startup,
the sync machine runs a one-shot NiPoPoW bootstrap BEFORE entering the normal
sync cycle. The bootstrap installs the proof's suffix as the chain origin via
`HeaderChain::install_from_nipopow_proof`; subsequent tip-following uses the
existing header sync loop without modification.

### `run_light_bootstrap(transport, chain, config) -> Result<(), LightBootstrapError>`

Top-level entry point. Called by `HeaderSync::run` when `state_type == Light`
AND `chain.is_empty()`. Idempotent: if the chain is already non-empty (e.g.,
restart after a successful prior bootstrap), this is a no-op and the function
returns immediately.

State machine:

1. **Wait for at least one outbound peer** with a delivery-eligible status
   (handshake complete, not banned). Poll `transport.outbound_peers()` every
   1s up to a 60s deadline. No peers → `LightBootstrapError::NoPeers`.

2. **Send `GetNipopowProof`** to the first eligible peer with `m=6`, `k=10`,
   `header_id = None` (no anchor — request a proof at the peer's current tip).
   The wire envelope is built via `src/nipopow_serve::serialize_get_nipopow_proof`
   (a new function — currently only the response serializer exists).

3. **Wait for `NipopowProof` response** (P2P code 91) from that peer with a
   30-second timeout. Other messages from other peers during this window are
   processed normally by the rest of the sync machine; only `code == 91`
   from the requested peer counts as the response. Timeout or wrong-peer
   response → mark peer stalled, rotate to next eligible peer, retry up to
   3 peers total. All 3 stalled → `LightBootstrapError::AllPeersStalled`.

4. **Verify** the inner proof bytes via
   `enr_chain::verify_nipopow_proof_bytes`. Verification failure → mark
   peer hostile (NOT just stalled — sending an invalid proof is a protocol
   violation), rotate, retry. Three hostile peers in a row →
   `LightBootstrapError::AllPeersHostile`.

5. **Install** the verified suffix into the local `HeaderChain`:
   - The result's `headers: Vec<Header>` slice contains, in order:
     `prefix`, then `suffix_head.header`, then `suffix_tail`. The light
     client only installs the suffix portion (`suffix_head` + `suffix_tail`),
     NOT the prefix headers — the prefix exists to prove cumulative work
     and is discarded after verification.
   - The split point inside `headers` is `headers.len() - k` (the last `k`
     entries are the suffix; the rest is the prefix). With `k=10`, the
     install passes `headers[headers.len()-10]` as `suffix_head` and the
     remaining 9 as `suffix_tail`.
   - On success, `chain.height()` returns the suffix tip's height. Set
     the `validated_height` watermark to the same value (light mode treats
     all installed headers as "validated" — the proof's PoW checks are
     the validation).

6. **Transition to normal tip-following sync** via the existing
   `sync_from_peer` loop. From here on out, light mode behaves like full
   mode minus block bodies: the sync machine sends SyncInfo, receives
   header Inv, requests headers, validates them via `try_append`, and
   advances the tip.

### `LightBootstrapError`

```rust
pub enum LightBootstrapError {
    NoPeers,
    AllPeersStalled,
    AllPeersHostile,
    InstallFailed(ChainError),
    StreamClosed,
}
```

All variants are fatal to bootstrap — the sync machine logs and exits.
The main crate decides whether to retry from scratch or terminate the node;
for first release, terminate.

### Bootstrap invariants

- **Single peer per attempt**: bootstrap requests from ONE peer at a time.
  Multi-peer best-arg comparison (KMZ17 §4.3, where the client compares
  proofs from multiple peers and picks the one with highest cumulative work
  via `bestArg`) is **out of scope for first release** and tracked as a
  hardening follow-up. The first-release trust model is "trust the first
  peer that returns a verifiable proof." This is documented as a known
  limitation in the user-facing release notes.
- **No restart-resume state**: bootstrap is one-shot and re-runs from
  scratch on every restart where `chain.is_empty()`. Once the chain is
  installed, subsequent restarts skip bootstrap entirely (chain is loaded
  from store and is non-empty). There is no partial-bootstrap state that
  needs persistence — the operation is atomic.
- **Bootstrap NEVER mutates `store/`** beyond what `HeaderChain` itself
  writes via its existing persistence path. The proof bytes are not
  archived after install. If we want to re-verify the proof after a
  reboot, we'd need to re-fetch it; this is not a first-release concern.

### Trust model

Standard SPV: single-peer bootstrap trusts that peer's view of the
chain. Failure mode is liveness, not safety — a hostile peer causes a
recoverable DoS, not loss of funds. Multi-peer best-arg comparison
(KMZ17 §4.3) is the standard hardening, tracked as a follow-up.

## Block Section Download

After header sync reaches the peer's tip, the sync machine downloads block sections
for stored headers. Which sections are downloaded depends on `SyncConfig.state_type`:
- **UTXO mode**: BlockTransactions (102) + Extension (108). No AD proofs.
- **Digest mode**: all three including ADProofs (104).
- **Light mode**: NONE. `required_section_ids` returns an empty Vec, the section
  queue stays empty, and the watermark scanner has nothing to advance.

This mirrors the JVM's `ToDownloadProcessor.requiredModifiersForHeader`, which calls
`Header.sectionIdsWithNoProof` in UTXO mode and `Header.sectionIds` in digest mode.
JVM has no light-mode analog at the section-id level (it gates the entire
download phase via `nipopowBootstrap`); our chain crate folds the gating into
`required_section_ids` returning empty, which keeps the sync loop unchanged.

### Download queue

The sync machine maintains an internal queue of `(type_id, modifier_id)` pairs
for block sections that need downloading. The queue is populated from two sources:

1. **On header progress**: when the pipeline reports new chained headers, compute
   section IDs for each new header (via `chain::required_section_ids` with the
   configured state type), check the store (`SyncStore::has_modifier`), and
   enqueue any missing sections.

2. **On startup**: walk stored headers from height 1 to tip, compute section IDs,
   check the store, enqueue what's missing. One-time startup cost.

### Download cycle

Block section requests follow the same pattern as header requests:
- Send `ModifierRequest` with the section type and IDs
- Track delivery via the `DeliveryTracker`
- On timeout: re-request from a different peer
- On receive: the pipeline stores the bytes (no validation)

### Inv handling

The JVM may send Inv with non-header type IDs (102, 104, 108). The sync machine
should react to these the same way as header Inv: request the listed modifiers
and track delivery.

### Prioritization

Header sync takes priority. Block section download starts only after header sync
reaches the `Synced` state. During active header sync, block section requests
are paused to avoid saturating the peer's bandwidth.

## Bootstrap Mode (Optional Fastsync)

When a node starts with a large gap between the network's chain tip and the
local `downloaded_height`, the main crate MAY delegate bulk block fetching
to the **fastsync addon** — a separate process that fetches headers and
block sections via REST from multiple peers in parallel, rather than
serialized `ModifierRequest` over P2P.

### No dependency

Fastsync is an **optional external process**. The main `ergo-node` crate has
no Cargo dependency on the fastsync crate. No shared types, no library
imports. Communication is exclusively via:

- Process boundary: `std::process::Command` spawn + wait on exit.
- REST: fastsync is a client of the main node's ingestion API (see `facts/api.md`).

If the fastsync binary is not present on the host, the addon is disabled in
config, or fastsync exits with a non-zero status, the main node MUST
continue correctly via P2P-only sync. A missing or broken fastsync is never
a node-fatal error. The main crate **ignores the exit status** — any exit
(clean or crash) means "move on to P2P mode." Retry semantics are out of
scope: validation catches poisoned data, and P2P fetches whatever fastsync
didn't deliver.

### Boot-time decision

Once during startup, the main crate:

1. Waits for at least one connected outbound peer to deliver a `SyncInfo`
   message, with a bounded timeout (default 30 seconds — same order of
   magnitude as `run_light_bootstrap`'s peer-wait).
2. If the timeout expires with no `SyncInfo` received, skips fastsync and
   proceeds to P2P-only sync. A node with no peers can't bootstrap either
   way; fastsync adds no value.
3. Otherwise computes:

   ```
   gap = peer_reported_chain_tip - downloaded_height
   ```

   `peer_reported_chain_tip` is the highest tip seen in any incoming
   `SyncInfo` received during the wait. `downloaded_height` is read from
   the shared `Arc<AtomicU32>` (initialized from the validator's persisted
   height).
4. If `gap > fastsync_threshold_blocks` AND the fastsync binary is available
   AND `fastsync == true` in config, the main crate spawns fastsync
   as a subprocess and waits for it to exit before opening the P2P
   block-request gate.
5. Otherwise, proceeds directly to P2P sync.

### Threshold rationale

The default threshold of 25,000 blocks corresponds to approximately 35 days
of chain depth at the 2-minute target block time. Data at that depth has
been buried under ~25,000 blocks of PoW and is overwhelmingly likely to be
honest. The trust model is: "trust peer-returned data for deep chain
history; verify downstream via the normal validation pipeline." Any bad
data that slips through is caught by PoW verification, header chain
validation, AD proof verification, and script evaluation — the same
pipeline that validates P2P-delivered data. Fastsync does not bypass
validation; it only accelerates delivery.

### Config

The main node exposes these config keys (location: node config, not
`SyncConfig` — bootstrap orchestration is the main crate's job, not the
sync crate's):

```
fastsync: bool                       # default: true
fastsync_threshold_blocks: u32       # default: 25_000
fastsync_peer_wait_timeout_sec: u32  # default: 30
```

Operators who want to disable fastsync entirely can set `fastsync = false`,
or simply not install the binary. Operators who want to tune the trigger
threshold can adjust `fastsync_threshold_blocks`. The fastsync addon
inherits the threshold from this config (passed via `--handoff-distance`
when spawned by the main node); standalone fastsync invocations default to
the same 25,000 so the two sides agree out of the box.

### P2P behavior during fastsync

While fastsync is running, the main node's P2P layer continues to
participate in the network with one exception:

- **Connections stay up.** Outbound and inbound peers remain connected,
  handshakes complete, keep-alives and peer discovery run normally.
- **SyncInfo exchange continues.** The sync machine MAY send and process
  `SyncInfo` messages to signal progress and observe the current chain tip.
- **`ModifierRequest` is gated off.** The sync machine does not request
  headers or block sections from peers while fastsync is running; fastsync
  is fetching them via REST in parallel. Incoming `Inv` messages do not
  produce outgoing requests during this window.
- **Gate opens on fastsync exit.** When the fastsync subprocess terminates
  (regardless of exit status), the main crate opens the P2P block-request
  gate and the sync state machine proceeds with normal tip-following sync.

### Fastsync interface (from main node's perspective)

Fastsync is a REST client. It writes data into the main node via the
existing ingestion endpoints (see `facts/api.md`). The main node:

- Exposes those endpoints unconditionally (not gated on bootstrap state).
- Receives headers and block sections, stores them via the normal store
  write path, and advances `downloaded_height` via the watermark scanner.
- Runs the validation pipeline on delivered data concurrently, advancing
  `state_applied_height` and `script_verified_height` normally.

The main node does NOT supervise fastsync's peer selection, fetch strategy,
or internal state. It waits for the subprocess to exit and then transitions
to P2P mode. Whether fastsync closed the gap, partially closed it, or
failed, the main node's behavior is identical: open the gate and run
normal sync.

### Exit condition

Fastsync exits when either:
- Its own gap check reports `chain_tip - downloaded_height` has closed to
  within `fastsync_threshold_blocks`, OR
- It encounters a fatal error (no peers, all peers misbehaving, REST
  unreachable, addon crash).

Both cases trigger the same main-crate response:
1. Log the exit status and duration.
2. Open the P2P block-request gate.
3. Continue with normal sync.

### No re-trigger

Fastsync is **boot-only**. Once the main node exits bootstrap mode (fastsync
has completed or was skipped), it never re-spawns fastsync during the
lifetime of the process. If the node falls far behind during normal
operation (network partition, long hibernation, misbehaving peers), it
catches up via P2P only. Restarting the node triggers a fresh boot-time
gap check.

Rationale: once a node is near tip, the P2P 192-block window keeps up with
the mean block interval. The "far behind while running" case is rare enough
that the added complexity of continuous monitoring and mode transitions
isn't justified. Restart is an acceptable recovery mechanism.

## Block Assembly (state_applied_height / script_verified_height)

The sync machine tracks three watermarks:

- **`downloaded_height`** — highest height where all required block sections
  are present in the store.
- **`state_applied_height`** — highest height where `apply_state()` returned Ok.
  External consumers (API, mempool, mining) see this height.
- **`script_verified_height`** — highest height where `evaluate_scripts()` has
  completed successfully. Internal bookkeeping for rollback decisions.
  Advances in-order as eval results arrive via crossbeam channel.

`downloaded_height` and `state_applied_height` are initialized from
`validator.validated_height()` on startup. `script_verified_height` is
persisted separately and loaded on startup.

### Invariants

- `script_verified_height <= state_applied_height <= downloaded_height <= chain_height`
- `state_applied_height` is monotonically increasing (except on reorg or eval failure)
- Heights at or below `script_verified_height` are fully validated (state + scripts)

### Eval dispatch

Script evaluation is dispatched to the rayon thread pool via `rayon::spawn`.
Results are sent through `crossbeam_channel::Sender<(u32, Result<(), ValidationError>)>`.
The sync layer drains the receiver non-blocking between blocks during the
sweep, and blocking after the sweep completes.

No backpressure. Memory per DeferredEval is ~25KB typical, ~410KB worst case.

### At chain tip

When `sweep_size == 1`, drain the eval channel synchronously after applying
state. No pipeline benefit for a single block during live sync.

### Eval failure handling

Eval failures are detected in `drain_eval_results` during the sweep loop.
Two detection points:

**In-loop detection:** After each non-blocking drain, the sweep compares
`state_applied_height` against its pre-drain value. If `handle_eval_failure`
reduced it during the drain, the sweep corrects `validated_to` and breaks
immediately — preventing the post-loop code from overwriting the rolled-back
watermark with the stale `validated_to`. Without this check, the sweep would
continue feeding blocks to a rolled-back validator (height mismatch) and then
clobber `state_applied_height` back to the pre-rollback value.

**Post-sweep detection:** The blocking drain after sweep completion can also
find failures. `handle_eval_failure` sets the correct watermarks directly;
no post-drain code overwrites them.

`handle_eval_failure` sequence:
1. Drain and discard remaining channel results
2. Look up digest via `chain.header_at(failed_height - 1).state_root`
3. Call `validator.reset_to(failed_height - 1, digest)`
4. Reset `state_applied_height` and `script_verified_height` to `failed_height - 1`
5. Reset `downloaded_height` to match
6. Log the error, resume sync

### Startup gap handling

On startup, if persisted `script_verified_height < state_applied_height`,
the gap is accepted — the AVL digest already proved state correctness during
`apply_state`, and proof boxes aren't available without re-running apply_state.
`script_verified_height` is advanced to match `state_applied_height`.

### Watermark scanner

`advance_downloaded_height()` scans forward from the current watermark. For each
height, it computes `required_section_ids(header, state_type)` and checks the
store for each. Advances as far as possible, stops at the first gap. On advance,
calls `advance_state_applied_height()` to run the pipeline on newly downloaded blocks.

### Trigger points

1. **Startup**: after the section queue is built from stored headers.
2. **DeliveryData::Received**: after sections are stored by the pipeline.
3. **Delivery check timer**: every 5 seconds during active sync. This is the
   primary trigger — the data channel can overflow when sections arrive
   faster than the sync machine processes events, so the timer ensures the
   scanner runs regardless.
4. **Synced ticker**: every 30 seconds during the synced polling loop.

## Does NOT own

- Header validation — that's `enr-chain` via the validation pipeline
- Block section validation — that's `ergo-validation` via `BlockValidator` trait
- Persistent storage — that's `store/`
- Network I/O — that's `enr-p2p`
- Section ID computation — that's `enr-chain` (`section_ids()` / `required_section_ids()`)
- Fork choice / reorg execution — that's the pipeline (via `HeaderChain::try_reorg_deep`)
- Bootstrap orchestration — that's the main crate (decides whether to spawn
  fastsync at boot, gates P2P block requests during bootstrap)

## Future Extensions

- UTXO state management coordination
- Parallel header download from multiple peers
- Turbo sync mode (adaptive batch sizes, see IDEAS.md)
