# ModifierRequest Fallback Routing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a ModifierRequest has no inv table entry, forward it to an available outbound peer instead of dropping it.

**Architecture:** Restructure the `ModifierRequest` arm in `route_message` to try the inv table first, then fall back to any outbound peer (excluding the source). Same request tracker and latency recording in both paths. Contract update already applied via facts submodule.

**Tech Stack:** Rust, no new dependencies.

---

### Task 1: TDD — Fallback routing when no inv entry

**Files:**
- Modify: `tests/routing_test.rs`
- Modify: `src/routing/router.rs:116-134`

- [ ] **Step 1: Write failing test — request with no inv entry forwards to outbound peer**

```rust
#[test]
fn router_modifier_request_no_inv_falls_back_to_outbound() {
    let mut router = Router::new();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    // No Inv — peer 2 requests a modifier the router has never seen
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    // Should fall back to outbound peer 1
    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
    }).collect();
    assert_eq!(targets, vec![PeerId(1)]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test router_modifier_request_no_inv_falls_back -- --nocapture`
Expected: FAIL — actions is empty because no inv entry exists.

- [ ] **Step 3: Implement fallback in `route_message`**

Replace the `ModifierRequest` arm (lines 116-134 of `src/routing/router.rs`):

```rust
            ProtocolMessage::ModifierRequest { modifier_type, ids } => {
                let mut actions = Vec::new();
                for id in &ids {
                    let target = if let Some(inv_target) = self.inv_table.lookup(id) {
                        if inv_target == source { continue; }
                        Some(inv_target)
                    } else {
                        self.peers.iter()
                            .find(|(pid, entry)| **pid != source && entry.direction == Direction::Outbound)
                            .map(|(pid, _)| *pid)
                    };

                    if let Some(target) = target {
                        self.request_tracker.record(*id, source);
                        self.latency_tracker.record_request(*id, target);
                        actions.push(Action::Send {
                            target,
                            message: ProtocolMessage::ModifierRequest {
                                modifier_type,
                                ids: vec![*id],
                            },
                        });
                    }
                }
                actions
            }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test router_modifier_request_no_inv_falls_back -- --nocapture`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `cargo test`
Expected: all tests pass including existing `router_modifier_request_routes_via_inv_table`.

- [ ] **Step 6: Commit**

```bash
git add src/routing/router.rs tests/routing_test.rs
git commit -m "feat(routing): fallback to outbound peer when inv table has no entry for ModifierRequest"
```

---

### Task 2: Test — No outbound peers produces no actions

**Files:**
- Modify: `tests/routing_test.rs`

- [ ] **Step 1: Write test**

```rust
#[test]
fn router_modifier_request_no_inv_no_outbound_drops() {
    let mut router = Router::new();
    router.register_peer(PeerId(1), Direction::Inbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    // No outbound peers at all, no inv entry
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    assert!(actions.is_empty(), "no outbound peers means no fallback target");
}
```

- [ ] **Step 2: Run test**

Run: `cargo test router_modifier_request_no_inv_no_outbound -- --nocapture`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add tests/routing_test.rs
git commit -m "test(routing): no outbound peers produces no fallback actions"
```

---

### Task 3: Test — Inv table still preferred when entry exists

**Files:**
- Modify: `tests/routing_test.rs`

- [ ] **Step 1: Write test**

The existing `router_modifier_request_routes_via_inv_table` already covers this, but let's add a test that verifies the inv target is chosen over a different outbound peer — proving fallback doesn't override the inv table.

```rust
#[test]
fn router_modifier_request_with_inv_ignores_fallback() {
    let mut router = Router::new();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(3), Direction::Inbound, ProxyMode::Full);

    // Peer 2 announces, so inv table maps to peer 2
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::Inv { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    // Peer 3 requests — should route to peer 2 (inv hit), not peer 1 (fallback)
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(3),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
    }).collect();
    assert_eq!(targets, vec![PeerId(2)], "inv table target should be used, not fallback");
}
```

- [ ] **Step 2: Run test**

Run: `cargo test router_modifier_request_with_inv_ignores_fallback -- --nocapture`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add tests/routing_test.rs
git commit -m "test(routing): inv table preferred over fallback for ModifierRequest"
```

---

### Task 4: Test — Response to fallback-routed request reaches requester

**Files:**
- Modify: `tests/routing_test.rs`

- [ ] **Step 1: Write test**

```rust
#[test]
fn router_fallback_request_response_reaches_requester() {
    let mut router = Router::new();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    // No inv — fallback routes request to peer 1
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    // Peer 1 responds — should route back to peer 2 (the original requester)
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 1,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3])],
        },
    });

    let targets: Vec<PeerId> = actions.iter().filter_map(|a| match a {
        Action::Send { target, .. } => Some(*target),
    }).collect();
    assert_eq!(targets, vec![PeerId(2)], "response should route back to original requester");
}
```

- [ ] **Step 2: Run test**

Run: `cargo test router_fallback_request_response_reaches_requester -- --nocapture`
Expected: PASS

- [ ] **Step 3: Run full test suite**

Run: `cargo test`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add tests/routing_test.rs
git commit -m "test(routing): fallback-routed request response reaches original requester"
```

---

### Task 5: Commit submodule update

**Files:**
- Modify: `facts` (submodule pointer)

- [ ] **Step 1: Stage and commit the submodule update**

```bash
git add facts
git commit -m "docs: update routing contract with ModifierRequest fallback behavior"
```
