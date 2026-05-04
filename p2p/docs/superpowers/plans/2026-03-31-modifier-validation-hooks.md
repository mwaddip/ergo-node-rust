# Modifier Validation Hooks — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `ModifierValidator` trait to the router so the integrator can plug in modifier validation (e.g., header PoW verification) without the P2P layer knowing what validation means.

**Architecture:** New `routing/validator.rs` defines `ModifierVerdict` enum and `ModifierValidator` trait. The `Router` holds an optional `Box<dyn ModifierValidator>`. In `route_message`, the `ModifierResponse` arm calls the validator (if present) for each modifier before touching the latency tracker or request tracker. Rejected modifiers are silently dropped — as if they never arrived.

**Tech Stack:** Rust, no new dependencies.

---

### Task 1: Define ModifierVerdict and ModifierValidator

**Files:**
- Create: `src/routing/validator.rs`
- Modify: `src/routing/mod.rs:1-4`

- [ ] **Step 1: Create `src/routing/validator.rs`**

```rust
//! Modifier validation hook for the router.
//!
//! The router calls the validator (if set) for each modifier in a
//! ModifierResponse before forwarding. The validator decides: accept or reject.
//! The router does not interpret the verdict beyond forward/drop.

/// Result of validating a single modifier from a ModifierResponse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierVerdict {
    /// Forward this modifier to the requester.
    Accept,
    /// Drop this modifier silently. Do not forward.
    Reject,
}

/// Hook for external validation of modifiers before forwarding.
///
/// The router calls `validate` for each modifier in a ModifierResponse.
/// Implementations can parse, verify, track, or do whatever they need.
/// The router only cares about the verdict.
///
/// # Contract
/// - Must never panic. Return `Reject` on any internal error.
/// - Called with the raw modifier bytes — parsing is the implementor's job.
/// - A `Reject` verdict means the modifier is dropped before the request
///   tracker or latency tracker see it (as if it never arrived).
pub trait ModifierValidator: Send {
    /// Validate a single modifier.
    ///
    /// - `modifier_type`: the type byte (1=Header, 2=Tx, 3=BlockTransactions, etc.)
    /// - `id`: the 32-byte modifier ID
    /// - `data`: the raw modifier bytes
    fn validate(&mut self, modifier_type: u8, id: &[u8; 32], data: &[u8]) -> ModifierVerdict;
}
```

- [ ] **Step 2: Add the module to `src/routing/mod.rs`**

Add `pub mod validator;` to the module declarations:

```rust
pub mod inv_table;
pub mod tracker;
pub mod router;
pub mod latency;
pub mod validator;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: success, no errors.

- [ ] **Step 4: Commit**

```bash
git add src/routing/validator.rs src/routing/mod.rs
git commit -m "feat(routing): add ModifierVerdict and ModifierValidator trait"
```

---

### Task 2: Wire validator into Router struct

**Files:**
- Modify: `src/routing/router.rs:1-3` (imports)
- Modify: `src/routing/router.rs:33-49` (struct + constructor)

- [ ] **Step 1: Add import to `src/routing/router.rs`**

Add to the imports at the top of the file:

```rust
use crate::routing::validator::{ModifierValidator, ModifierVerdict};
```

- [ ] **Step 2: Add validator field to Router struct**

Add the field to the `Router` struct (after `latency_tracker`):

```rust
pub struct Router {
    peers: HashMap<PeerId, PeerEntry>,
    inv_table: InvTable,
    request_tracker: RequestTracker,
    sync_tracker: SyncTracker,
    latency_tracker: LatencyTracker,
    validator: Option<Box<dyn ModifierValidator>>,
}
```

- [ ] **Step 3: Initialize field in `Router::new()`**

Add `validator: None` to the constructor:

```rust
impl Router {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
            inv_table: InvTable::new(),
            request_tracker: RequestTracker::new(),
            sync_tracker: SyncTracker::new(),
            latency_tracker: LatencyTracker::new(),
            validator: None,
        }
    }
