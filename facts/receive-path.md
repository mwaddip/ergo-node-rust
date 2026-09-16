# Receive-Path Binding Contract

Cross-component invariant for every byte that arrives from a peer. Each
component contract enforces its own row of the table below; this file states
the rule once and maps every sink to its owner.

## Invariant

**No peer-delivered bytes are stored, served, or acted on under a label the
node has not recomputed from those bytes.**

A label is whatever the node later uses to find, trust, or forward the bytes:
a modifier id, a manifest or subtree id, a transaction id, a peer address's
"seen" timestamp. The peer supplies a label with every delivery; the node
treats it as a claim. The bytes are accepted only when the node's own
computation over the bytes reproduces the label, and only when the delivery
was solicited wherever the protocol has a request to check it against.

Three gates, applied in this order, each independent of the others:

1. **Binding** — recompute the label from the bytes; mismatch → drop.
2. **Request gate** — the node asked for this label, or, where the protocol
   has no request to check against, the label refers to something the node
   already holds; otherwise → drop.
3. **Peer gate** — where the JVM requires the answer to come from the peer
   it asked (the snapshot manifest), so do we; elsewhere the JVM accepts a
   requested modifier from any peer, and so do we.

A drop is a drop: the bytes are not stored, not forwarded, not reported as
received to any tracker, and never overwrite what an honest delivery stored.
Whether a drop also penalizes the sender is a routing decision
(`facts/p2p-routing.md`), not part of this invariant.

## Sinks

| Bytes | Label | Binding computation | Request gate | Enforced by |
|---|---|---|---|---|
| Header (101) | modifier id | id recomputed by `parse_header` from the header's own fields; the wire bytes must equal the re-serialization (else the stored/served bytes would not be the bytes the label was computed over); PoW verified | none — headers self-authenticate | pipeline; `facts/chain.md` `parse_header` / `verify_pow` |
| Block section (102 / 104 / 108) | modifier id | `enr_chain::section_id_from_body` | the body's `header_id` is a header the node holds (best chain or store) **and** the delivered id is one of `section_ids(header)` — the header's roots, not the body's own hash, decide whether the block has that section | pipeline (`facts/sync.md` § Receive-path binding); computation in `facts/chain.md` § Section bodies; precondition in `facts/store.md` |
| Unconfirmed transaction (2) | tx id | `Transaction::id()` over the parsed bytes | none today — the JVM drops unrequested transactions; we validate every tx on entry against state under one shared rate-limit budget (`facts/mempool.md`) | mempool |
| Snapshot manifest (79) | manifest id | root node label recomputed from the node's fields | id chosen by quorum; response accepted only from the peer asked | sync, `facts/snapshot.md` § Manifest download |
| Snapshot chunk (81) | subtree id | root node label recomputed from the node's fields | id is in flight | sync, `facts/snapshot.md` § Chunk download |
| Assembled snapshot | state root | root label and tree height compared with the header's `state_root` at the snapshot height | — | main crate, `facts/snapshot.md` § State initialization |
| NiPoPoW proof (91) | — | full proof verification | only from polled peers; one response counted per peer | sync, `facts/sync.md` `run_light_bootstrap` |
| Peers gossip | "seen" timestamp | none possible — gossip is hearsay and is recorded as hearsay, never as observation | — | `facts/p2p-peerdb.md` § Observed and hearsay |
| `POST /ingest/modifiers` | modifier id | same channel as P2P deliveries: same binding, same request gate | same | `facts/api.md`, `facts/fastsync.md` |

## Delivery reporting

The pipeline reports `DeliveryData::Received` only for labels it accepted:
headers that parsed and passed PoW, sections that bound and were stored. A
dropped delivery is invisible to the tracker, so its pending request keeps
its timeout and is re-requested from another peer (`facts/sync.md` § Trigger
points). The tracker itself stays peer-agnostic, matching the JVM's
`processSpam`, which filters on request status and not on sender.

## JVM reference

- Section binding: `FullBlockSectionProcessor.scala` rule
  `bsCorrespondsToHeader` (`header.sectionIds.exists(_._2 == m.id)`), with
  `m.id` computed from the parsed body via `NonHeaderBlockSection.computeId`.
- Receipt gates: `ErgoNodeViewSynchronizer.processSpam` (non-requested →
  `SpamPenalty`) and `parseModifiers` (`id == mod.id`, else
  `MisbehaviorPenalty`).
- Manifest: `ErgoNodeViewSynchronizer.processManifest` — computed
  `manifest.id`, must have been requested, must come from the requested peer.
- Peer propagation: `PeerManager.SeenPeers` serves only peers with a completed
  handshake; gossip enters via `AddPeerIfEmpty` with `lastHandshake = 0`.
