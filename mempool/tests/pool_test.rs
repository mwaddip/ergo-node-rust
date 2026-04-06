use std::time::Instant;

use ergo_lib::chain::transaction::Transaction;
use ergo_lib::chain::transaction::input::UnsignedInput;
use ergo_lib::ergotree_ir::chain::ergo_box::{
    BoxId, ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters,
    box_value::BoxValue,
};
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use ergo_chain_types::Digest32;

use ergo_mempool::pool::OrderedPool;
use ergo_mempool::weight::TxWeight;
use ergo_mempool::types::{FeeStrategy, UnconfirmedTx};

/// Minimal ErgoTree that represents `true` proposition.
/// This is the simplest valid tree: header byte 0x00 + constant `true` as SigmaProp.
/// We use a known-good serialized form: v0, no constant segregation, DeserializeRegister fallback.
/// Actually, the simplest approach: header=0x00, body = BoolConst(true) wrapped in BoolToSigmaProp.
/// Hex: 00 cd 02 -- but let's just use the bytes sigma-rust can parse.
fn minimal_ergo_tree() -> ErgoTree {
    // Minimal valid ErgoTree: v0 header (0x00), then a SigmaProp constant (TrivialProp(true)).
    // This is the simplest tree that parses. Bytes from sigma-rust test fixtures.
    // Header: 0x00 (v0, no constant segregation, no size)
    // Body: 0x08 0xcd 0x02 -- SigmaPropConstant(ProveDlog(point)) doesn't work without a real point.
    //
    // Simpler: use `ErgoTree::sigma_parse_bytes` with known-good bytes.
    // A "height > 0" tree works: 0x00 0x01 0x73 0x00 0x04 0x01 0xd1 0x93 0x04 ...
    // Actually simplest: serialize `true` as a BoolConst then wrap.
    //
    // After investigation, the simplest valid ErgoTree is a raw SigmaProp constant.
    // Let's construct via ErgoTree struct directly since ErgoBoxCandidate just needs
    // any ErgoTree that can serialize/deserialize.
    //
    // Known-good: 0x08cd + 33 bytes of a generator point (standard EC point for P2PK).
    // But we don't need a VALID script for pool tests — we just need a Transaction object
    // that can be constructed. The ErgoTree just needs to serialize.
    //
    // Simplest: an unparseable tree is still accepted by ErgoBoxCandidate.
    // Let's use a v0 tree with a TRUE proposition: [0x00, 0xd1, 0x93, ...] — no, too complex.
    //
    // Actually the very simplest: header=0x00 then any SigmaSerializable expression.
    // We can use a Constant(true:SBool) wrapped in BoolToSigmaProp.
    // Serialized bytes: [0x00, 0xd1, 0x93, 0x00, 0x08]
    //
    // Let me just use the raw bytes approach that's been proven to work:
    // v0 header + SigmaPropValue constant with TrivialProp(true)
    //
    // From JVM reference: SigmaPropConstant(TrivialProp.TrueProp) serializes as:
    // [header=0x00][type=0x08][SigmaBoolConst=0xcd][tag=0x04(TrivialPropTrue)][flag=0x01]
    //
    // Actually, let me just try parsing a minimal tree and see what works.
    // The bytes [0x00, 0x08, 0xcd, 0x01] should be: header=v0, Constant(SigmaProp(TrivialTrue))

    // V0 header, body = BoolToSigmaProp(TrueLeaf)
    // TrueLeaf opcode = 0x9e (158 decimal), but let's not guess.
    //
    // Safest approach: build from expression, if available.
    // If that's too complex, use a known-good hex string from sigma-rust tests.
    //
    // From ergotree-ir test: the smallest working tree is just header + SigmaPropValue.
    // Let me try: 0x08cd + 33-byte compressed EC point.
    // We need any 33-byte compressed point. Generator point:
    // 02 + x-coord of secp256k1 generator = 0279BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798
    let tree_hex = "0008cd0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    let tree_bytes = hex::decode(tree_hex).unwrap();
    ErgoTree::sigma_parse_bytes(&tree_bytes).unwrap()
}

/// Build a BoxId from a seed byte. Each seed produces a unique, deterministic BoxId.
fn make_box_id(seed: u8) -> BoxId {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    let digest = Digest32::try_from(&bytes[..]).unwrap();
    BoxId::from(digest)
}

