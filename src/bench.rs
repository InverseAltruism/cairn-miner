//! CPU hashing micro-benchmark: measures the scalar per-nonce path against the
//! N-way interleaved SHA-NI batch path over a fixed nonce range, so the batch
//! speedup on this machine is a single-command, reproducible number.
//!
//! It is intentionally not a criterion harness (no dev-dep, no GPU): a warm-up
//! plus a timed sweep of `--nonces` hashes is enough to compare the two paths
//! on the same header, and it doubles as a smoke check that `hash_batch` and
//! `hash_one` agree bit-for-bit before either is trusted for mining.

use std::time::Instant;

use crate::sha256d_cpu::{shani_available, BatchHasher, BATCH_LANES};

pub struct BenchOpts {
    pub nonces: u32,
}

fn sample_header() -> [u8; 84] {
    // Deterministic non-trivial header; the exact bytes don't matter for timing,
    // only that the midstate/tail are realistic.
    let mut h = [0u8; 84];
    for (i, b) in h.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(37).wrapping_add(11);
    }
    h
}

pub fn run(opts: BenchOpts) -> anyhow::Result<()> {
    let n = opts.nonces.max(1);
    let header = sample_header();
    let hasher = BatchHasher::new(&header);

    println!("=== cairn-miner cpu bench ===");
    println!(
        "sha-ni: {}   lanes: {}   nonces: {}",
        if shani_available() { "yes" } else { "no (portable fallback)" },
        BATCH_LANES,
        n
    );

    // Correctness guard: batch must equal the per-nonce path for a few nonces
    // before we report throughput (a fast-but-wrong hasher is worthless).
    {
        let mut out = [[0u8; 32]; BATCH_LANES];
        hasher.hash_batch::<BATCH_LANES>(0, &mut out);
        for (i, h) in out.iter().enumerate() {
            if *h != hasher.hash_one(i as u32) {
                anyhow::bail!("bench: hash_batch disagrees with hash_one at lane {i}");
            }
        }
        println!("correctness: hash_batch == hash_one  [ok]");
    }

    // Warm up (turbo ramp + i-cache) so the first path isn't penalized.
    let mut sink = 0u8;
    {
        let mut out = [[0u8; 32]; BATCH_LANES];
        for base in (0..(n / 4)).step_by(BATCH_LANES) {
            hasher.hash_batch::<BATCH_LANES>(base, &mut out);
            sink ^= out[0][0];
        }
    }

    // Scalar path.
    let t0 = Instant::now();
    for nonce in 0..n {
        let h = hasher.hash_one(nonce);
        sink ^= h[0];
    }
    let dt_scalar = t0.elapsed().as_secs_f64();
    let mhs_scalar = (n as f64) / 1e6 / dt_scalar;

    // Batched path.
    let t1 = Instant::now();
    let mut out = [[0u8; 32]; BATCH_LANES];
    let batches = n / BATCH_LANES as u32;
    for b in 0..batches {
        hasher.hash_batch::<BATCH_LANES>(b * BATCH_LANES as u32, &mut out);
        sink ^= out[0][0];
    }
    let dt_batch = t1.elapsed().as_secs_f64();
    let mhs_batch = (batches as u64 * BATCH_LANES as u64) as f64 / 1e6 / dt_batch;

    println!("scalar  (hash_one)  : {mhs_scalar:8.1} MH/s  (1 thread)");
    println!("batched (hash_batch): {mhs_batch:8.1} MH/s  (1 thread)");
    println!("speedup             : {:.2}x", mhs_batch / mhs_scalar);
    // Keep `sink` observable so the optimizer can't delete the loops.
    std::hint::black_box(sink);
    Ok(())
}
