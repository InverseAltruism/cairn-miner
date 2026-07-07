//! CPU mining backend.
//!
//! Uses the precomputed 64-byte midstate so the inner loop only runs one
//! SHA-256 compression over the 20-byte tail (merkle_tail | time | bits | nonce)
//! followed by the outer SHA-256 over the 32-byte digest. This matches the GPU
//! kernel exactly and is also the only backend that runs out-of-the-box
//! on every platform — perfect for end-to-end smoke testing.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;

use crate::backend::{MiningBackend, MiningResult};
use crate::sha256d_cpu::{BatchHasher, BATCH_LANES};

pub struct CpuBackend {
    pub threads: usize,
}

impl CpuBackend {
    pub fn new(threads: usize) -> Self {
        Self {
            threads: threads.max(1),
        }
    }
}

#[inline]
fn hash_leq_target(hash: &[u8; 32], target: &[u8; 32]) -> bool {
    // Lexicographic big-endian compare. hash <= target.
    for i in 0..32 {
        if hash[i] < target[i] {
            return true;
        }
        if hash[i] > target[i] {
            return false;
        }
    }
    true
}

impl MiningBackend for CpuBackend {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn hash_range(
        &self,
        header_84: [u8; 84],
        target: [u8; 32],
        nonce_start: u32,
        nonce_end: u32,
        stop: &AtomicBool,
    ) -> Option<MiningResult> {
        if nonce_end <= nonce_start {
            return None;
        }
        // One hasher precomputes the first-block midstate; it is `Copy`, so each
        // worker thread gets its own by value. On a SHA-NI CPU its `hash_batch`
        // interleaves BATCH_LANES independent nonces to keep the SHA unit busy;
        // on other CPUs it falls back to the portable per-nonce path. Either
        // way the output is byte-identical to the scalar reference (pinned by
        // `batch_matches_reference` and end-to-end by `selftest`).
        let hasher = BatchHasher::new(&header_84);

        let next_nonce = AtomicU32::new(nonce_start);
        let found = std::sync::Arc::new(std::sync::Mutex::new(None::<MiningResult>));
        let local_stop = AtomicBool::new(false);

        thread::scope(|scope| {
            for _ in 0..self.threads {
                let hasher = hasher;
                let target = target;
                let next_nonce = &next_nonce;
                let found = found.clone();
                let local_stop = &local_stop;

                scope.spawn(move || {
                    let record = |n: u32, h: [u8; 32]| {
                        let mut g = found.lock().unwrap();
                        if g.is_none() {
                            *g = Some(MiningResult { nonce: n, hash: h });
                        }
                        local_stop.store(true, Ordering::Relaxed);
                    };
                    loop {
                        if stop.load(Ordering::Relaxed) || local_stop.load(Ordering::Relaxed) {
                            return;
                        }
                        // Grab a chunk of nonces so threads don't hammer the
                        // atomic. A multiple of BATCH_LANES so the batch loop
                        // divides evenly except at the final clamp.
                        const CHUNK: u32 = 4096;
                        let start = next_nonce.fetch_add(CHUNK, Ordering::Relaxed);
                        if start >= nonce_end {
                            return;
                        }
                        let end = start.saturating_add(CHUNK).min(nonce_end);
                        let lanes = BATCH_LANES as u32;
                        let mut n = start;
                        // Full interleaved batches.
                        while n + lanes <= end {
                            let mut out = [[0u8; 32]; BATCH_LANES];
                            hasher.hash_batch::<BATCH_LANES>(n, &mut out);
                            for (i, h) in out.iter().enumerate() {
                                if hash_leq_target(h, &target) {
                                    record(n + i as u32, *h);
                                    return;
                                }
                            }
                            n += lanes;
                        }
                        // Remainder (only at the clamped tail of the range).
                        while n < end {
                            let h = hasher.hash_one(n);
                            if hash_leq_target(&h, &target) {
                                record(n, h);
                                return;
                            }
                            n += 1;
                        }
                    }
                });
            }
        });

        let g = found.lock().unwrap();
        *g
    }
}