/// Build a minimal Transaction with one input (spending `input_box_id`) and one output.
fn make_tx(input_seed: u8, output_value: u64) -> Transaction {
    let unsigned_input = UnsignedInput::new(make_box_id(input_seed), ContextExtension::empty());
    let input = unsigned_input.input_to_sign();

    let output = ErgoBoxCandidate {
        value: BoxValue::new(output_value).unwrap(),
        ergo_tree: minimal_ergo_tree(),
        tokens: None,
        additional_registers: NonMandatoryRegisters::empty(),
        creation_height: 1,
    };

    Transaction::new_from_vec(
        vec![input],
        vec![],
        vec![output],
    ).unwrap()
}

/// Extract tx_id as [u8; 32].
fn tx_id_bytes(tx: &Transaction) -> [u8; 32] {
    let mut arr = [0u8; 32];
    arr.copy_from_slice(tx.id().as_ref());
    arr
}

/// Build an UnconfirmedTx from a Transaction with given fee.
fn make_utx(tx: Transaction, fee: u64) -> UnconfirmedTx {
    let tx_bytes = tx.sigma_serialize_bytes().unwrap();
    let cost = tx_bytes.len() as u32;
    let now = Instant::now();
    UnconfirmedTx {
        tx,
        tx_bytes,
        fee,
        cost,
        created: now,
        last_checked: now,
        source: None,
    }
}

/// Extract input box IDs from a Transaction.
fn input_box_ids(tx: &Transaction) -> Vec<[u8; 32]> {
    tx.inputs.iter().map(|i| {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(i.box_id.as_ref());
        arr
    }).collect()
}

/// Build output (box_id, ErgoBox) pairs from a Transaction.
fn output_box_pairs(tx: &Transaction) -> Vec<([u8; 32], ErgoBox)> {
    tx.outputs.iter().map(|b| {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(b.box_id().as_ref());
        (arr, b.clone())
    }).collect()
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[test]
fn insert_and_lookup() {
    let mut pool = OrderedPool::new(100);

    let tx = make_tx(1, 1_000_000);
    let tx_id = tx_id_bytes(&tx);
    let input_ids = input_box_ids(&tx);
    let outputs = output_box_pairs(&tx);
    let utx = make_utx(tx, 2_000_000);

    let weight = TxWeight::new(tx_id, 2_000_000, utx.tx_bytes.len(), utx.cost, FeeStrategy::FeePerByte);
    pool.insert(weight, utx, &input_ids, outputs);

    assert_eq!(pool.len(), 1);
    assert!(pool.contains(&tx_id));
    assert!(pool.get(&tx_id).is_some());
}

#[test]
fn ordering_by_fee() {
    let mut pool = OrderedPool::new(100);

    // Insert three txs with different fees
    let tx_low = make_tx(1, 1_000_000);
    let tx_mid = make_tx(2, 1_000_000);
    let tx_high = make_tx(3, 1_000_000);

    let _id_low = tx_id_bytes(&tx_low);
    let _id_mid = tx_id_bytes(&tx_mid);
    let _id_high = tx_id_bytes(&tx_high);

    let fee_low = 1_000_000u64;
    let fee_mid = 5_000_000u64;
    let fee_high = 10_000_000u64;

    for (tx, fee) in [(tx_low, fee_low), (tx_mid, fee_mid), (tx_high, fee_high)] {
        let tx_id = tx_id_bytes(&tx);
        let inputs = input_box_ids(&tx);
        let outputs = output_box_pairs(&tx);
        let utx = make_utx(tx, fee);
        let size = utx.tx_bytes.len();
        let cost = utx.cost;
        let weight = TxWeight::new(tx_id, fee, size, cost, FeeStrategy::FeePerByte);
        pool.insert(weight, utx, &inputs, outputs);
    }

    assert_eq!(pool.len(), 3);

    // top(1) should return the highest-fee tx
    let top = pool.top(1);
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].fee, fee_high, "top tx should have highest fee");

    // all_prioritized should be ordered highest first
    let all = pool.all_prioritized();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].fee, fee_high);
    assert_eq!(all[1].fee, fee_mid);
    assert_eq!(all[2].fee, fee_low);
}

#[test]
fn remove_cleans_indexes() {
    let mut pool = OrderedPool::new(100);

    let tx = make_tx(1, 1_000_000);
    let tx_id = tx_id_bytes(&tx);
    let input_ids = input_box_ids(&tx);
    let outputs = output_box_pairs(&tx);
    let output_box_id = outputs[0].0;
    let utx = make_utx(tx, 2_000_000);

    let weight = TxWeight::new(tx_id, 2_000_000, utx.tx_bytes.len(), utx.cost, FeeStrategy::FeePerByte);
    pool.insert(weight, utx, &input_ids, outputs);

    assert!(pool.contains(&tx_id));
    assert!(pool.spending_tx(&input_ids[0]).is_some());
    assert!(pool.unconfirmed_box(&output_box_id).is_some());

    // Remove
    let removed = pool.remove(&tx_id);
    assert!(removed.is_some());
    assert_eq!(pool.len(), 0);
    assert!(!pool.contains(&tx_id));
    assert!(pool.spending_tx(&input_ids[0]).is_none(), "input index should be cleaned");
    assert!(pool.unconfirmed_box(&output_box_id).is_none(), "output index should be cleaned");
}

