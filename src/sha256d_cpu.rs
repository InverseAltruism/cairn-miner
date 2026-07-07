//! Reference CPU sha256d, plus a helper that precomputes the SHA-256
//! midstate over the first 64-byte chunk of an 84-byte block header.
//!
//! Used by:
//!   - the CPU backend directly,
//!   - GPU backends to upload the midstate to the device once per template
//!     (avoiding re-hashing the first 64 bytes inside every kernel thread).
//!
//! The CPU backend uses `finish_sha256d_from_midstate_fast` which goes
//! through `sha2::compress256` — on every modern x86 CPU this dispatches
//! to the SHA-NI hardware instructions via the `cpufeatures` crate.
//! `finish_sha256d_from_midstate` (hand-coded) is kept as a verified
//! reference and to mirror the GPU kernel structure exactly.

use sha2::digest::generic_array::GenericArray;
use sha2::digest::typenum::U64;
use sha2::{compress256, Digest, Sha256};

/// Plain reference. Used by the CPU backend and for cross-checking GPU
/// results during development.
#[inline]
pub fn sha256d(buf: &[u8]) -> [u8; 32] {
    let h1 = Sha256::digest(buf);
    let h2 = Sha256::digest(h1);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h2);
    out
}

/// 8-word SHA-256 IV.
pub const SHA256_IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// SHA-256 round constants.
pub const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// One SHA-256 compression function call. `state` is updated in place over
/// a single 64-byte `block`.
pub fn sha256_compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[4 * i],
            block[4 * i + 1],
            block[4 * i + 2],
            block[4 * i + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    let mut f = state[5];
    let mut g = state[6];
    let mut h = state[7];
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// Compute the SHA-256 midstate after consuming the FIRST 64-byte chunk
/// of an 84-byte block header. The remaining 20 bytes (merkle_tail(4) | time(8) |
/// bits(4) | nonce(4)) are fed to the GPU kernel along with this midstate; the
/// kernel finishes the inner hash, then runs the outer SHA-256 over the 32-byte
/// digest.
pub fn midstate_of_first_chunk(header_84: &[u8; 84]) -> [u32; 8] {
    let mut state = SHA256_IV;
    let mut chunk = [0u8; 64];
    chunk.copy_from_slice(&header_84[..64]);
    sha256_compress(&mut state, &chunk);
    state
}

/// CPU finisher: given the midstate of the first 64 bytes of an 84-byte
/// header and the final 20 bytes (`tail`), compute sha256d. Used by the CPU
/// backend so its loop matches the GPU's exactly.
pub fn finish_sha256d_from_midstate(midstate: &[u32; 8], tail: &[u8; 20]) -> [u8; 32] {
    // Second SHA-256 chunk = 20 message bytes + 1 (0x80) + zero padding +
    // 8 bytes length (in bits). Total message length = 84 bytes = 672 bits.
    let mut block = [0u8; 64];
    block[..20].copy_from_slice(tail);
    block[20] = 0x80;
    // Length in bits = 672 = 0x2A0; place as big-endian u64 in last 8 bytes.
    let bitlen: u64 = 84 * 8;
    let len_be = bitlen.to_be_bytes();
    block[56..64].copy_from_slice(&len_be);

    let mut state = *midstate;
    sha256_compress(&mut state, &block);

    // Now hash that 32-byte digest with a fresh SHA-256.
    let mut inner_digest = [0u8; 32];
    for i in 0..8 {
        inner_digest[4 * i..4 * i + 4].copy_from_slice(&state[i].to_be_bytes());
    }
    let mut outer = SHA256_IV;
    let mut outer_block = [0u8; 64];
    outer_block[..32].copy_from_slice(&inner_digest);
    outer_block[32] = 0x80;
    let outer_bitlen: u64 = 32 * 8;
    outer_block[56..64].copy_from_slice(&outer_bitlen.to_be_bytes());
    sha256_compress(&mut outer, &outer_block);

    let mut out = [0u8; 32];
    for i in 0..8 {
        out[4 * i..4 * i + 4].copy_from_slice(&outer[i].to_be_bytes());
    }
    out
}

/// SHA-NI-accelerated CPU finisher. Same contract as
/// `finish_sha256d_from_midstate` but uses `sha2::compress256` which the
/// cpufeatures crate dispatches to SHA-NI on modern x86_64. Output is
/// byte-identical.
#[inline]
pub fn finish_sha256d_from_midstate_fast(midstate: &[u32; 8], tail: &[u8; 20]) -> [u8; 32] {
    // Second SHA-256 chunk of the inner hash: 20 message bytes + 0x80 +
    // zero padding + 64-bit BE bitlen (84*8 = 672).
    let mut block_buf = [0u8; 64];
    block_buf[..20].copy_from_slice(tail);
    block_buf[20] = 0x80;
    let bitlen: u64 = 84 * 8;
    block_buf[56..64].copy_from_slice(&bitlen.to_be_bytes());

    let mut state = *midstate;
    let block = GenericArray::<u8, U64>::clone_from_slice(&block_buf);
    compress256(&mut state, core::slice::from_ref(&block));

    // Outer hash over the 32-byte inner digest.
    let mut outer_block = [0u8; 64];
    for i in 0..8 {
        outer_block[4 * i..4 * i + 4].copy_from_slice(&state[i].to_be_bytes());
    }
    outer_block[32] = 0x80;
    let outer_bitlen: u64 = 32 * 8;
    outer_block[56..64].copy_from_slice(&outer_bitlen.to_be_bytes());

    let mut outer_state = SHA256_IV;
    let outer = GenericArray::<u8, U64>::clone_from_slice(&outer_block);
    compress256(&mut outer_state, core::slice::from_ref(&outer));

    let mut out = [0u8; 32];
    for i in 0..8 {
        out[4 * i..4 * i + 4].copy_from_slice(&outer_state[i].to_be_bytes());
    }
    out
}

/// Same dispatch path as the per-nonce finisher but for the initial
/// midstate. The kernel uploads this midstate verbatim, so it must match
/// the hand-coded version byte-for-byte; tests below assert it.
#[inline]
pub fn midstate_of_first_chunk_fast(header_84: &[u8; 84]) -> [u32; 8] {
    let mut state = SHA256_IV;
    let block = GenericArray::<u8, U64>::clone_from_slice(&header_84[..64]);
    compress256(&mut state, core::slice::from_ref(&block));
    state
}

// ===========================================================================
// N-way interleaved SHA-NI batch hasher.
//
// One scalar sha256d chain leaves the SHA-NI execution unit mostly idle:
// `sha256rnds2` has multi-cycle latency but ~1-2 cycle inverse throughput,
// so a single dependency chain (32 sequential rnds2 per compression) stalls
// the pipe most of the time. Hashing N independent nonces at once — each in
// its own XMM state, interleaved at 4-round granularity — overlaps those
// chains and roughly doubles per-thread throughput on Alder Lake.
//
// The lane count actually used by the CPU backend is `BATCH_LANES`, picked
// by measurement on an i5-12500 via `cairn-miner bench` (see src/bench.rs).
// Correctness of every lane count is pinned by the `batch_matches_reference`
// test below and, end-to-end, by `cairn-miner selftest`.
// ===========================================================================

/// Lane count the CPU backend hashes per batch. Benchmarked fastest on
/// SHA-NI Alder Lake (see `cairn-miner bench`); larger counts spill XMM
/// registers, smaller ones leave the SHA unit idle between rounds.
pub const BATCH_LANES: usize = 4;

/// Returns true when this CPU can run the interleaved SHA-NI batch path.
/// Cached by std's feature-detection machinery; cheap to call repeatedly.
#[inline]
pub fn shani_available() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        std::arch::is_x86_feature_detected!("sha")
            && std::arch::is_x86_feature_detected!("sse4.1")
            && std::arch::is_x86_feature_detected!("ssse3")
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        false
    }
}

