# Handoff notes — voting + nipopow follow-ups

Written 2026-04-07 at the end of session 13 after implementing soft-fork
voting end-to-end and the NiPoPoW serve+verify handler. Two tasks were
left partially verified — this file is the quickest on-ramp for the next
session to finish them.

## Task #9 — Complete voting test on test server

**Status at session end**: fresh cold-start sync on the test server with
the Phase B binary, actively validating past the first 8+ epoch boundaries
(including block 128). Zero `ParameterMismatch` errors. Rate ~28 blocks/sec
peer-bound. ETA to testnet tip ~2.7 hours from the 13:48 UTC deploy.

**How to check**:

```bash
# 1. Service still running?
ssh admin@216.128.144.28 "systemctl is-active ergo-node-rust"

# 2. Where is the validator REALLY?
#    (API fullHeight reports chain.height, not validated_height — known bug)
ssh admin@216.128.144.28 "sudo grep -aE 'validated height advanced' /var/log/ergo-node/ergo-node.log | tail -5"

# 3. Any ParameterMismatch errors post-13:48 (after the Phase B deploy)?
ssh admin@216.128.144.28 "sudo grep -aE 'ParameterMismatch|epoch-boundary parameter' /var/log/ergo-node/ergo-node.log | grep '2026-04-07T1[4-9]'"

# 4. Has validator caught up to the JVM tip?
ssh admin@216.128.144.28 "curl -s http://127.0.0.1:9063/info | python3 -c 'import sys,json; d=json.load(sys.stdin); print(f\"jvm_full={d[\\\"fullHeight\\\"]} jvm_headers={d[\\\"headersHeight\\\"]}\")'"
```

**Completion criteria**:
- No `ParameterMismatch` errors in the log since the 2026-04-07 13:48 deploy
- `validated height advanced` log lines at or very near the JVM tip
- No other validation errors at the current testnet height

If all three are true, mark task #9 completed in the task list.

**If something broke overnight**: check the log for any new validation
errors. The most likely failure modes are:
- Some later epoch boundary with actual parameter votes that weren't
  captured in the simple default case (testnet probably doesn't vote
  though — this is unlikely)
- A block whose extension has a parameter ID we don't recognize beyond
  the ones we added — relax further or add the variant
- Non-voting-related issues (AVL tree corruption, bad AD proof, etc.)

## Task #10 — NiPoPoW serve exercise against JVM

**Status at session end**: handler is deployed and wired, but the JVM peer
in full mode doesn't proactively send `GetNipopowProof` requests. So the
code path has never been exercised end-to-end in production. The unit
tests in `src/nipopow_serve.rs` pass (parsers and serializers verified),
but the full loop `incoming code 90 → build_nipopow_proof → code 91 response`
hasn't been observed.

**Three testing options** (pick one):

### Option A: REST API smoke test (fastest)

Add a debug REST endpoint `/test/nipopow-proof` that:
1. Locks the chain Mutex
2. Calls `enr_chain::build_nipopow_proof(&chain, m=6, k=6, None)`
3. Returns `hex::encode(&proof_bytes)` in the response body

This verifies `build_nipopow_proof` works on the live chain without needing
a peer request. Then feed the returned bytes back in via the same endpoint
or a sibling `/test/verify-nipopow-proof` that calls
`enr_chain::verify_nipopow_proof_bytes`. If both succeed, the chain-side
implementation is sound.

This does NOT exercise the P2P wire format — those are covered by the
unit tests in `src/nipopow_serve.rs`. Good enough for a smoke test.

### Option B: Tiny test client (most thorough)

Write `tests/nipopow_serve_integration.rs` using `enr-p2p` to:
1. Connect as an outbound peer to the running Rust node (127.0.0.1:9030)
2. Handshake
3. Send `GetNipopowProof { m: 6, k: 6, header_id: None }` (code 90)
4. Wait for the code 91 response
5. Decode the proof and verify with `enr_chain::verify_nipopow_proof_bytes`
6. Assert success

~100 lines. Uses the existing `ergo_node_rust::nipopow_serve` codecs for
building/parsing. Exercises the full P2P path.

### Option C: Configure a light-client JVM peer

Run a second JVM node locally with `ergo.node.nipopowBootstrap = true`
and point it at our Rust node. It'll send a GetNipopowProof during
startup. Most realistic but heaviest setup. Probably overkill for this
task — save for actual light-client sync work.

**Recommendation**: Option A first (5 minutes), then Option B if you want
end-to-end certification (20 minutes).

**Completion criteria**:
- A `build_nipopow_proof` call on the live chain returns non-empty bytes
- The same bytes pass `verify_nipopow_proof_bytes`
- Either: Option B showed a successful GetNipopowProof → NipopowProof
  round-trip on the wire, OR the REST endpoint of Option A worked

## Environment quick reference

- **Test server**: `admin@216.128.144.28`, Debian 13
- **Service name**: `ergo-node-rust.service`
- **Data dir**: `/var/lib/ergo-node/data/`
- **Log**: `/var/log/ergo-node/ergo-node.log` (1.6G full run)
- **Rust node**: P2P on `[::]:9030`, REST on `127.0.0.1:9053`
- **JVM peer**: P2P on `127.0.0.1:9033`, REST on `127.0.0.1:9063`
- **Binary path**: `/usr/bin/ergo-node-rust`
- **Build + deploy**: `./build-deb && scp ... && ssh ... "sudo dpkg -i ..."`
- **Remember**: the postinst script only `start`s, it doesn't `restart`.
  After dpkg install, you must `sudo systemctl restart ergo-node-rust`
  to pick up the new binary.

## State of the codebase at session end

Four new commits on local main (not pushed):
- `52eb437` chore: initial sigma-rust rev bump + submodule pointers
- `5bacbcc` feat: in-repo voting + nipopow wiring
- `8891b3a` fix(voting): testnet block 128 three fixes
- `9377b30` chore: sigma-rust 836f2b32 + chain Phase B

Chain submodule at `efc3d1f` (Phase B).
Facts submodule at `e59b3d5`.
Sigma-rust integration branch at `836f2b32`.
Upstream PRs open: #848 (ErgoTreePredef), #850 (Parameter variants).

Two in-flight open prompts in `prompts/` (gitignored, locally-only):
- `chain-voting-network-aware.md` (consumed)
- `chain-phase-b-subblocks.md` (consumed)
- `sigma-rust-subblocks-variant.md` (consumed)

Can delete these when confident they're done with.

## Related memories to read before starting

- `project_testnet_block128_diagnosis.md` — ground truth for regression
- `project_release_roadmap.md` — the updated roadmap
- `project_sigma_rust_fork.md` — fork workflow reference
- `feedback_clear_high_context.md` — if dispatching to submodule sessions, /clear when they're above 250k tokens
- `feedback_facts_submodule_commit.md` — if changing contracts, commit + push facts first

## Follow-ups beyond these two tasks (low priority)

Documented in `project_release_roadmap.md` under "Follow-up fixes":
- API `/info` fullHeight bug
- Router forwards unknown P2P codes to all peers
- `SoftForkDisablingRules` (ID 124) consensus check not enforced
- Rule 409 guard for SubblocksPerBlock auto-insert

None are blocking. Address when convenient.
