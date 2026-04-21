//! Benchmark: k256 (pure Rust) vs libsecp256k1 (C+asm) for EC operations.
//!
//! Run with: cargo test --release -p ergo-node-rust ec_benchmark -- --nocapture
//!
//! Both libraries operate on secp256k1. The hot path in sigma protocol
//! verification is arbitrary point * scalar (~37% of sync CPU). This test
//! measures whether libsecp256k1 delivers the claimed 2-3x speedup.

#[test]
fn ec_performance_comparison() {
    use std::hint::black_box;
    use std::time::Instant;

    const N: usize = 10_000;

    // --- k256 setup ---
    let k_scalar_a = k256_scalar(&[0x42; 32]);
    let k_scalar_b = k256_scalar(&[0x13; 32]);
    let k_point = {
        use k256::elliptic_curve::ops::MulByGenerator;
        k256::ProjectivePoint::mul_by_generator(&k_scalar_a)
    };

    // --- secp256k1 setup ---
    let secp = secp256k1::Secp256k1::new();
    let s_sk_a = secp256k1::SecretKey::from_slice(&[0x42; 32]).unwrap();
    let s_pk = secp256k1::PublicKey::from_secret_key(&secp, &s_sk_a);
    let s_sk_b = secp256k1::SecretKey::from_slice(&[0x13; 32]).unwrap();
    let s_tweak = secp256k1::Scalar::from(s_sk_b);

    // =========================================================================
    // 1. Generator * scalar
    // =========================================================================

    // k256
    let start = Instant::now();
    for _ in 0..N {
        use k256::elliptic_curve::ops::MulByGenerator;
        black_box(k256::ProjectivePoint::mul_by_generator(black_box(&k_scalar_b)));
    }
    let k_gen = start.elapsed();

    // libsecp256k1
    let start = Instant::now();
    for _ in 0..N {
        black_box(secp256k1::PublicKey::from_secret_key(
            black_box(&secp),
            black_box(&s_sk_b),
        ));
    }
    let s_gen = start.elapsed();

    // =========================================================================
    // 2. Arbitrary point * scalar (THE HOT PATH)
    // =========================================================================

    // k256
    let start = Instant::now();
    for _ in 0..N {
        black_box(black_box(k_point) * black_box(k_scalar_b));
    }
    let k_mul = start.elapsed();

    // libsecp256k1
    let start = Instant::now();
    for _ in 0..N {
        black_box(
            black_box(s_pk)
                .mul_tweak(black_box(&secp), black_box(&s_tweak))
                .unwrap(),
        );
    }
    let s_mul = start.elapsed();

    // =========================================================================
    // 3. Full dlog verification: g^z * (h^e)^(-1)
    //    This is what runs per transaction input during block validation.
    // =========================================================================

    // k256 composite
    let start = Instant::now();
    for _ in 0..N {
        use k256::elliptic_curve::ops::MulByGenerator;
        let g_z = k256::ProjectivePoint::mul_by_generator(black_box(&k_scalar_b));
        let h_e = black_box(k_point) * black_box(k_scalar_b);
        let result = g_z + (-h_e);
        black_box(result);
    }
    let k_dlog = start.elapsed();

    // libsecp256k1 composite
    let start = Instant::now();
    for _ in 0..N {
        let g_z = secp256k1::PublicKey::from_secret_key(black_box(&secp), black_box(&s_sk_b));
        let h_e = black_box(s_pk)
            .mul_tweak(black_box(&secp), black_box(&s_tweak))
            .unwrap();
        let h_e_neg = h_e.negate(&secp);
        let result = g_z.combine(&h_e_neg).unwrap();
        black_box(result);
    }
    let s_dlog = start.elapsed();

    // =========================================================================
    // Results
    // =========================================================================

    let us = |d: std::time::Duration| d.as_micros() as f64 / N as f64;
    let ratio = |a: std::time::Duration, b: std::time::Duration| {
        a.as_nanos() as f64 / b.as_nanos() as f64
    };

    println!("\n{}", "=".repeat(60));
    println!("EC Performance: k256 vs libsecp256k1  ({N} iterations, --release)");
    println!("{}", "=".repeat(60));
    println!();
    println!("Generator * scalar:");
    println!("  k256:         {:>7.1} us/op", us(k_gen));
    println!("  libsecp256k1: {:>7.1} us/op", us(s_gen));
    println!("  speedup:      {:>7.1}x", ratio(k_gen, s_gen));
    println!();
    println!("Point * scalar (HOT PATH):");
    println!("  k256:         {:>7.1} us/op", us(k_mul));
    println!("  libsecp256k1: {:>7.1} us/op", us(s_mul));
    println!("  speedup:      {:>7.1}x", ratio(k_mul, s_mul));
    println!();
    println!("Full dlog verify (gen*z + point*e + add):");
    println!("  k256:         {:>7.1} us/op", us(k_dlog));
    println!("  libsecp256k1: {:>7.1} us/op", us(s_dlog));
    println!("  speedup:      {:>7.1}x", ratio(k_dlog, s_dlog));
    println!();

    // Estimate sync impact: at ~37% CPU from k256, what would the new % be?
    let mul_speedup = ratio(k_mul, s_mul);
    let new_pct = 37.0 / mul_speedup;
    let total_speedup = 100.0 / (100.0 - 37.0 + new_pct);
    println!("Estimated sync impact:");
    println!("  Current k256 CPU share:    37%");
    println!("  Projected libsecp share:   {new_pct:.0}%");
    println!("  Overall sync speedup:      {total_speedup:.2}x");
    println!("{}", "=".repeat(60));
}

/// Create a k256 Scalar from raw bytes (big-endian).
fn k256_scalar(bytes: &[u8; 32]) -> k256::Scalar {
    use k256::elliptic_curve::ff::PrimeField;
    let ct = k256::Scalar::from_repr((*bytes).into());
    Option::from(ct).expect("test scalar must be valid (< curve order)")
}
