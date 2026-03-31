# Ergo Difficulty Adjustment Specification

**Version:** 1.0 (documenting protocol version 6.0.x)
**Status:** Reverse-engineered from JVM reference node (ergoplatform/ergo v6.0.3), verified by cross-referencing Rust implementation against JVM source and test vectors.
**Date:** 2026-03-31

## Overview

Ergo uses epoch-based difficulty adjustment with linear regression for smoothing. The algorithm recalculates difficulty at the first block of each new epoch based on the observed block times over recent epochs.

Two algorithms exist:
- **Pre-EIP-37** (`calculate`): Linear regression over multiple epochs. Used on testnet always, and on mainnet before height 844,673.
- **EIP-37** (`eip37Calculate`): Averages the linear regression approach with a classic Bitcoin-style calculation, with 50%–150% capping. Used on mainnet from height 844,673 onward.

No official specification exists. This document is derived from `DifficultyAdjustment.scala` in the JVM reference node.

## Chain Parameters

| Parameter | Testnet | Mainnet (pre-EIP-37) | Mainnet (post-EIP-37) |
|-----------|---------|----------------------|-----------------------|
| `epoch_length` | 128 | 1024 | 128 |
| `block_interval` | 45s | 120s | 120s |
| `use_last_epochs` | 8 | 8 | 8 |
| `initial_difficulty` | 1 | from config | from config |
| EIP-37 activation | never | height 844,673 | — |

## When Difficulty Recalculates

Difficulty recalculates when the **parent** of the new block is at an epoch boundary:

```
recalculate = (parent.height % epoch_length == 0) && (parent.height > 0)
```

If not at a boundary, the new block inherits `parent.n_bits` unchanged.

The `epoch_length` used depends on network and height:
- Testnet: always 128
- Mainnet before 844,673: 1024
- Mainnet from 844,673: 128

## Headers Required for Recalculation

When recalculation is triggered for a block at height `h`, the algorithm needs headers at specific past epoch boundaries.

```
fn previous_heights_for_recalculation(h, epoch_length, use_last_epochs):
    if (h - 1) % epoch_length == 0 && epoch_length > 1:
        # Normal case: collect epoch boundary heights, filter out negative
        heights = (0..=use_last_epochs).map(|i| (h-1) - i * epoch_length)
                                       .filter(|h| h >= 0)
                                       .reverse()
        return heights

    else if (h - 1) % epoch_length == 0 && h > epoch_length * use_last_epochs:
        # Special case: epoch_length == 1 with enough history
        # No filter needed — all heights guaranteed non-negative
        heights = (0..=use_last_epochs).map(|i| (h-1) - i * epoch_length)
                                       .reverse()
        return heights

    else:
        # Mid-epoch: only need the parent
        return [h - 1]
```

For testnet with `epoch_length=128` and `use_last_epochs=8`, the first recalculation at height 129 needs headers at heights `[0, 128]` (2 headers — only one epoch of history). By height 1153 (`128 * 9 + 1`), it would request heights `[0, 128, 256, 384, 512, 640, 768, 896, 1024, 1152]` (9 headers = 8 epoch intervals).

## Pre-EIP-37 Algorithm: `calculate`

### Input

A sequence of headers at epoch boundary heights (from `previous_heights_for_recalculation`).

### Algorithm

**Step 1: Edge cases**