#[test]
fn double_spend_detection() {
    let mut pool = OrderedPool::new(100);

    // Two txs spending the same input (input_seed=1) but with different output values
    // so they get different tx_ids.
    let tx_a = make_tx(1, 1_000_000);
    let tx_b = make_tx(1, 2_000_000);

    let id_a = tx_id_bytes(&tx_a);
    let id_b = tx_id_bytes(&tx_b);
    assert_ne!(id_a, id_b, "different txs should have different ids");

    // Both spend input box_id seed=1
    let shared_input = {
        let mut arr = [0u8; 32];
        arr[0] = 1;
        arr
    };

    // Insert tx_a
    let inputs_a = input_box_ids(&tx_a);
    let outputs_a = output_box_pairs(&tx_a);
    let utx_a = make_utx(tx_a, 3_000_000);
    let w_a = TxWeight::new(id_a, 3_000_000, utx_a.tx_bytes.len(), utx_a.cost, FeeStrategy::FeePerByte);
    pool.insert(w_a, utx_a, &inputs_a, outputs_a);

    // The input should be tracked as spent by tx_a
    let spender = pool.spending_tx(&shared_input).unwrap();
    assert_eq!(spender.tx_id, id_a);

    // Insert tx_b (same input) — pool doesn't prevent this itself,
    // the Mempool.process() handles conflict resolution.
    // But spending_tx will now return tx_b since HashMap overwrites.
    let inputs_b = input_box_ids(&tx_b);
    let outputs_b = output_box_pairs(&tx_b);
    let utx_b = make_utx(tx_b, 5_000_000);
    let w_b = TxWeight::new(id_b, 5_000_000, utx_b.tx_bytes.len(), utx_b.cost, FeeStrategy::FeePerByte);
    pool.insert(w_b, utx_b, &inputs_b, outputs_b);

    // spending_tx should now return tx_b (last writer wins in HashMap)
    let spender = pool.spending_tx(&shared_input).unwrap();
    assert_eq!(spender.tx_id, id_b);

    // Both should be in the pool (pool doesn't enforce conflict resolution)
    assert!(pool.contains(&id_a));
    assert!(pool.contains(&id_b));
    assert_eq!(pool.len(), 2);
}

#[test]
fn evict_lowest() {
    let mut pool = OrderedPool::new(100);

    let tx_low = make_tx(1, 1_000_000);
    let tx_high = make_tx(2, 1_000_000);

    let id_low = tx_id_bytes(&tx_low);
    let id_high = tx_id_bytes(&tx_high);

    let fee_low = 1_000_000u64;
    let fee_high = 10_000_000u64;

    // Insert low-fee tx
    let inputs = input_box_ids(&tx_low);
    let outputs = output_box_pairs(&tx_low);
    let utx = make_utx(tx_low, fee_low);
    let w = TxWeight::new(id_low, fee_low, utx.tx_bytes.len(), utx.cost, FeeStrategy::FeePerByte);
    pool.insert(w, utx, &inputs, outputs);

    // Insert high-fee tx
    let inputs = input_box_ids(&tx_high);
    let outputs = output_box_pairs(&tx_high);
    let utx = make_utx(tx_high, fee_high);
    let w = TxWeight::new(id_high, fee_high, utx.tx_bytes.len(), utx.cost, FeeStrategy::FeePerByte);
    pool.insert(w, utx, &inputs, outputs);

    assert_eq!(pool.len(), 2);

    // Evict lowest — should remove the low-fee tx
    let evicted = pool.evict_lowest().unwrap();
    assert_eq!(evicted, id_low, "should evict the lowest-fee tx");
    assert_eq!(pool.len(), 1);
    assert!(pool.contains(&id_high));
    assert!(!pool.contains(&id_low));
}

