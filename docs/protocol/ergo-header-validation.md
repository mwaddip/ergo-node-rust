# Ergo Header Serialization & Validation Specification

**Version:** 1.0 (documenting protocol version 6.0.x)
**Status:** Reverse-engineered from JVM reference node (ergoplatform/ergo v6.0.3), verified against sigma-rust (ergo-chain-types), and validated by syncing a JVM node through a Rust relay on testnet.
**Date:** 2026-03-31

## Overview

Block headers are the primary unit of chain validation in the Ergo protocol. A header contains the cryptographic links to its parent, the block's state transitions (Merkle roots), the proof-of-work solution, and voting data. This document specifies the wire serialization format and the validation rules for headers.

No official specification exists for header serialization. The JVM reference node's `HeaderSerializer.scala` and sigma-rust's `ergo-chain-types` crate are the two implementations. This spec was produced by cross-referencing both against observed testnet traffic.

## Modifier Type ID

Headers use modifier type ID **101** (0x65) in P2P Inv, ModifierRequest, and ModifierResponse messages. This is the `NetworkObjectTypeId` from the JVM source, defined in `NetworkObjectTypeId.scala`.

**Warning:** Informal Ergo documentation and community references frequently cite modifier type 1 for headers. This is wrong. All block section type IDs are >= 50 (the `BlockSectionThreshold`). The complete mapping:

| Type ID | Modifier |
|---------|----------|
| 101 | Header |
| 102 | BlockTransactions |
| 104 | ADProofs |
| 108 | Extension |

Transaction type (2) appears in mempool Inv messages only, not in block modifier responses.

## Header Serialization Format

Headers are serialized using the Scorex framework. The format differs between version 1 (Autolykos v1) and version 2+ (Autolykos v2).

### Common Fields (all versions)

Serialized in this exact order:

| Field | Encoding | Size | Notes |
|-------|----------|------|-------|
| `version` | raw byte | 1 | Header version (1 = Autolykos v1, 2+ = Autolykos v2) |
| `parent_id` | raw bytes | 32 | Blake2b-256 hash of parent header |
| `ad_proofs_root` | raw bytes | 32 | Merkle root of AD proofs |
| `transactions_root` | raw bytes | 32 | Merkle root of block transactions |
| `state_root` | raw bytes | **33** | AVL+ tree digest. 33 bytes, not 32 — the extra byte is the AVL tree height |
| `timestamp` | VLQ unsigned | 1–10 | Milliseconds since Unix epoch |
| `extension_root` | raw bytes | 32 | Merkle root of extension section |
| `n_bits` | 4-byte big-endian | 4 | Compact difficulty target. **NOT VLQ** — this is one of the few fixed-width fields |
| `height` | VLQ unsigned | 1–5 | Block height |
| `votes` | raw bytes | 3 | Three voting bytes (soft-fork parameter voting) |

### Version-Dependent Fields

After `votes`, for version > 1:

| Field | Encoding | Size | Notes |
|-------|----------|------|-------|
| `unparsed_bytes_length` | raw byte | 1 | Length of additional data (currently 0 on all networks) |
| `unparsed_bytes` | raw bytes | variable | Reserved for future protocol extensions |

### Autolykos Solution

Immediately follows the header fields. Format depends on header version.

#### Version 1 (Autolykos v1)

| Field | Encoding | Size | Notes |
|-------|----------|------|-------|
| `miner_pk` | raw bytes | 33 | Compressed EC point (secp256k1) |
| `pow_onetime_pk` | raw bytes | 33 | Compressed EC point (one-time public key) |
| `nonce` | raw bytes | 8 | Mining nonce |
| `d_length` | raw byte | 1 | Length of distance bytes |
| `pow_distance` | raw bytes | variable | **Unsigned** big-endian integer. See encoding note below. |

#### Version 2+ (Autolykos v2)

| Field | Encoding | Size | Notes |
|-------|----------|------|-------|
| `miner_pk` | raw bytes | 33 | Compressed EC point (secp256k1) |
| `nonce` | raw bytes | 8 | Mining nonce |

Version 2 **omits** `pow_onetime_pk` and `pow_distance` entirely. They are not present on the wire. After deserialization, these fields are `None` / absent.

### pow_distance Encoding (V1 only)

The `pow_distance` value (`d`) is serialized as **unsigned** big-endian bytes. The JVM uses `BigIntegers.asUnsignedByteArray()` for serialization and `BigIntegers.fromUnsignedByteArray()` for parsing.

**Known bug in sigma-rust 0.15.0:** The published crate uses `BigInt::from_signed_bytes_be()` to parse `d`, which interprets the bytes as signed two's-complement. If the high bit of the first byte is set, the value is incorrectly interpreted as negative. The correct parsing uses unsigned interpretation (`BigUint::from_bytes_be()` or equivalent).

## Header ID Computation

The header ID is **not transmitted on the wire**. It is computed after deserialization as:

```
header_id = blake2b256(serialize_without_pow(header) ++ serialize_pow_solution(version, solution))
```

Where:
- `serialize_without_pow` produces all fields from `version` through `votes` (and `unparsed_bytes` for v2+)
- `serialize_pow_solution` produces the Autolykos solution fields (version-dependent, as above)
- `++` is byte concatenation
- `blake2b256` is the full 32-byte Blake2b-256 hash

