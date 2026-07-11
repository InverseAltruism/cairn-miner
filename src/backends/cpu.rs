//! CPU mining backend.
//!
//! Uses the precomputed 64-byte midstate so the inner loop only runs one
//! SHA-256 compression over the 20-byte tail (merkle_tail | time | bits | nonce)
//! followed by the outer SHA-256 over the 32-byte digest. This matches the GPU
//! kernel exactly and is also the only backend that runs out-of-the-box
//! on every platform — perfect for end-to-end smoke testing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use crate::backend::{HashOutcome, MiningBackend, MiningResult};
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
    ) -> anyhow::Result<HashOutcome> {
        if nonce_end <= nonce_start {
            return Ok(HashOutcome::none(0));
        }
        // One hasher precomputes the first-block midstate; it is `Copy`, so each
        // worker thread gets its own by value. On a SHA-NI CPU its `hash_batch`
        // interleaves BATCH_LANES independent nonces to keep the SHA unit busy;
        // on other CPUs it falls back to the portable per-nonce path. Either
        // way the output is byte-identical to the scalar reference (pinned by
        // `batch_matches_reference` and end-to-end by `selftest`).
        let hasher = BatchHasher::new(&header_84);

        // The cursor is u64 so `fetch_add` can step past `nonce_end` (up to
        // u32::MAX) without wrapping the u32 nonce space — a u32 cursor near the
        // top of the range wraps to a low value, the `>= nonce_end` guard never
        // fires, and every thread re-sweeps forever (an infinite hang the miner
        // hits once its sweep reaches the top of the space).
        let next_nonce = AtomicU64::new(nonce_start as u64);
        let found = std::sync::Arc::new(std::sync::Mutex::new(None::<MiningResult>));
        let local_stop = AtomicBool::new(false);
        // Nonces actually hashed across all threads → honest hashrate. The CPU
        // backend can't fault, so it always returns Ok.
        let swept = AtomicU64::new(0);

        thread::scope(|scope| {
            for _ in 0..self.threads {
                let hasher = hasher;
                let target = target;
                let next_nonce = &next_nonce;
                let found = found.clone();
                let local_stop = &local_stop;
                let swept = &swept;

                scope.spawn(move || {
                    let record = |n: u32, h: [u8; 32]| {
                        let mut g = found.lock().unwrap();
                        if g.is_none() {
                            *g = Some(MiningResult { nonce: n, hash: h });
                        }
                        local_stop.store(true, Ordering::Relaxed);
                    };
                    // Count what THIS thread hashes, flushed once on exit so the
                    // per-nonce hot path stays lock-free.
                    let mut local_done: u64 = 0;
                    'sweep: loop {
                        if stop.load(Ordering::Relaxed) || local_stop.load(Ordering::Relaxed) {
                            break 'sweep;
                        }
                        // Grab a chunk of nonces so threads don't hammer the
                        // atomic. A multiple of BATCH_LANES so the batch loop
                        // divides evenly except at the final clamp.
                        const CHUNK: u32 = 4096;
                        let start_u64 = next_nonce.fetch_add(CHUNK as u64, Ordering::Relaxed);
                        if start_u64 >= nonce_end as u64 {
                            break 'sweep;
                        }
                        // start < nonce_end <= u32::MAX, so it fits a u32.
                        let start = start_u64 as u32;
                        let end = start.saturating_add(CHUNK).min(nonce_end);
                        let lanes = BATCH_LANES as u32;
                        let mut n = start;
                        // Full interleaved batches. `end - n` (never `n + lanes`)
                        // so the check can't overflow when end is near u32::MAX.
                        while end - n >= lanes {
                            let mut out = [[0u8; 32]; BATCH_LANES];
                            hasher.hash_batch::<BATCH_LANES>(n, &mut out);
                            local_done += lanes as u64;
                            for (i, h) in out.iter().enumerate() {
                                if hash_leq_target(h, &target) {
                                    record(n + i as u32, *h);
                                    break 'sweep;
                                }
                            }
                            n += lanes;
                        }
                        // Remainder (only at the clamped tail of the range).
                        while n < end {
                            let h = hasher.hash_one(n);
                            local_done += 1;
                            if hash_leq_target(&h, &target) {
                                record(n, h);
                                break 'sweep;
                            }
                            n += 1;
                        }
                    }
                    swept.fetch_add(local_done, Ordering::Relaxed);
                });
            }
        });

        let result = *found.lock().unwrap();
        Ok(HashOutcome {
            result,
            nonces_done: swept.load(Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a range ending at u32::MAX must TERMINATE. With the old u32
    /// cursor (and the old `n + lanes` batch check) the sweep overflowed past
    /// nonce_end, wrapped to low nonces, and re-swept forever. This test would
    /// hang without the u64-cursor / `end - n` fixes.
    #[test]
    fn terminates_at_top_of_nonce_space() {
        let backend = CpuBackend::new(2);
        let header = [0u8; 84];
        // All-zero target: no real sha256d hash is <= 0, so it sweeps the whole
        // range and returns None (never finds), exercising full termination.
        let target = [0u8; 32];
        let stop = AtomicBool::new(false);
        let start = u32::MAX - 10;
        let out = backend
            .hash_range(header, target, start, u32::MAX, &stop)
            .expect("cpu backend never errors");
        assert!(out.result.is_none());
        // Swept exactly [u32::MAX-10, u32::MAX): 10 nonces, no wrap re-sweep.
        assert_eq!(out.nonces_done, 10);
    }

    #[test]
    fn honest_nonces_done_over_a_small_range() {
        let backend = CpuBackend::new(2);
        let out = backend
            .hash_range([0u8; 84], [0u8; 32], 100, 4200, &AtomicBool::new(false))
            .unwrap();
        assert!(out.result.is_none());
        assert_eq!(out.nonces_done, 4100); // [100, 4200)
    }
}