/// Per-template batch hasher: precomputes the first-block midstate once and
/// then hashes consecutive nonces in N-way interleaved SHA-NI batches
/// (portable sha2 fallback on CPUs without SHA-NI). Plain data — `Copy`, so
/// each worker thread gets its own by value.
#[derive(Clone, Copy)]
pub struct BatchHasher {
    midstate: [u32; 8],
    tail16: [u8; 16],
    use_shani: bool,
}

impl BatchHasher {
    pub fn new(header_84: &[u8; 84]) -> Self {
        let mut tail16 = [0u8; 16];
        tail16.copy_from_slice(&header_84[64..80]);
        Self {
            midstate: midstate_of_first_chunk_fast(header_84),
            tail16,
            use_shani: shani_available(),
        }
    }

    /// sha256d of the header with `nonce` in bytes 80..84. Portable path
    /// (sha2 crate, itself SHA-NI-dispatched); used for batch remainders
    /// and as the non-x86 / no-SHA-NI fallback.
    #[inline]
    pub fn hash_one(&self, nonce: u32) -> [u8; 32] {
        let mut tail = [0u8; 20];
        tail[..16].copy_from_slice(&self.tail16);
        tail[16..].copy_from_slice(&nonce.to_le_bytes());
        finish_sha256d_from_midstate_fast(&self.midstate, &tail)
    }

