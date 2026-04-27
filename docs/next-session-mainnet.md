# Session 18 — Mainnet Support (v0.2)

> **Archived session handoff** — mainnet support landed in v0.2.0.

Load CLAUDE.md, SESSION_CONTEXT.md, ergo-node-development skill, and memory.

## Goal

Make the node run on mainnet. The codebase is already network-parametric
(chain config, voting, emission, P2P magic, API ports all switch on the
`Network` enum). What remains is filling in the mainnet-specific constants
and testing against real mainnet data.

## Task 1: Mainnet genesis boxes (the one `unimplemented!()`)

`src/main.rs:65-67` has:
```rust
enr_p2p::types::Network::Mainnet => {
    unimplemented!("mainnet genesis not yet implemented")
}
```

This is in `build_genesis_boxes()`. It needs mainnet no-premine proof
strings and founders' public keys.

**Mainnet no-premine proofs** (from JVM `mainnet.conf:24-30`):
```
"00000000000000000014c2e2e7e33d51ae7e66f6ccb6942c3437127b36c33747"
"0xd07a97293468d9132c5a2adab2e52a23009e6798608e47b0d2623c7e3e923463"
"Brexit: both Tory sides play down risk of no-deal after business alarm"
"述评：平衡、持续、包容——新时代应对全球化挑战的中国之道"
"Дивиденды ЧТПЗ вырастут на 33% на акцию"
```

**Founders' public keys** — same as testnet (already in the code):
```
"039bb5fe52359a64c99a60fd944fc5e388cbdc4d37ff091cc841c3ee79060b8647"
"031fb52cf6e805f80d97cde289f4f757d49accf0c83fb864b27d2cf982c37f9a8b"
"0352ac2a471339b0d23b3d2c5ce0db0e81c969f77891b9edf0bda7fd39a78184e7"
```

Note: testnet and mainnet share the same founders' PKs (verified in JVM
`application.conf:209-213`). The proof strings differ (testnet uses March
2019 headlines, mainnet uses July 2019).

**Verification**: After implementing, write a test `mainnet_genesis_boxes_produce_correct_digest`
mirroring the existing `testnet_genesis_boxes_produce_correct_digest`. The
expected digest is `MAINNET_GENESIS_DIGEST` (already defined in main.rs:52-53):
```
a5df145d41ab15a01e0cd3ffbab046f0d029e5412293072ad0f5827428589b9302
```

## Task 2: Mainnet initial difficulty (chain submodule)

`chain/src/config.rs:67` has `initial_n_bits: 16842752` for mainnet — this
is `encode_compact_bits(1)`, same as testnet. But mainnet launched with a
higher initial difficulty.

From JVM `mainnet.conf:18`: `initialDifficultyHex = "011765000000"`
That's `BigInt(1199990374400)`. The JVM converts this to n_bits via
`DifficultySerializer.encodeCompactBits(initialDifficulty)` (line 52 of
`ChainSettings.scala`).

Compute the correct n_bits for this difficulty and update
`ChainConfig::mainnet()`. Use `decode_compact_bits` / `encode_compact_bits`
from `ergo-chain-types` for the conversion.

Also: JVM mainnet.conf has `version2ActivationDifficultyHex = "6f98d5000000"`
at the Autolykos v2 hard fork (height 417,792). Check whether our difficulty
adjustment code needs this. The JVM stores it as
`chainSettings.initialDifficultyVersion2` and uses it in
`DifficultyAdjustment` for the v2 transition. If our difficulty code
doesn't handle this transition, it will diverge at block 417,792.

## Task 3: Mainnet genesis ID validation

JVM `mainnet.conf:21`:
```
genesisId = "b0244dfc267baca974a4caee06120321562784303a8a688976ae56170e4d175b"
```

The JVM validates that the genesis block ID matches this value. Check
whether our code validates this. If not, add it — the genesis ID is a
commitment to the genesis block contents. A mismatch means we computed
genesis boxes wrong.

## Task 4: Mainnet seed peers

JVM `mainnet.conf:129-143` lists 13 seed peers (12 IPv4 + 1 IPv6).
Create a `mainnet.toml` config file (similar to `local-standalone.toml`)
with these peers. The P2P port for mainnet is 9030.

Key peers from the JVM config:
```
213.239.193.208:9030
159.65.11.55:9030
165.227.26.175:9030
159.89.116.15:9030
136.244.110.145:9030
94.130.108.35:9030
51.75.147.1:9020
221.165.214.185:9030
217.182.197.196:9030
173.212.220.9:9030
176.9.65.58:9130
213.152.106.56:9030
[2001:41d0:700:6662::]:29031
```

## Task 5: Mainnet smoke test

Start the node with `network = "mainnet"` and verify:
1. Genesis boxes produce the correct digest
2. Handshake succeeds with mainnet peers (magic bytes `[1,0,2,4]`)
3. Headers download and validate (PoW, difficulty, timestamps)
4. The first epoch boundary (height 1024) passes difficulty adjustment
5. The Autolykos v2 transition at height 417,792 is handled correctly

The full mainnet chain is 1.2M+ blocks — don't wait for full sync, but
verify at least the first few thousand headers and the v2 transition
boundary.

## Task 6: Mainnet voting parameters

JVM `mainnet.conf:36-41`:
```
version2ActivationHeight = 417792
version2ActivationDifficultyHex = "6f98d5000000"
```

And `mainnet.conf:110-119` has active votes and disabled rules for 6.0:
```
120 = 1  // vote for 6.0 soft-fork
rulesToDisable = [215, 409]
```

Check that our `VotingConfig::mainnet()` matches. The disabled rules are
applied at the protocol level — verify they're handled.

## Approach

Tasks 1-4 are mechanical — port constants from JVM config. Task 5 is the
real test: sync against mainnet and see what breaks. Task 6 may surface
edge cases at specific heights.

Work through sequentially. Task 1 first (removes the `unimplemented!()`),
then 2-3 (chain submodule prompt for difficulty), then 4 (config), then
5-6 (smoke test + fix whatever breaks).

## Reference files

- JVM mainnet config: `~/projects/ergo-node-build/src/main/resources/mainnet.conf`
- JVM defaults: `~/projects/ergo-node-build/src/main/resources/application.conf`
- JVM ChainSettings: `~/projects/ergo-node-build/ergo-core/src/main/scala/org/ergoplatform/settings/ChainSettings.scala`
- JVM DifficultyAdjustment: `~/projects/ergo-node-build/ergo-core/src/main/scala/org/ergoplatform/mining/difficulty/DifficultyAdjustment.scala`
- JVM ErgoState genesis: `~/projects/ergo-node-build/src/main/scala/org/ergoplatform/nodeView/state/ErgoState.scala`
