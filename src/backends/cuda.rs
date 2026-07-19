//! CUDA mining backend — 2-stream pipelined.
//!
//! A pre-built PTX (`sha256d.ptx`, compiled offline from `sha256d.cu`) is loaded
//! via the driver's JIT (`cuModuleLoadData`) at startup — NVRTC is never invoked,
//! so only the NVIDIA driver is needed at runtime (no CUDA Toolkit). Two CUDA
//! streams alternate
//! kernel launches; while one stream is hashing the host reads back the
//! other stream's prior flag and decides whether to stop. The kernel
//! itself sweeps `nonces_per_thread` nonces per work-item.
//!
//! Default geometry on a modern desktop GPU:
//!   blocks=560 threads_per_block=256 nonces_per_thread=4096
//!   → 587 M nonces per launch
//!   → 2 launches blanket nearly all 4.29 G u32 nonces
//!
//! cudarc 0.19 API (CudaContext + per-stream ops). Was cudarc 0.11
//! (CudaDevice + device-level ops); 0.19 supports CUDA 13.x natively
//! which fixes the PTX_UNSUPPORTED error the old version hit against
//! the user's CUDA 13.1 driver / 13.2 toolkit.
//!
//! iter-hotpath #2: streams + device allocations live on `CudaBackend`
//! itself, set up once in `new()`. `hash_range` only memcpys the
//! per-launch inputs (midstate, tail_16, target, zeroed found_flag)
//! into the persistent buffers — no `new_stream` / `alloc_zeros` /
//! `clone_htod` thrashing on every template refresh.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use cudarc::driver::{
    CudaContext, CudaFunction, CudaModule, CudaSlice, CudaStream, LaunchConfig, PushKernelArg,
};
use cudarc::nvrtc::Ptx;

use crate::backend::{HashOutcome, MiningBackend, MiningResult};
use crate::sha256d_cpu::midstate_of_first_chunk_fast as midstate_of_first_chunk;

// CUDA kernel, pre-compiled to PTX *offline* (nvcc -ptx -arch=compute_75
// -maxrregcount=64 --use_fast_math; see scripts/build-ptx). We embed the PTX and
// let the NVIDIA driver JIT it to SASS at module-load time, so the CUDA backend
// needs ONLY the NVIDIA driver at runtime — NOT the CUDA Toolkit / nvrtc shared
// library. The PTX `.version` is pinned LOW (6.3, the sm_75 floor): a newer
// toolkit stamps a too-new ISA that older drivers reject with
// CUDA_ERROR_UNSUPPORTED_PTX_VERSION (→ silent CPU fallback). The kernel uses only
// old integer ops, so 6.3 is valid and loads on any CUDA-10.0+ driver.
// Regenerate sha256d.ptx with scripts/build-ptx after editing the .cu.
const KERNEL_PTX: &str = include_str!("../kernels/sha256d.ptx");
const KERNEL_NAME: &str = "mine_sha256d";

pub struct CudaBackend {
    // iter-hotpath #2: `ctx` and `module` are owned here ONLY to keep
    // the underlying CUDA resources alive for the lifetime of the
    // backend. After the refactor we no longer touch them from the hot
    // path (the streams + function handle inside `pipes` hold their own
    // Arc references), but dropping the owning Arc would invalidate the
    // device buffers / loaded module. `#[allow(dead_code)]` documents
    // that this is a deliberate ownership root, not a leftover field.
    #[allow(dead_code)]
    ctx: Arc<CudaContext>,
    #[allow(dead_code)]
    module: Arc<CudaModule>,
    pub blocks: u32,
    pub threads_per_block: u32,
    pub nonces_per_thread: u32,

    // iter-hotpath #2: persistent per-pipe state. Set up exactly once
    // in `new()` then reused for every `hash_range` invocation. The
    // Mutex enforces single-threaded access from `hash_range`
    // (`MiningBackend::hash_range` takes `&self`, so we need interior
    // mutability for the device buffers; in practice each backend is
    // owned by one mining thread so the lock is uncontended).
    pipes: Mutex<PipePair>,
}

/// Two pipes (A + B) plus the per-launch function handle. All members
/// outlive a single `hash_range` call — they are torn down only when
/// the backend is dropped.
struct PipePair {
    a: PipeRes,
    b: PipeRes,
    func: CudaFunction,
}