#[test]
fn lowest_weight_tracks_minimum() {
    let mut pool = OrderedPool::new(100);

    assert!(pool.lowest_weight().is_none(), "empty pool has no lowest weight");

    let tx1 = make_tx(1, 1_000_000);
    let id1 = tx_id_bytes(&tx1);
    let inputs = input_box_ids(&tx1);
    let outputs = output_box_pairs(&tx1);
    let utx = make_utx(tx1, 5_000_000);
    let w1 = TxWeight::new(id1, 5_000_000, utx.tx_bytes.len(), utx.cost, FeeStrategy::FeePerByte);
    let expected_weight = w1.fee_per_factor;
    pool.insert(w1, utx, &inputs, outputs);

    assert_eq!(pool.lowest_weight(), Some(expected_weight));

    // Add a lower-fee tx
    let tx2 = make_tx(2, 1_000_000);
    let id2 = tx_id_bytes(&tx2);
    let inputs = input_box_ids(&tx2);
    let outputs = output_box_pairs(&tx2);
    let utx = make_utx(tx2, 1_000_000);
    let w2 = TxWeight::new(id2, 1_000_000, utx.tx_bytes.len(), utx.cost, FeeStrategy::FeePerByte);
    let lower_weight = w2.fee_per_factor;
    pool.insert(w2, utx, &inputs, outputs);

    assert_eq!(pool.lowest_weight(), Some(lower_weight));
}

#[test]
fn is_full_respects_capacity() {
    let mut pool = OrderedPool::new(2);

    assert!(!pool.is_full());

    let tx1 = make_tx(1, 1_000_000);
    let id1 = tx_id_bytes(&tx1);
    let inputs = input_box_ids(&tx1);
    let outputs = output_box_pairs(&tx1);
    let utx = make_utx(tx1, 2_000_000);
    let w = TxWeight::new(id1, 2_000_000, utx.tx_bytes.len(), utx.cost, FeeStrategy::FeePerByte);
    pool.insert(w, utx, &inputs, outputs);
    assert!(!pool.is_full());

    let tx2 = make_tx(2, 1_000_000);
    let id2 = tx_id_bytes(&tx2);
    let inputs = input_box_ids(&tx2);
    let outputs = output_box_pairs(&tx2);
    let utx = make_utx(tx2, 3_000_000);
    let w = TxWeight::new(id2, 3_000_000, utx.tx_bytes.len(), utx.cost, FeeStrategy::FeePerByte);
    pool.insert(w, utx, &inputs, outputs);
    assert!(pool.is_full());
}

#[test]
fn tx_ids_returns_all() {
    let mut pool = OrderedPool::new(100);

    let tx1 = make_tx(1, 1_000_000);
    let tx2 = make_tx(2, 1_000_000);
    let id1 = tx_id_bytes(&tx1);
    let id2 = tx_id_bytes(&tx2);

    for (tx, fee) in [(tx1, 2_000_000u64), (tx2, 3_000_000u64)] {
        let tx_id = tx_id_bytes(&tx);
        let inputs = input_box_ids(&tx);
        let outputs = output_box_pairs(&tx);
        let utx = make_utx(tx, fee);
        let w = TxWeight::new(tx_id, fee, utx.tx_bytes.len(), utx.cost, FeeStrategy::FeePerByte);
        pool.insert(w, utx, &inputs, outputs);
    }

    let ids = pool.tx_ids();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&id1));
    assert!(ids.contains(&id2));
}

#[test]
fn update_weight_reorders() {
    let mut pool = OrderedPool::new(100);

    let tx_a = make_tx(1, 1_000_000);
    let tx_b = make_tx(2, 1_000_000);

    let id_a = tx_id_bytes(&tx_a);
    let id_b = tx_id_bytes(&tx_b);

    // tx_a starts with low fee, tx_b with high fee
    let inputs_a = input_box_ids(&tx_a);
    let outputs_a = output_box_pairs(&tx_a);
    let utx_a = make_utx(tx_a, 1_000_000);
    let w_a = TxWeight::new(id_a, 1_000_000, utx_a.tx_bytes.len(), utx_a.cost, FeeStrategy::FeePerByte);
    pool.insert(w_a, utx_a, &inputs_a, outputs_a);

    let inputs_b = input_box_ids(&tx_b);
    let outputs_b = output_box_pairs(&tx_b);
    let utx_b = make_utx(tx_b, 5_000_000);
    let w_b = TxWeight::new(id_b, 5_000_000, utx_b.tx_bytes.len(), utx_b.cost, FeeStrategy::FeePerByte);
    pool.insert(w_b, utx_b, &inputs_b, outputs_b);

    // Before update: tx_b is top
    assert_eq!(pool.top(1)[0].fee, 5_000_000);

    // Boost tx_a's weight above tx_b
    let very_high = 999_999_999;
    pool.update_weight(&id_a, very_high);

    // Now tx_a should be top
    assert_eq!(pool.top(1)[0].fee, 1_000_000, "boosted tx_a should now be at the top");
}