If only one header, or if the first header's timestamp >= the last header's timestamp (timestamps not increasing), return `required_difficulty(headers[0])` (the first header's decoded nBits).

**Step 2: Build data points**

For each consecutive pair of epoch-boundary headers `(start, end)`:

```
desired_ms = block_interval_ms * epoch_length
time_delta = end.timestamp - start.timestamp
difficulty = end.required_difficulty * desired_ms / time_delta
data_point = (end.height, difficulty)
```

This produces one data point per epoch interval. The difficulty is what it *should have been* if blocks arrived at the target rate.

**Step 3: Linear regression**

Fit `y = a + bx` to the data points using integer arithmetic with precision scaling:

```
PRECISION = 1,000,000,000

n = data.len()
xy_sum = Σ(x * y)
x_sum  = Σ(x)
x2_sum = Σ(x²)
y_sum  = Σ(y)

b = (xy_sum * n - x_sum * y_sum) * PRECISION / (x2_sum * n - x_sum²)
a = (y_sum * PRECISION - b * x_sum) / n / PRECISION

# Extrapolate to next epoch boundary
point = max(data.x) + epoch_length
result = a + b * point / PRECISION
```

For a single data point, return it directly (no regression).

**Step 4: Floor and normalize**

If `result < 1`, use `initial_difficulty` instead.

Normalize through a compact encoding roundtrip:
```
n_bits = encode_compact_bits(result)
final_difficulty = decode_compact_bits(n_bits)
```

This roundtrip is consensus-critical — it discards low-order bits that can't be represented in the compact format (~24 bits mantissa), ensuring all nodes agree on the exact difficulty value.

## EIP-37 Algorithm: `eip37Calculate`

Used on mainnet from height 844,673. Requires at least 2 epoch-boundary headers.

### Algorithm

**Step 1: Predictive difficulty (capped)**

```
last_diff = required_difficulty(headers.last())
predictive = calculate(headers, epoch_length)    # the pre-EIP-37 algorithm

if predictive > last_diff:
    limited_predictive = min(predictive, last_diff * 3 / 2)
else:
    limited_predictive = max(predictive, last_diff / 2)
```

**Step 2: Classic Bitcoin-style difficulty**

Uses only the last two epoch-boundary headers:

```
start = headers[second_to_last]
end = headers[last]
classic = end.required_difficulty * block_interval_ms * epoch_length / (end.timestamp - start.timestamp)
```

**Step 3: Average and cap**

```
avg = (classic + limited_predictive) / 2

if avg > last_diff:
    result = min(avg, last_diff * 3 / 2)
else:
    result = max(avg, last_diff / 2)
```

**Step 4: Normalize**

Same encode/decode roundtrip as pre-EIP-37.

## Compact Difficulty Encoding (n_bits)

The `n_bits` field uses a 4-byte big-endian compact format (same concept as Bitcoin):

- **Byte 0**: exponent — the number of bytes in the full target representation
- **Bytes 1–3**: mantissa — the most significant 3 bytes of the target

The field is serialized as **4 bytes big-endian** on the wire, NOT VLQ. This is handled by `DifficultySerializer` in the JVM, outside the Scorex VLQ framework.

### encode / decode roundtrip

The roundtrip `decode_compact_bits(encode_compact_bits(x))` is **lossy** (keeps only ~24 bits of mantissa) but **idempotent** (applying it twice gives the same result as once). This normalization is applied to all computed difficulties before comparison, ensuring consensus across implementations.

## Header Validation: Difficulty Check

When validating a non-genesis header:

```
expected = expected_difficulty_after(parent)
valid = (header.n_bits == expected)
```

For genesis:

```
valid = (header.n_bits == initial_n_bits)
```

Where `initial_n_bits = encode_compact_bits(initial_difficulty)`.

## Mainnet-Specific: Version 2 Activation

At the v2 activation height (Autolykos v1 → v2 transition), the JVM uses a special `initialDifficultyVersion2` value instead of computing it from the epoch. This is a one-time override to account for the mining algorithm change. This is mainnet-only and doesn't affect testnet.

## Integer Arithmetic Notes

All difficulty calculations use arbitrary-precision integers (`BigInt`). Division is truncating (toward zero) in both JVM (`BigInt./`) and Rust (`num_bigint`). The `PRECISION` constant (10^9) is used to maintain precision in the linear regression without floating-point arithmetic.

The `required_difficulty` of a header is `decode_compact_bits(header.n_bits)` — always a positive integer.

## Sources

- JVM reference: `ergo-core/src/main/scala/org/ergoplatform/mining/difficulty/DifficultyAdjustment.scala`
- Difficulty serializer: `ergo-core/src/main/scala/org/ergoplatform/mining/difficulty/DifficultySerializer.scala`
- Chain settings: `ergo-core/src/main/scala/org/ergoplatform/settings/ChainSettings.scala`
- Header validation: `src/main/scala/org/ergoplatform/nodeView/history/storage/modifierprocessors/HeadersProcessor.scala`
- EIP-37 discussion: Ergo Improvement Proposal 37
- Test vectors: `ergo-core/src/test/scala/org/ergoplatform/mining/difficulty/DifficultyAdjustmentSpecification.scala`
- Rust implementation verified against all JVM source paths, 2026-03-31