```

- [ ] **Step 4: Add `set_validator` method**

Add after `new()`, before `register_peer`:

```rust
    /// Set a modifier validator. The router will call it for each modifier
    /// in a ModifierResponse before forwarding. Replaces any previously set validator.
    pub fn set_validator(&mut self, validator: Box<dyn ModifierValidator>) {
        self.validator = Some(validator);
    }
```

- [ ] **Step 5: Verify all existing tests still pass**

Run: `cargo test`
Expected: all tests pass. No behavior change — validator is None, nothing calls it yet.

- [ ] **Step 6: Commit**

```bash
git add src/routing/router.rs
git commit -m "feat(routing): add optional validator field to Router"
```

---

### Task 3: TDD — Implement validation gate

**Files:**
- Modify: `tests/routing_test.rs` (add test)
- Modify: `src/routing/router.rs:128-143` (ModifierResponse arm)

- [ ] **Step 1: Write failing test — validator rejects header modifiers**

Add to `tests/routing_test.rs`:

```rust
use ergo_proxy_node::routing::validator::{ModifierValidator, ModifierVerdict};

struct RejectHeaders;

impl ModifierValidator for RejectHeaders {
    fn validate(&mut self, modifier_type: u8, _id: &[u8; 32], _data: &[u8]) -> ModifierVerdict {
        if modifier_type == 1 {
            ModifierVerdict::Reject
        } else {
            ModifierVerdict::Accept
        }
    }
}

#[test]
fn router_validator_rejects_header_modifiers() {
    let mut router = Router::new();
    router.set_validator(Box::new(RejectHeaders));
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    // Inv + Request setup for a header (type 1)
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    // Response arrives — validator should reject it
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 1,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3])],
        },
    });

    assert!(actions.is_empty(), "rejected header should produce no actions");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test router_validator_rejects_header_modifiers -- --nocapture`
Expected: FAIL — the gate is not implemented yet, so the modifier is forwarded despite the validator.

- [ ] **Step 3: Implement validation gate in `route_message`**

Replace the `ModifierResponse` arm in `route_message` (lines 128–143 of `src/routing/router.rs`):

```rust
            ProtocolMessage::ModifierResponse { modifier_type, modifiers } => {
                let mut actions = Vec::new();
                for (id, data) in &modifiers {
                    if let Some(ref mut validator) = self.validator {
                        if validator.validate(modifier_type, id, data) == ModifierVerdict::Reject {
                            continue;
                        }
                    }
                    self.latency_tracker.record_response(id);
                    if let Some(requester) = self.request_tracker.fulfill(id) {
                        actions.push(Action::Send {
                            target: requester,
                            message: ProtocolMessage::ModifierResponse {
                                modifier_type,
                                modifiers: vec![(*id, data.clone())],
                            },
                        });
                    }
                }
                actions
            }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test router_validator_rejects_header_modifiers -- --nocapture`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `cargo test`
Expected: all tests pass, including all existing routing tests.

- [ ] **Step 6: Commit**

```bash
git add src/routing/router.rs tests/routing_test.rs
git commit -m "feat(routing): validate modifiers before forwarding"
```

---

### Task 4: TDD — Accept-all validator matches no-validator behavior

**Files:**
- Modify: `tests/routing_test.rs`

- [ ] **Step 1: Write test — accept-all validator produces same result as no validator**

Add to `tests/routing_test.rs`:

```rust
struct AcceptAll;

impl ModifierValidator for AcceptAll {
    fn validate(&mut self, _: u8, _: &[u8; 32], _: &[u8]) -> ModifierVerdict {
        ModifierVerdict::Accept
    }
}

#[test]
fn router_validator_accept_all_matches_no_validator() {
    let mut router = Router::new();
    router.set_validator(Box::new(AcceptAll));
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 2, ids: vec![[0xaa; 32]] },
    });

    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 2,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3])],
        },
    });

    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
    }).collect();
    assert_eq!(targets, vec![PeerId(2)]);
}
```

- [ ] **Step 2: Run test**

Run: `cargo test router_validator_accept_all_matches_no_validator -- --nocapture`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add tests/routing_test.rs
git commit -m "test(routing): accept-all validator matches no-validator behavior"
```

---

### Task 5: TDD — Partial rejection in multi-modifier response

**Files:**
- Modify: `tests/routing_test.rs`