struct PipeRes {
    stream: Arc<CudaStream>,
    mid_dev: CudaSlice<u32>,
    tail_dev: CudaSlice<u8>,
    target_dev: CudaSlice<u8>,
    found_nonce: CudaSlice<u32>,
    found_flag: CudaSlice<u32>,
    found_hash: CudaSlice<u32>,
    in_flight: bool,
}

impl CudaBackend {
    pub fn new(
        device_index: usize,
        blocks: u32,
        threads_per_block: u32,
        nonces_per_thread: u32,
    ) -> Result<Self> {
        let ctx = CudaContext::new(device_index)
            .map_err(|e| anyhow!("cuda: open device {} failed: {}", device_index, e))?;
        let name = ctx.name().unwrap_or_else(|_| "<unknown>".into());
        tracing::info!("cuda device: {}", name);

        // iter-hotpath #1: switch the CUDA context from spin-wait
        // (default CU_CTX_SCHED_AUTO -> spin on Windows) to blocking
        // sync. Every `stream.synchronize()` call in `drain_pipe` was
        // pinning one CPU core 100% in a kernel-level spin loop while
        // we waited for the GPU to finish a launch. With two streams
        // alternating that's a whole physical core lost to the
        // bookkeeping thread — CPU mining gets ~7% of its potential
        // because of this. Blocking-sync trades 1-10 us of extra
        // kernel-mode latency for ~1 freed physical core. Worth it.
        ctx.set_blocking_synchronize()
            .map_err(|e| anyhow!("cuda: set_blocking_synchronize failed: {}", e))?;

        // Load the pre-compiled PTX (embedded as `KERNEL_PTX`) and hand it to the
        // driver's built-in PTX->SASS JIT. The PTX targets the compute_75 (Turing)
        // virtual arch, so the driver forward-JITs it onto EVERY CUDA-13-supported
        // NVIDIA GPU (Turing through Blackwell) — this one public binary works
        // across miners' cards. Crucially, loading pre-built PTX uses ONLY the
        // driver (cuModuleLoadData); it never touches NVRTC, so no CUDA Toolkit /
        // nvrtc shared library is needed at runtime. (The previous code
        // NVRTC-compiled the kernel at startup and silently fell back to CPU on
        // any miner without nvrtc.dll / libnvrtc — the reported "full CPU load" bug.)
        let ptx = Ptx::from_src(KERNEL_PTX);
        let module = ctx
            .load_module(ptx)
            .map_err(|e| anyhow!("cuda: load_module failed: {}", e))?;

        // iter-hotpath #2: build the two pipes (streams + device
        // buffers) ONCE here, then reuse for the lifetime of the
        // backend. Previously this work happened inside every
        // hash_range call — 2 stream creations + 8 device allocations
        // + 6 host->device copies every ~1-2s template refresh. The
        // CUDA driver-level cost of those calls (alloc handlers,
        // stream-create syscalls) shows up as ~14 driver calls per
        // template on the host side and as alloc fragmentation on the
        // device side over long runs.
        let a = build_pipe(&ctx).map_err(|e| anyhow!("cuda: pipe A setup failed: {}", e))?;
        let b = build_pipe(&ctx).map_err(|e| anyhow!("cuda: pipe B setup failed: {}", e))?;
        let func: CudaFunction = module
            .load_function(KERNEL_NAME)
            .map_err(|e| anyhow!("cuda: load_function {} failed: {}", KERNEL_NAME, e))?;

        Ok(Self {
            ctx,
            module,
            blocks: blocks.max(1).min(65535),
            threads_per_block: threads_per_block.max(64).min(1024),
            nonces_per_thread: nonces_per_thread.max(1),
            pipes: Mutex::new(PipePair { a, b, func }),
        })
    }

    fn nonces_per_launch(&self) -> u64 {
        self.blocks as u64 * self.threads_per_block as u64 * self.nonces_per_thread as u64
    }
}

