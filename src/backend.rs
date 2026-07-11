//! Mining backend trait.
//!
//! A backend takes:
//!   - the 84-byte header skeleton (the merkle slot is already filled),
//!   - a 32-byte big-endian PoW target,
//!   - a half-open nonce range `[start, end)`,
//! and returns — via [`HashOutcome`] — the first nonce whose
//! `sha256d(header_with_nonce) <= target` (or "none in range"), *plus* how many
//! nonces it actually hashed. A device/driver error is a real `Err`, NOT a
//! silent "found nothing": swallowing GPU faults to `None` used to let a dead
//! card mine nothing forever while the process stayed up and the dashboard
//! reported a phantom hashrate. The caller ([`crate::stratum::loop_stratum`])
//! now distinguishes the three cases and exits on repeated device failure so a
//! supervisor (systemd / mine-auto / the HiveOS agent) can restart the rig.
//!
//! Implementations:
//!   - `backends::cpu::CpuBackend` — uses sha2 with rayon-style threading.
//!   - `backends::opencl::OpenclBackend` (feature = "opencl").
//!   - `backends::cuda::CudaBackend` (feature = "cuda").

#[derive(Debug, Clone, Copy)]
pub struct MiningResult {
    pub nonce: u32,
    pub hash: [u8; 32],
}

/// What a backend actually did over the requested nonce range.
#[derive(Debug, Clone, Copy, Default)]
pub struct HashOutcome {
    /// The first solving nonce, if one was found before the range was
    /// exhausted or `stop` fired. `None` = swept without a solution.
    pub result: Option<MiningResult>,
    /// How many nonces the backend ACTUALLY hashed. May be less than
    /// `nonce_end - nonce_start` when `stop` fired or a solution short-circuited
    /// the sweep. Drives honest hashrate accounting — never credit the requested
    /// range, only the work truly done.
    pub nonces_done: u64,
}

impl HashOutcome {
    /// A swept-nothing outcome that hashed `nonces_done` nonces.
    pub fn none(nonces_done: u64) -> Self {
        Self { result: None, nonces_done }
    }
}

pub trait MiningBackend {
    fn name(&self) -> &'static str;

    /// Hash `[nonce_start, nonce_end)`.
    ///
    /// `Ok(HashOutcome)` — swept successfully (with or without a solution).
    /// `Err(_)` — a device/driver/runtime fault; the caller treats repeated
    /// errors as fatal and exits so a supervisor can restart. Implementations
    /// must NOT map faults to `Ok(HashOutcome::none(..))`.
    fn hash_range(
        &self,
        header_84: [u8; 84],
        target: [u8; 32],
        nonce_start: u32,
        nonce_end: u32,
        stop: &std::sync::atomic::AtomicBool,
    ) -> anyhow::Result<HashOutcome>;
}