Both the JVM and sigma-rust implementations create the header with a dummy/zero ID, serialize the full header, hash the result, and assign the computed ID.

## Proof-of-Work Verification

### Autolykos v2 (current network)

PoW verification checks that the miner's solution meets the difficulty target declared in the header.

#### Algorithm

```
hit = pow_hit(header)                              # implementation-specific, uses Autolykos v2 algorithm
target = secp256k1_group_order / decode_compact_bits(n_bits)
valid = (hit < target)                             # STRICT less-than
```

#### Components

**`pow_hit(header)`**: Computes the Autolykos v2 proof-of-work hash. This involves:
1. Computing a seed from the header (excluding nonce)
2. Generating 32 indices via `gen_indexes` using the seed hash and table size N
3. Looking up or computing values at those indices
4. Combining them to produce a final `BigInt` hit value

The table size N grows with height: `N = 2^26` initially (height < 614,400), doubling in steps after that.

**`decode_compact_bits(n_bits)`**: Decodes the compact difficulty representation. `n_bits` is a 4-byte big-endian value encoding a large integer in a mantissa/exponent format (same as Bitcoin's "compact" format).

**`secp256k1_group_order`**: The order of the secp256k1 scalar field:
```
0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
```

This is called `q` in the JVM source (`mining.scala`).

**`target`**: Integer (truncating) division of the group order by the decoded difficulty. Both JVM (`BigInt./`) and Rust (`BigInt` division) use truncating semantics.

#### Comparison Operator

**Validation uses strict less-than (`<`).** The JVM source (`AutolykosPowScheme.scala`, `checkPoWForVersion2`) explicitly checks `hit < b` where `b` is the target.

The JVM **mining** path (`checkNonces`) uses less-than-or-equal (`<=`), accepting the boundary case. This difference is theoretically relevant but practically negligible — the probability of `hit == target` with these number sizes is astronomically small.

For consensus correctness: use `<` (strict).

#### Known Bug: gen_indexes Zero Modulo

`gen_indexes` extracts 32 4-byte windows from a seed hash, interprets each as a big-endian unsigned integer, and computes `value % N` to get a table index. When the result is exactly zero (value is an exact multiple of N), the JVM correctly returns 0 via `BigInt.toInt`.

**Known bug in sigma-rust 0.15.0:** The Rust implementation calls `BigInt::to_u32_digits()` which returns `(NoSign, vec![])` for zero. Indexing `.1[0]` on the empty vector panics. With N ~67M and 32 indices per header, this has a non-trivial cumulative probability across all mainnet blocks during initial sync.

Fix: replace `.1[0]` with `.1.first().copied().unwrap_or(0)`.

### Autolykos v1 (historical, pre-hardfork)

V1 uses a different algebraic verification involving group element checks (the `pow_onetime_pk` and `pow_distance` fields). The basic `hit < target` check still applies, but additional algebraic equations must hold. V1 blocks only exist below the hardfork height and are not produced on current networks.

## n_bits Encoding

The `n_bits` field uses the compact difficulty encoding (same concept as Bitcoin's `nBits`):

- Byte 0: exponent (number of bytes in the full target)
- Bytes 1-3: mantissa (most significant 3 bytes of the target)

The field is serialized as **4 bytes big-endian** on the wire. This is notable because almost everything else in the Ergo protocol uses VLQ encoding. The `DifficultySerializer` in the JVM explicitly reads/writes 4 fixed bytes.

**Note on type:** The JVM uses `Long` (i64) for `n_bits` because Scala/Java lack unsigned integers. The wire format is 4 bytes, so the effective type is u32. sigma-rust 0.15.0 stores it as `u64`; a fork corrects this to `u32`.

## Chain Sync Protocol Behavior

During initial header chain synchronization, the JVM node does **not** use the Inv→ModifierRequest→ModifierResponse flow. Instead:

1. Node sends `SyncInfo` (message code 65) containing its known header IDs
2. Peer responds with `Inv` for headers the node doesn't have
3. Node sends `ModifierRequest` for those header IDs
4. Peer responds with `ModifierResponse` containing serialized headers

The critical detail: the modifier IDs in step 3 come from the `Inv` in step 2, which was triggered by `SyncInfo`, not by a previous unsolicited `Inv` announcement. Any relay or proxy that routes `ModifierRequest` exclusively via an inv table (tracking unsolicited `Inv` announcements) will fail to forward sync-initiated requests.

This was verified on testnet: a JVM node syncing exclusively through a Rust relay accumulated `NonDeliveryPenalty` until the relay was modified to fall back to any available outbound peer when the inv table had no entry for a requested modifier ID.

## Sources

- JVM reference node: `ergoplatform/ergo` v6.0.3 branch
- Header serializer: `ergo-core/src/main/scala/org/ergoplatform/modifiers/history/header/HeaderSerializer.scala`
- PoW scheme: `ergo-core/src/main/scala/org/ergoplatform/mining/AutolykosPowScheme.scala`
- Difficulty serializer: `ergo-core/src/main/scala/org/ergoplatform/mining/difficulty/DifficultySerializer.scala`
- Network object type IDs: `ergo-core/src/main/scala/org/ergoplatform/modifiers/NetworkObjectTypeId.scala`
- sigma-rust: `ergo-chain-types/src/` (Header, AutolykosPowScheme, AutolykosSolution serializers)
- Testnet validation: JVM node syncing header chain through ergo-node-rust relay, 2026-03-31