/// Allocate the persistent buffers for ONE pipe. Buffer sizes are
/// fixed by the kernel ABI: midstate is 8 u32s, tail_16 is 16 bytes,
/// target is 32 bytes, found_flag/found_nonce are 1 u32 each, found_hash
/// is 8 u32s. Buffers are zero-initialised here; `hash_range` overwrites
/// them with the per-launch input on every invocation.
fn build_pipe(ctx: &Arc<CudaContext>) -> Result<PipeRes, cudarc::driver::DriverError> {
    let stream = ctx.new_stream()?;
    let mid_dev: CudaSlice<u32> = stream.alloc_zeros::<u32>(8)?;
    let tail_dev: CudaSlice<u8> = stream.alloc_zeros::<u8>(16)?;
    let target_dev: CudaSlice<u8> = stream.alloc_zeros::<u8>(32)?;
    let found_nonce: CudaSlice<u32> = stream.alloc_zeros::<u32>(1)?;
    let found_flag: CudaSlice<u32> = stream.alloc_zeros::<u32>(1)?;
    let found_hash: CudaSlice<u32> = stream.alloc_zeros::<u32>(8)?;
    Ok(PipeRes {
        stream,
        mid_dev,
        tail_dev,
        target_dev,
        found_nonce,
        found_flag,
        found_hash,
        in_flight: false,
    })
}

/// Re-prime ONE persistent pipe with the current call's per-launch
/// inputs and clear the found-flag + in_flight state ready for a fresh
/// launch sequence. Cheap path: 3 small H2D memcpys, no allocations.
fn prime_pipe(
    pipe: &mut PipeRes,
    midstate: &[u32; 8],
    tail_16: &[u8; 16],
    target: &[u8; 32],
) -> Result<(), cudarc::driver::DriverError> {
    pipe.stream.memcpy_htod(midstate.as_slice(), &mut pipe.mid_dev)?;
    pipe.stream.memcpy_htod(tail_16.as_slice(), &mut pipe.tail_dev)?;
    pipe.stream.memcpy_htod(target.as_slice(), &mut pipe.target_dev)?;
    // found_flag is zeroed at the top of each launch inside the inner
    // loop anyway, but reset here too so a stale "found from previous
    // hash_range" flag can never leak in if a previous call returned
    // mid-pipeline.
    let zeros = [0u32];
    pipe.stream.memcpy_htod(&zeros, &mut pipe.found_flag)?;
    pipe.in_flight = false;
    Ok(())
}