    /// sha256d for the N consecutive nonces `base_nonce..base_nonce+N`
    /// (wrapping): `out[i]` = hash of nonce `base_nonce + i`.
    #[inline]
    pub fn hash_batch<const N: usize>(&self, base_nonce: u32, out: &mut [[u8; 32]; N]) {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        if self.use_shani {
            // SAFETY: `use_shani` was set from runtime CPU-feature detection
            // of exactly the features `sha256d_batch` enables.
            unsafe { shani::sha256d_batch::<N>(&self.midstate, &self.tail16, base_nonce, out) };
            return;
        }
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.hash_one(base_nonce.wrapping_add(i as u32));
        }
    }

    /// Force the portable fallback path (test/bench hook).
    #[cfg(any(test, feature = "cpu-only", feature = "cuda", feature = "opencl"))]
    pub fn with_shani(mut self, enabled: bool) -> Self {
        self.use_shani = self.use_shani && enabled;
        self
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod shani {
    //! Raw SHA-NI intrinsics, interleaved N ways. Same canonical Intel
    //! shuffle/round structure as sha2's x86 backend (which the tests
    //! cross-check us against), extended to N independent states.

    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;

    use super::{SHA256_IV, SHA256_K};

    /// Shuffle a plain-word SHA-256 state into the (ABEF, CDGH) register
    /// layout `sha256rnds2` expects.
    #[inline(always)]
    unsafe fn load_state(state: &[u32; 8]) -> (__m128i, __m128i) {
        let dcba = _mm_loadu_si128(state.as_ptr() as *const __m128i);
        let efgh = _mm_loadu_si128(state.as_ptr().add(4) as *const __m128i);
        let cdab = _mm_shuffle_epi32(dcba, 0xB1);
        let efgh = _mm_shuffle_epi32(efgh, 0x1B);
        let abef = _mm_alignr_epi8(cdab, efgh, 8);
        let cdgh = _mm_blend_epi16(efgh, cdab, 0xF0);
        (abef, cdgh)
    }

    /// Inverse of `load_state`: returns (abcd, efgh) vectors whose element
    /// `j` is state word `j` / `4+j` — directly usable as the first two
    /// message quads of the outer hash.
    #[inline(always)]
    unsafe fn unshuffle_state(abef: __m128i, cdgh: __m128i) -> (__m128i, __m128i) {
        let feba = _mm_shuffle_epi32(abef, 0x1B);
        let dchg = _mm_shuffle_epi32(cdgh, 0xB1);
        let abcd = _mm_blend_epi16(feba, dchg, 0xF0);
        let efgh = _mm_alignr_epi8(dchg, feba, 8);
        (abcd, efgh)
    }

    /// Message-schedule step: given quads W[q..q+4), W[q+4..q+8),
    /// W[q+8..q+12), W[q+12..q+16), produce W[q+16..q+20).
    #[inline(always)]
    unsafe fn schedule(m0: __m128i, m1: __m128i, m2: __m128i, m3: __m128i) -> __m128i {
        let t = _mm_sha256msg1_epu32(m0, m1);
        let t = _mm_add_epi32(t, _mm_alignr_epi8(m3, m2, 4));
        _mm_sha256msg2_epu32(t, m3)
    }

    /// One SHA-256 compression over N independent (state, message) lanes,
    /// interleaved at 4-round granularity: the `for l in 0..N` bodies are
    /// unrolled (N is const) so the N `sha256rnds2` dependency chains issue
    /// back-to-back and hide each other's latency.
    ///
    /// `m[l]` is a 4-quad ring: slot `q % 4` holds message quad `q` when
    /// round-quad `q` executes, and is overwritten with quad `q + 4` for the
    /// later rounds.
    #[inline(always)]
    unsafe fn compress_n<const N: usize>(
        abef: &mut [__m128i; N],
        cdgh: &mut [__m128i; N],
        m: &mut [[__m128i; 4]; N],
    ) {
        let save_abef = *abef;
        let save_cdgh = *cdgh;
        for q in 0..16 {
            let k = _mm_loadu_si128(SHA256_K.as_ptr().add(4 * q) as *const __m128i);
            for l in 0..N {
                let wk = _mm_add_epi32(m[l][q % 4], k);
                cdgh[l] = _mm_sha256rnds2_epu32(cdgh[l], abef[l], wk);
                abef[l] = _mm_sha256rnds2_epu32(abef[l], cdgh[l], _mm_shuffle_epi32(wk, 0x0E));
                if q < 12 {
                    m[l][q % 4] = schedule(
                        m[l][q % 4],
                        m[l][(q + 1) % 4],
                        m[l][(q + 2) % 4],
                        m[l][(q + 3) % 4],
                    );
                }
            }
        }
        for l in 0..N {
            abef[l] = _mm_add_epi32(abef[l], save_abef[l]);
            cdgh[l] = _mm_add_epi32(cdgh[l], save_cdgh[l]);
        }
    }

    /// sha256d over N consecutive nonces from the precomputed midstate.
    /// Message blocks are built as BE-packed words directly (no per-nonce
    /// byte buffers), mirroring the GPU kernels:
    ///   inner block: tail16 words | bswap(nonce) | 0x80000000 | 0.. | 672
    ///   outer block: inner digest words | 0x80000000 | 0.. | 256
    #[allow(clippy::cast_ptr_alignment)] // unaligned loads/stores via loadu/storeu
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    pub(super) unsafe fn sha256d_batch<const N: usize>(
        midstate: &[u32; 8],
        tail16: &[u8; 16],
        base_nonce: u32,
        out: &mut [[u8; 32]; N],
    ) {
        // Per-32-bit-word byte reversal (BE pack of LE bytes and vice versa).
        let bswap = _mm_setr_epi8(3, 2, 1, 0, 7, 6, 5, 4, 11, 10, 9, 8, 15, 14, 13, 12);
        let tail_quad =
            _mm_shuffle_epi8(_mm_loadu_si128(tail16.as_ptr() as *const __m128i), bswap);

        let (mid_abef, mid_cdgh) = load_state(midstate);
        let mut abef = [mid_abef; N];
        let mut cdgh = [mid_cdgh; N];

        // Inner hash: second 64-byte block of the 84-byte header.
        let zero = _mm_setzero_si128();
        let inner_len = _mm_setr_epi32(0, 0, 0, 672); // W[15] = 84 * 8 bits
        let mut m = [[zero; 4]; N];
        for l in 0..N {
            let nonce = base_nonce.wrapping_add(l as u32);
            m[l] = [
                tail_quad,
                // W[4] = LE nonce bytes packed BE = bswap; W[5] = 0x80 pad.
                _mm_setr_epi32(nonce.swap_bytes() as i32, 0x80000000u32 as i32, 0, 0),
                zero,
                inner_len,
            ];
        }
        compress_n(&mut abef, &mut cdgh, &mut m);

        // Outer hash over each 32-byte inner digest.
        let outer_pad = _mm_setr_epi32(0x80000000u32 as i32, 0, 0, 0); // W[8]
        let outer_len = _mm_setr_epi32(0, 0, 0, 256); // W[15] = 32 * 8 bits
        for l in 0..N {
            let (abcd, efgh) = unshuffle_state(abef[l], cdgh[l]);
            m[l] = [abcd, efgh, outer_pad, outer_len];
        }
        let (iv_abef, iv_cdgh) = load_state(&SHA256_IV);
        for l in 0..N {
            abef[l] = iv_abef;
            cdgh[l] = iv_cdgh;
        }
        compress_n(&mut abef, &mut cdgh, &mut m);

        for l in 0..N {
            let (abcd, efgh) = unshuffle_state(abef[l], cdgh[l]);
            _mm_storeu_si128(
                out[l].as_mut_ptr() as *mut __m128i,
                _mm_shuffle_epi8(abcd, bswap),
            );
            _mm_storeu_si128(
                out[l].as_mut_ptr().add(16) as *mut __m128i,
                _mm_shuffle_epi8(efgh, bswap),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midstate_finish_matches_reference() {
        let header = [0xCDu8; 84];
        let reference = sha256d(&header);

        let mid = midstate_of_first_chunk(&header);
        let mut tail = [0u8; 20];
        tail.copy_from_slice(&header[64..]);
        let viamid = finish_sha256d_from_midstate(&mid, &tail);

        assert_eq!(reference, viamid);
    }

    #[test]
    fn empty_sha256d_matches_known_vector() {
        // Sanity check: sha256d("") for cross-version stability.
        let h = sha256d(b"");
        assert_eq!(
            hex::encode(h),
            "5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456"
        );
    }

    #[test]
    fn sha2_fast_path_matches_hand_coded() {
        // Identical output across the two CPU implementations + the
        // canonical sha256d on the full 84-byte buffer.
        for seed in [0u8, 7, 0x55, 0xFF] {
            let header = [seed; 84];
            let mid_hand = midstate_of_first_chunk(&header);
            let mid_sha2 = midstate_of_first_chunk_fast(&header);
            assert_eq!(mid_hand, mid_sha2, "midstate mismatch seed={}", seed);

            let mut tail = [0u8; 20];
            tail.copy_from_slice(&header[64..]);

            let hand = finish_sha256d_from_midstate(&mid_hand, &tail);
            let fast = finish_sha256d_from_midstate_fast(&mid_sha2, &tail);
            let canonical = sha256d(&header);
            assert_eq!(hand, fast, "finisher mismatch seed={}", seed);
            assert_eq!(fast, canonical, "fast finisher vs canonical seed={}", seed);
        }
    }
}
