//! Benchmark: glibc malloc vs mimalloc vs jemalloc under interpreter-like workloads.
//!
//! Run with each allocator variant:
//!   cargo test --release --test alloc_benchmark --no-default-features -- --nocapture
//!   cargo test --release --test alloc_benchmark --features mimalloc -- --nocapture
//!   cargo test --release --test alloc_benchmark --no-default-features --features jemalloc -- --nocapture

#[test]
fn allocator_benchmark() {
    use std::hint::black_box;
    use std::time::Instant;

    let alloc_name = if cfg!(feature = "mimalloc") {
        "mimalloc"
    } else if cfg!(feature = "jemalloc") {
        "jemalloc"
    } else {
        "glibc"
    };

    const ITERS: usize = 1_000_000;

    // =========================================================================
    // 1. Small Box alloc/dealloc — mimics interpreter Expr node creation
    // =========================================================================
    let start = Instant::now();
    for i in 0..ITERS {
        let b: Box<[u8; 64]> = Box::new([i as u8; 64]);
        black_box(&b);
    }
    let small_box = start.elapsed();

    // =========================================================================
    // 2. Vec growing — mimics result accumulation in eval loop
    // =========================================================================
    let start = Instant::now();
    for _ in 0..ITERS / 10 {
        let mut v: Vec<u64> = Vec::new();
        for j in 0..100 {
            v.push(j);
        }
        black_box(&v);
    }
    let vec_grow = start.elapsed();

    // =========================================================================
    // 3. Mixed sizes — mimics varied interpreter allocations
    //    (AST nodes, values, byte buffers, strings)
    // =========================================================================
    let start = Instant::now();
    for i in 0..ITERS {
        match i % 4 {
            0 => { black_box(Box::new([0u8; 32])); }
            1 => { black_box(Box::new([0u8; 128])); }
            2 => { black_box(Vec::<u8>::with_capacity(256)); }
            3 => { black_box(String::with_capacity(64)); }
            _ => unreachable!(),
        }
    }
    let mixed = start.elapsed();

    // =========================================================================
    // 4. Concurrent alloc/dealloc — mimics rayon par_iter validation
    //    (multiple threads allocating simultaneously)
    // =========================================================================
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let per_thread = ITERS / threads;
    let start = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                for i in 0..per_thread {
                    let b: Box<[u8; 64]> = Box::new([i as u8; 64]);
                    black_box(&b);
                    let v = vec![i as u8; 128];
                    black_box(&v);
                }
            });
        }
    });
    let concurrent = start.elapsed();

    // =========================================================================
    // 5. Churn — alloc, use briefly, dealloc in rapid succession
    //    (closest to interpreter eval: build node, evaluate, discard)
    // =========================================================================
    let start = Instant::now();
    for i in 0..ITERS {
        let node = Box::new((i as u64, vec![i as u8; 48], [0u8; 32]));
        let hash = node.0.wrapping_mul(31) ^ node.2[0] as u64;
        black_box(hash);
    }
    let churn = start.elapsed();

    // =========================================================================
    // Results
    // =========================================================================
    let ns = |d: std::time::Duration, n: usize| d.as_nanos() as f64 / n as f64;

    println!("\n{}", "=".repeat(60));
    println!("Allocator: {alloc_name}  ({threads} threads available)");
    println!("{}", "=".repeat(60));
    println!();
    println!("Small Box (64B) alloc+dealloc:  {:>6.1} ns/op  ({ITERS} ops)", ns(small_box, ITERS));
    println!("Vec<u64> grow to 100:           {:>6.1} ns/op  ({} ops)", ns(vec_grow, ITERS / 10), ITERS / 10);
    println!("Mixed sizes (32-256B):          {:>6.1} ns/op  ({ITERS} ops)", ns(mixed, ITERS));
    println!("Concurrent ({threads}T, Box+Vec):     {:>6.1} ns/op  ({ITERS} ops)", ns(concurrent, ITERS));
    println!("Churn (Box+Vec, build+discard): {:>6.1} ns/op  ({ITERS} ops)", ns(churn, ITERS));
    println!("{}", "=".repeat(60));
}