impl MiningBackend for CudaBackend {
    fn name(&self) -> &'static str {
        "cuda"
    }

    fn hash_range(
        &self,
        header_84: [u8; 84],
        target: [u8; 32],
        nonce_start: u32,
        nonce_end: u32,
        stop: &AtomicBool,
    ) -> Result<HashOutcome> {
        if nonce_end <= nonce_start {
            return Ok(HashOutcome::none(0));
        }

        let midstate = midstate_of_first_chunk(&header_84);
        let mut tail_16 = [0u8; 16];
        tail_16.copy_from_slice(&header_84[64..80]);

        // iter-hotpath #2: borrow the persistent pipes for this call
        // instead of rebuilding them. The mutex is uncontended in
        // practice — each backend is owned by one mining thread. A
        // poisoned mutex means a prior launch panicked → surface it as an
        // error so the caller can restart, never as "found nothing".
        let mut pipes = self
            .pipes
            .lock()
            .map_err(|e| anyhow!("cuda pipes mutex poisoned: {e}"))?;

        // Re-prime BOTH pipes with the current header's midstate, tail
        // and target. This is the per-launch hot path that replaces
        // the old setup_pipe() — 6 small H2D memcpys (3 per pipe) and
        // zero allocations.
        prime_pipe(&mut pipes.a, &midstate, &tail_16, &target)
            .map_err(|e| anyhow!("cuda prime pipe A: {e}"))?;
        prime_pipe(&mut pipes.b, &midstate, &tail_16, &target)
            .map_err(|e| anyhow!("cuda prime pipe B: {e}"))?;

        let cfg = LaunchConfig {
            grid_dim: (self.blocks, 1, 1),
            block_dim: (self.threads_per_block, 1, 1),
            shared_mem_bytes: 0,
        };

        let nonces_per_launch = self.nonces_per_launch();
        let mut next_start: u64 = nonce_start as u64;
        let mut current_pipe = 0u8;
        // Nonces launched so far → honest hashrate on every exit path.
        let done = |next: u64| next.saturating_sub(nonce_start as u64);

        // Destructure once so the borrow checker treats a / b / func as
        // disjoint fields (otherwise selecting `&mut pipes.a` inside
        // the loop locks the whole `pipes` binding and conflicts with
        // `&pipes.func` for `launch_builder`).
        let PipePair { a, b, func } = &mut *pipes;

        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(HashOutcome::none(done(next_start)));
            }

            // Drain the current pipe if in flight. Reset `in_flight` BEFORE
            // using the result: the old code returned with the flag left true,
            // so this pipe's found buffer (found_flag=1, found_nonce=X) leaked
            // into the next hash_range call, which re-drained it and re-submitted
            // the SAME nonce - the duplicate-share bug. It only surfaced at low
            // difficulty, where nearly every chunk finds and so the reset path
            // (drain -> None) never ran; fast cards at high diff cleared it every
            // launch and never saw it. The OpenCL backend already does this.
            // A driver/copy fault here is a real error (`?`), not a silent
            // "no solution".
            let drain_result = {
                let pipe: &mut PipeRes = if current_pipe == 0 { &mut *a } else { &mut *b };
                if pipe.in_flight {
                    let res = drain_pipe(pipe)?;
                    pipe.in_flight = false;
                    res
                } else {
                    None
                }
            };
            if let Some(res) = drain_result {
                // Drain the sibling too so its abandoned in-flight launch can't
                // leak into the next call either. Do NOT `?` here: we already
                // hold a found share, and a sibling fault will resurface on its
                // own next launch/drain; losing the share now helps nobody.
                let other: &mut PipeRes = if current_pipe == 0 { &mut *b } else { &mut *a };
                if other.in_flight {
                    let _ = drain_pipe(other);
                    other.in_flight = false;
                }
                return Ok(HashOutcome {
                    result: Some(res),
                    nonces_done: done(next_start),
                });
            }

            if next_start < nonce_end as u64 {
                let pipe: &mut PipeRes = if current_pipe == 0 { &mut *a } else { &mut *b };
                let launch_size = nonces_per_launch.min(nonce_end as u64 - next_start);
                if launch_size > 0 {
                    let start_u32 = next_start as u32;
                    let zeros = [0u32];
                    pipe.stream
                        .memcpy_htod(&zeros, &mut pipe.found_flag)
                        .map_err(|e| anyhow!("cuda reset found_flag: {e}"))?;
                    let end_u32 = nonce_end;

                    let mut builder = pipe.stream.launch_builder(func);
                    builder.arg(&pipe.mid_dev);
                    builder.arg(&pipe.tail_dev);
                    builder.arg(&pipe.target_dev);
                    builder.arg(&start_u32);
                    builder.arg(&end_u32);
                    builder.arg(&self.nonces_per_thread);
                    builder.arg(&mut pipe.found_nonce);
                    builder.arg(&mut pipe.found_flag);
                    builder.arg(&mut pipe.found_hash);
                    unsafe { builder.launch(cfg) }
                        .map_err(|e| anyhow!("cuda kernel launch: {e}"))?;
                    pipe.in_flight = true;
                    next_start = next_start.saturating_add(launch_size);
                }
            } else if !a.in_flight && !b.in_flight {
                return Ok(HashOutcome::none(done(next_start)));
            }

            current_pipe ^= 1;
        }
    }
}

/// Synchronize a launched pipe and read back any solution.
/// `Ok(Some)` = found, `Ok(None)` = launched but no solution, `Err` = a
/// device/copy fault (the caller treats repeated faults as fatal + restarts).
fn drain_pipe(pipe: &mut PipeRes) -> Result<Option<MiningResult>> {
    pipe.stream
        .synchronize()
        .map_err(|e| anyhow!("cuda synchronize: {e}"))?;
    let flag_host: Vec<u32> = pipe
        .stream
        .clone_dtoh(&pipe.found_flag)
        .map_err(|e| anyhow!("cuda read found_flag: {e}"))?;
    if flag_host[0] == 0 {
        return Ok(None);
    }
    let nonce_host: Vec<u32> = pipe
        .stream
        .clone_dtoh(&pipe.found_nonce)
        .map_err(|e| anyhow!("cuda read found_nonce: {e}"))?;
    let hash_host: Vec<u32> = pipe
        .stream
        .clone_dtoh(&pipe.found_hash)
        .map_err(|e| anyhow!("cuda read found_hash: {e}"))?;
    let mut hash = [0u8; 32];
    for i in 0..8 {
        let be = hash_host[i].to_be_bytes();
        hash[4 * i..4 * i + 4].copy_from_slice(&be);
    }
    Ok(Some(MiningResult {
        nonce: nonce_host[0],
        hash,
    }))
}