- [ ] **Step 1: Write test — one rejected modifier doesn't affect others**

Add to `tests/routing_test.rs`:

```rust
struct RejectById {
    rejected: [u8; 32],
}

impl ModifierValidator for RejectById {
    fn validate(&mut self, _: u8, id: &[u8; 32], _: &[u8]) -> ModifierVerdict {
        if id == &self.rejected {
            ModifierVerdict::Reject
        } else {
            ModifierVerdict::Accept
        }
    }
}

#[test]
fn router_validator_partial_rejection() {
    let mut router = Router::new();
    router.set_validator(Box::new(RejectById { rejected: [0xbb; 32] }));
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    // Announce and request three modifiers
    let ids = vec![[0xaa; 32], [0xbb; 32], [0xcc; 32]];
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 1, ids: ids.clone() },
    });
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: ids.clone() },
    });

    // Response with all three — 0xbb should be rejected
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 1,
            modifiers: vec![
                ([0xaa; 32], vec![1]),
                ([0xbb; 32], vec![2]),
                ([0xcc; 32], vec![3]),
            ],
        },
    });

    // Two actions: 0xaa and 0xcc forwarded to peer 2
    assert_eq!(actions.len(), 2, "rejected modifier should be dropped, others forwarded");

    // Verify the forwarded modifier IDs
    let forwarded_ids: Vec<[u8; 32]> = actions.iter().filter_map(|a| match a {
        Action::Send { message: ProtocolMessage::ModifierResponse { modifiers, .. }, .. } => {
            Some(modifiers[0].0)
        }
        _ => None,
    }).collect();
    assert!(forwarded_ids.contains(&[0xaa; 32]));
    assert!(!forwarded_ids.contains(&[0xbb; 32]));
    assert!(forwarded_ids.contains(&[0xcc; 32]));
}
```

- [ ] **Step 2: Run test**

Run: `cargo test router_validator_partial_rejection -- --nocapture`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add tests/routing_test.rs
git commit -m "test(routing): partial rejection in multi-modifier response"
```

---

### Task 6: TDD — Rejected modifiers skip trackers

**Files:**
- Modify: `tests/routing_test.rs`

This test verifies that a rejected modifier does NOT update the latency tracker or fulfill the request tracker. Strategy: reject a modifier, then send it again with an accept-all validator. If the trackers were incorrectly updated during rejection, the second send would find no pending request to fulfill.

- [ ] **Step 1: Write test — rejected modifiers don't touch trackers**

Add to `tests/routing_test.rs`:

```rust
struct RejectAll;

impl ModifierValidator for RejectAll {
    fn validate(&mut self, _: u8, _: &[u8; 32], _: &[u8]) -> ModifierVerdict {
        ModifierVerdict::Reject
    }
}

#[test]
fn router_rejected_modifier_skips_trackers() {
    let mut router = Router::new();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    // Setup: inv + request
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    // Phase 1: reject the response
    router.set_validator(Box::new(RejectAll));
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 1,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3])],
        },
    });
    assert!(actions.is_empty(), "rejected response should produce no actions");

    // Latency tracker should have no stats — rejected response was not recorded
    assert!(router.latency_stats().is_none(), "rejected response should not update latency tracker");

    // Phase 2: accept the same modifier (proves request tracker wasn't fulfilled)
    router.set_validator(Box::new(AcceptAll));
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 1,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3])],
        },
    });

    // The request tracker should still have the pending entry — now fulfilled
    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
    }).collect();
    assert_eq!(targets, vec![PeerId(2)], "request tracker should still have pending entry after rejection");

    // Now latency tracker should have stats from the accepted response
    assert!(router.latency_stats().is_some(), "accepted response should update latency tracker");
}
```

- [ ] **Step 2: Run test**

Run: `cargo test router_rejected_modifier_skips_trackers -- --nocapture`
Expected: PASS

- [ ] **Step 3: Run full test suite**

Run: `cargo test`
Expected: all tests pass — both new validator tests and all existing routing/transport/protocol tests.

- [ ] **Step 4: Commit**

```bash
git add tests/routing_test.rs
git commit -m "test(routing): rejected modifiers skip latency and request trackers"
```
