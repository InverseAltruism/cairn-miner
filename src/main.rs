//! cairn-miner CLI.
//!
//! The public build mines to the CSD pool by default: it connects to the
//! compiled-in pool endpoint (see [`cairn_miner::endpoint`]) over Stratum v1.
//! There is intentionally **no** node/pool override flag — the only required
//! argument is `--address`, your addr20 payout address.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{bail, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

use cairn_miner::backends::cpu::CpuBackend;
use cairn_miner::endpoint;
use cairn_miner::logging;
use cairn_miner::mining_config::MiningConfig;
use cairn_miner::stratum::{run_stratum, StratumClient};

#[cfg(feature = "opencl")]
use cairn_miner::backends::opencl::OpenclBackend;

#[cfg(feature = "cuda")]
use cairn_miner::backends::cuda::CudaBackend;

mod config_file;
mod keygen;

#[derive(Parser, Debug)]
#[command(
    name = "cairn-miner",
    version,
    about = "Open GPU/CPU pool miner for Compute Substrate (CSD). Defaults to the cairn pool; --pool points it anywhere."
)]
struct Cli {
    /// Path to a TOML config file. If omitted, `./config.toml` then the platform
    /// config dir (`~/.config/cairn-miner/config.toml`, or on Windows
    /// `%APPDATA%\cairn-miner\config.toml`) are tried. Explicit CLI flags
    /// always override the config file. See `config.example.toml`.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Stratum pool endpoint(s) as `host:port`. Repeat the flag for failover
    /// backups: the miner starts on the first endpoint and walks down the list
    /// when a pool stays unreachable. Defaults to the public cairn pool.
    #[arg(long = "pool")]
    pool: Vec<String>,

    /// Your addr20 payout address (the pool credits shares to this address).
    /// 40 lowercase hex chars, optionally `0x`-prefixed (42). Provide it here or
    /// as `address =` in the config file; this flag wins if both are set.
    #[arg(long)]
    address: Option<String>,

    /// Worker/rig name. The pool authorizes as `<address>.<worker>` so per-rig
    /// stats show up separately on the dashboard; payouts still go to the bare
    /// address. Defaults to this machine's hostname when unset.
    #[arg(long)]
    worker: Option<String>,

    /// Backend to use.
    #[arg(long, default_value = "auto")]
    backend: BackendChoice,

    /// Total CPU threads to use for hashing in the CPU backend (or
    /// fallback). Defaults to all logical cores minus `--reserve`.
    #[arg(long)]
    threads: Option<usize>,

    /// CPU threads to leave free for the OS + node + dashboard + the
    /// miner's own I/O. The CPU backend will use `available - reserve`
    /// (clamped to >= 1). Ignored when a GPU backend is active — the
    /// GPU is doing the hashing and CPU threads here only handle I/O.
    #[arg(long, default_value_t = 4)]
    reserve: usize,

    /// GPU launch geometry: blocks per kernel launch.
    #[arg(long, default_value_t = 560)]
    blocks: u32,

    /// GPU launch geometry: threads per block.
    #[arg(long, default_value_t = 256)]
    threads_per_block: u32,

    /// GPU kernel inner loop: nonces tried per thread per launch.
    /// Total nonces per launch = blocks * threads_per_block * nonces_per_thread.
    /// Default 560*256*4096 = 587M nonces/launch.
    #[arg(long, default_value_t = 4096)]
    nonces_per_thread: u32,

    /// Dual mining: CPU worker threads to run alongside the GPU backend.
    /// Default 0 (GPU-only) — the GPU does the hashing and the CPU stays free.
    /// A full CPU pool on a GPU rig burns power and heat for ~0.1% more
    /// hashrate, and on laptops it can *lower* GPU hashrate by stealing the
    /// shared power/thermal budget. Raise it (e.g. 8 or 16) only on a desktop
    /// with thermal headroom. Range 0..num_cpus; each worker uses SHA-NI via
    /// sha2::compress256.
    #[arg(long, default_value_t = 0)]
    cpu_threads: usize,

    /// Dual mining: fraction of the per-template nonce range the CPU pool
    /// sweeps (0.0..=1.0). GPU takes the rest. Ignored when --cpu-threads 0.
    #[arg(long, default_value_t = 0.4)]
    cpu_share: f32,

    /// GPU device index to mine on (see the `devices` subcommand for the list).
    /// Default 0. To use multiple GPUs, run one instance per card, each with a
    /// different --device (e.g. --device 0 and --device 1), all to the same
    /// address — the pool sums their shares.
    #[arg(long, default_value_t = 0)]
    device: usize,

    /// Log directory (rotates previous log on startup).
    #[arg(long, default_value = "logs")]
    log_dir: PathBuf,

    /// Serve live telemetry as JSON on `http://127.0.0.1:<PORT>/stats`
    /// (loopback only). The native launcher sets this so it can display
    /// hashrate, shares and connection state. Omit or set 0 to disable.
    #[arg(long)]
    stats_port: Option<u16>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Validate an addr20 payout address and return its canonical 40-lowercase-hex
/// form (the `0x` prefix, if present, is stripped). Accepts exactly 40
/// lowercase hex chars, or 42 chars when `0x`-prefixed. Rejects wrong length,
/// uppercase, and any non-hex character.
///
/// Kept pure (no I/O) so it is unit-testable and so `main` can fail fast with a
/// clear message before opening a socket to the pool.
fn validate_address(addr: &str) -> Result<String> {
    let body = addr.strip_prefix("0x").unwrap_or(addr);
    if body.len() != 40 {
        bail!(
            "--address must be 40 hex chars (or 42 with a 0x prefix); got {} chars",
            addr.len()
        );
    }
    if !body.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("--address must be lowercase hex (0-9, a-f); got {addr:?}");
    }
    Ok(body.to_string())
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Create a brand-new CSD payout wallet (keypair + addr20) locally, print
    /// it, and save it to ./csd-wallet.txt. The private key is generated on
    /// this machine and is NEVER sent anywhere — back it up, losing it loses
    /// the coins. The address it prints is what you pass to `--address`.
    Newwallet,

    /// Probe and print available GPU devices, then exit. Use this when
    /// `--backend auto` keeps falling back to CPU and you want to know why.
    Devices {
        /// Emit a machine-readable JSON device list (unified GPU list with
        /// per-device backend + index, plus CPU core count). The launcher
        /// consumes this to populate its device pickers.
        #[arg(long)]
        json: bool,
    },

    /// Cross-check every available backend against the canonical CPU
    /// sha256d on randomized inputs. Exits 0 if all backends agree, 1
    /// on any mismatch.
    Selftest {
        /// Number of randomized trials per backend.
        #[arg(long, default_value_t = 4)]
        trials: usize,

        /// Nonce range to scan per trial (must be <= u32::MAX).
        #[arg(long, default_value_t = 1_048_576)]
        nonce_range: u32,

        /// How many leading zero bytes the target requires (controls
        /// expected hits-per-trial). Default 2 → ~16 hits in 1M.
        #[arg(long, default_value_t = 2)]
        target_zero_bytes: usize,

        /// Deterministic RNG seed so failures are reproducible.
        #[arg(long, default_value_t = 0xC0FFEE)]
        seed: u64,
    },

    /// Micro-benchmark the CPU hashing paths (scalar vs N-way interleaved
    /// SHA-NI batch) on this machine and print MH/s + speedup. No network.
    Bench {
        /// Nonces to hash per path.
        #[arg(long, default_value_t = 20_000_000)]
        nonces: u32,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BackendChoice {
    Auto,
    Cpu,
    Opencl,
    Cuda,
}

/// Best-effort machine name for the default worker/rig label. Prefers the OS
/// hostname; falls back to `None` (miner then authorizes with the bare
/// address). No `hostname` crate dependency — reads the env vars the platform
/// sets, then `/etc/hostname`.
fn default_worker_name() -> Option<String> {
    for var in ["CAIRN_WORKER", "HOSTNAME", "COMPUTERNAME"] {
        if let Some(v) = std::env::var_os(var) {
            let s = v.to_string_lossy().trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    #[cfg(not(windows))]
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Keep only `[A-Za-z0-9_-]` from a worker name (dots break the pool's
/// `address.rig` split; other chars can confuse Stratum framing). Truncated to
/// 32 chars so a runaway hostname can't bloat every authorize line.
fn sanitize_worker(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(32)
        .collect()
}

fn num_cpus_default() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn cpu_hashing_threads(cli: &Cli) -> usize {
    if let Some(t) = cli.threads {
        return t.max(1);
    }
    let avail = num_cpus_default();
    avail.saturating_sub(cli.reserve).max(1)
}

/// Build the dual-mining MiningConfig from CLI flags.
///
/// When a GPU backend is active, `--cpu-threads`/`--cpu-share` directly
/// drive the in-loop CPU worker pool that races the GPU per launch. When the
/// active backend IS the CPU backend (no GPU usable), we deliberately zero the
/// dual-mining pool: the CPU backend already saturates all its hashing threads
/// internally, so spawning a second pool inside the loop would just contend
/// with itself.
fn build_mining_config(cli: &Cli, backend_is_cpu: bool) -> MiningConfig {
    if backend_is_cpu {
        return MiningConfig {
            cpu_threads: 0,
            cpu_share: 0.0,
        };
    }
    let max_threads = num_cpus_default();
    let cpu_threads = cli.cpu_threads.min(max_threads);
    let cpu_share = cli.cpu_share.clamp(0.0, 1.0);
    MiningConfig {
        cpu_threads,
        cpu_share,
    }
}

/// Parse a backend name from the config file into a `BackendChoice`.
fn parse_backend(s: &str) -> Option<BackendChoice> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(BackendChoice::Auto),
        "cpu" => Some(BackendChoice::Cpu),
        "opencl" => Some(BackendChoice::Opencl),
        "cuda" => Some(BackendChoice::Cuda),
        _ => None,
    }
}

/// Merge `file` config values into `cli` IN PLACE, but only for fields the user
/// did NOT set explicitly on the command line — giving precedence
/// CLI > config file > built-in default. `address` has no clap default, so it is
/// taken from the file only when absent on the CLI.
fn merge_config(cli: &mut Cli, matches: &clap::ArgMatches, file: config_file::FileConfig) {
    use clap::parser::ValueSource;
    let explicit = |id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);

    if cli.address.is_none() {
        cli.address = file.address;
    }
    if cli.pool.is_empty() {
        if let Some(pools) = file.pool {
            cli.pool = pools;
        }
    }
    if cli.worker.is_none() {
        cli.worker = file.worker;
    }
    if !explicit("backend") {
        if let Some(s) = file.backend.as_deref() {
            match parse_backend(s) {
                Some(b) => cli.backend = b,
                None => tracing::warn!(backend = s, "config: unknown backend, keeping default"),
            }
        }
    }
    if !explicit("threads") {
        if let Some(v) = file.threads {
            cli.threads = Some(v);
        }
    }
    if !explicit("reserve") {
        if let Some(v) = file.reserve {
            cli.reserve = v;
        }
    }
    if !explicit("blocks") {
        if let Some(v) = file.blocks {
            cli.blocks = v;
        }
    }
    if !explicit("threads_per_block") {
        if let Some(v) = file.threads_per_block {
            cli.threads_per_block = v;
        }
    }
    if !explicit("nonces_per_thread") {
        if let Some(v) = file.nonces_per_thread {
            cli.nonces_per_thread = v;
        }
    }
    if !explicit("cpu_threads") {
        if let Some(v) = file.cpu_threads {
            cli.cpu_threads = v;
        }
    }
    if !explicit("cpu_share") {
        if let Some(v) = file.cpu_share {
            cli.cpu_share = v;
        }
    }
    if !explicit("device") {
        if let Some(v) = file.device {
            cli.device = v;
        }
    }
}

fn main() {
    if let Err(e) = run() {
        // Print the full error chain (anyhow's `{:?}` includes the causes).
        eprintln!("\nError: {e:?}");
        // A double-clicked console .exe on Windows closes its window the instant
        // the process exits, so the message above would flash and vanish — the
        // #1 "it just instantly stops" report. If we own an interactive console
        // (i.e. double-clicked, not piped from a script/service), hold it open
        // until the user has read the error.
        #[cfg(windows)]
        {
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                eprintln!("\n────────────────────────────────────────────────────────");
                eprintln!("cairn-miner could not start (see the error above).");
                eprintln!("The most common cause: no payout address was given.");
                eprintln!("Run it from a terminal with your address, e.g.:");
                eprintln!("    cairn-miner.exe --address <your-addr20>");
                eprintln!("...or use the cairn-miner launcher, which sets this for you.");
                eprintln!("────────────────────────────────────────────────────────");
                eprintln!("Press Enter to close this window...");
                let mut _s = String::new();
                let _ = std::io::stdin().read_line(&mut _s);
            }
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).map_err(|e| anyhow::anyhow!("{e}"))?;
    let _log_guard = logging::init("cairn-miner", &cli.log_dir)?;

    // Merge an optional TOML config file. Precedence: explicit CLI flag > config
    // file value > built-in default. Done before any subcommand so values set in
    // the config (geometry, etc.) also apply to `selftest`. Logging is already
    // up, so a parse-failed config produces a visible warning (then is ignored).
    let (file_cfg, loaded_from) = config_file::FileConfig::load(cli.config.as_deref());
    if let Some(p) = &loaded_from {
        tracing::info!(config = %p.display(), "loaded config file");
    }
    merge_config(&mut cli, &matches, file_cfg);

    if matches!(cli.cmd, Some(Cmd::Newwallet)) {
        // No network, no address needed: generate a key locally and exit.
        return keygen::run();
    }

    if let Some(Cmd::Devices { json }) = cli.cmd {
        return print_devices(json);
    }

    if let Some(Cmd::Bench { nonces }) = cli.cmd {
        return cairn_miner::bench::run(cairn_miner::bench::BenchOpts { nonces });
    }

    if let Some(Cmd::Selftest {
        trials,
        nonce_range,
        target_zero_bytes,
        seed,
    }) = cli.cmd
    {
        return cairn_miner::selftest::run(cairn_miner::selftest::SelftestOpts {
            trials,
            nonce_range,
            target_zero_bytes,
            seed,
            blocks: cli.blocks,
            threads_per_block: cli.threads_per_block,
            nonces_per_thread: cli.nonces_per_thread,
        });
    }

    print_build_features();

    // Validate the payout address up front so a typo fails fast (before we open
    // a socket to the pool) with a clear message. It may come from --address or
    // the config file's `address =` key.
    let address = match cli.address.as_deref() {
        Some(a) => validate_address(a)?,
        None => bail!(
            "no payout address: pass --address <addr20>, or set `address = \"<addr20>\"` in a config file (see config.example.toml / the README)"
        ),
    };

    // Endpoint precedence: --pool flag(s) > config file `pool =` > built-in
    // default (the public cairn pool). Multiple endpoints = initial-connect
    // failover, first one that answers wins.
    let pools: Vec<String> = if cli.pool.is_empty() {
        vec![endpoint::pool_endpoint()]
    } else {
        cli.pool.clone()
    };
    // The authorize username is `<address>.<worker>` so per-rig stats separate
    // on the dashboard; the pool credits the bare address regardless. Worker
    // name is sanitized to `[A-Za-z0-9_-]` (dots would confuse the pool's
    // address.rig split) and defaults to the hostname.
    let worker = cli
        .worker
        .clone()
        .or_else(default_worker_name)
        .map(|w| sanitize_worker(&w))
        .filter(|w| !w.is_empty());
    let auth_username = match &worker {
        Some(w) => format!("{address}.{w}"),
        None => address.clone(),
    };

    // Shared live telemetry: the Stratum client (connection/shares/difficulty)
    // and the mining loop (hashrate) both update this one instance, and the
    // optional loopback stats server reads it for the launcher.
    let stats = Arc::new(cairn_miner::stats::MinerStats::new());

    let mut client = None;
    let mut last_err = None;
    for endpoint in &pools {
        tracing::info!("cairn-miner: connecting to pool {endpoint} as {auth_username}");
        match StratumClient::connect_with_stats_and_endpoints(
            endpoint,
            &auth_username,
            stats.clone(),
            pools.clone(),
        ) {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(e) => {
                tracing::warn!("pool {endpoint} unreachable: {e}");
                last_err = Some(anyhow::anyhow!("failed to connect to pool {endpoint}: {e}"));
            }
        }
    }
    let client = match client {
        Some(c) => c,
        None => return Err(last_err.expect("at least one pool endpoint attempted")),
    };

    // Publish descriptive metadata and, if requested, start the loopback stats
    // server the launcher polls. A port of 0 (or the flag omitted) disables it.
    stats.set_meta(client.endpoint(), worker.as_deref().unwrap_or(""));
    if let Some(port) = cli.stats_port {
        if port != 0 {
            cairn_miner::stats_server::spawn(stats.clone(), port);
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc_lite(move || {
            tracing::warn!("ctrl-c, shutting down");
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    match cli.backend {
        BackendChoice::Cpu => {
            let n = cpu_hashing_threads(&cli);
            let b = CpuBackend::new(n);
            tracing::info!(
                "backend=cpu (forced) hashing_threads={} reserved={}",
                b.threads,
                cli.reserve
            );
            run_stratum(&b, &client, stop, build_mining_config(&cli, true))
        }

        #[cfg(feature = "opencl")]
        BackendChoice::Opencl => {
            tracing::info!(
                "backend=opencl (forced) blocks={} tpb={} npt={} - trying init...",
                cli.blocks, cli.threads_per_block, cli.nonces_per_thread,
            );
            let init = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                OpenclBackend::new(cli.device, cli.blocks, cli.threads_per_block, cli.nonces_per_thread)
            }));
            let b = match init {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    tracing::error!("opencl init failed: {}", e);
                    bail!("opencl init failed: {}", e);
                }
                Err(_) => bail!("opencl init panicked; try --backend cpu"),
            };
            tracing::info!(
                "backend=opencl ready (geom={}x{}x{} = {} nonces/launch, 2-queue pipelined)",
                b.blocks, b.threads_per_block, b.nonces_per_thread,
                (b.blocks as u64) * (b.threads_per_block as u64) * (b.nonces_per_thread as u64),
            );
            run_stratum(&b, &client, stop, build_mining_config(&cli, false))
        }
        #[cfg(not(feature = "opencl"))]
        BackendChoice::Opencl => bail!("opencl backend not compiled in (rebuild with --features opencl)"),

        #[cfg(feature = "cuda")]
        BackendChoice::Cuda => {
            tracing::info!(
                "backend=cuda (forced) blocks={} tpb={} npt={} - trying init...",
                cli.blocks, cli.threads_per_block, cli.nonces_per_thread,
            );
            // cudarc can panic (not just return Err) on a low-level driver/context
            // error during init; catch it so we exit with a clear message instead
            // of an unwinding backtrace.
            let init = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                CudaBackend::new(cli.device, cli.blocks, cli.threads_per_block, cli.nonces_per_thread)
            }));
            let b = match init {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    tracing::error!("cuda init failed: {}", e);
                    bail!("cuda init failed: {}", e);
                }
                Err(_) => bail!(
                    "cuda init panicked (driver/context error during init); try --backend opencl or --backend cpu"
                ),
            };
            tracing::info!(
                "backend=cuda ready (geom={}x{}x{} = {} nonces/launch, 2-stream pipelined)",
                b.blocks, b.threads_per_block, b.nonces_per_thread,
                (b.blocks as u64) * (b.threads_per_block as u64) * (b.nonces_per_thread as u64),
            );
            run_stratum(&b, &client, stop, build_mining_config(&cli, false))
        }
        #[cfg(not(feature = "cuda"))]
        BackendChoice::Cuda => bail!("cuda backend not compiled in (rebuild with --features cuda)"),

        BackendChoice::Auto => {
            tracing::info!("backend=auto - probing in order: cuda -> opencl -> cpu");

            #[cfg(feature = "cuda")]
            {
                tracing::info!(
                    "auto: trying CUDA geom={}x{}x{}",
                    cli.blocks, cli.threads_per_block, cli.nonces_per_thread
                );
                // cudarc can panic (rather than return Err) on a low-level driver
                // or context error during init. Catch the panic so `auto` can fall
                // through to OpenCL instead of crashing.
                let cuda_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    CudaBackend::new(cli.device, cli.blocks, cli.threads_per_block, cli.nonces_per_thread)
                }));
                match cuda_result {
                    Ok(Ok(b)) => {
                        tracing::info!(
                            "auto: SELECTED cuda (geom={}x{}x{} = {} nonces/launch, 2-stream pipelined)",
                            b.blocks, b.threads_per_block, b.nonces_per_thread,
                            (b.blocks as u64) * (b.threads_per_block as u64) * (b.nonces_per_thread as u64),
                        );
                        return run_stratum(&b, &client, stop, build_mining_config(&cli, false));
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("auto: CUDA init returned error: {}", e);
                    }
                    Err(p) => {
                        let msg = if let Some(s) = p.downcast_ref::<&'static str>() {
                            (*s).to_string()
                        } else if let Some(s) = p.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "<non-string panic>".to_string()
                        };
                        tracing::warn!(
                            "auto: CUDA init panicked (cudarc/nvrtc version mismatch?): {}",
                            msg
                        );
                    }
                }
            }
            #[cfg(not(feature = "cuda"))]
            {
                tracing::warn!("auto: CUDA not compiled in (build with --features cuda to enable)");
            }

            #[cfg(feature = "opencl")]
            {
                tracing::info!(
                    "auto: trying OpenCL geom={}x{}x{}",
                    cli.blocks, cli.threads_per_block, cli.nonces_per_thread
                );
                // Some AMD ICDs panic (rather than return Err) during platform
                // enumeration or context creation. Catch the panic so `auto`
                // falls through to CPU instead of aborting. Mirrors the CUDA
                // sibling above and the forced `--backend opencl` path.
                let opencl_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    OpenclBackend::new(cli.device, cli.blocks, cli.threads_per_block, cli.nonces_per_thread)
                }));
                match opencl_result {
                    Ok(Ok(b)) => {
                        tracing::info!(
                            "auto: SELECTED opencl (geom={}x{}x{} = {} nonces/launch, 2-queue pipelined)",
                            b.blocks, b.threads_per_block, b.nonces_per_thread,
                            (b.blocks as u64) * (b.threads_per_block as u64) * (b.nonces_per_thread as u64),
                        );
                        return run_stratum(&b, &client, stop, build_mining_config(&cli, false));
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("auto: OpenCL init failed: {}", e);
                    }
                    Err(p) => {
                        let msg = if let Some(s) = p.downcast_ref::<&'static str>() {
                            (*s).to_string()
                        } else if let Some(s) = p.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "<non-string panic>".to_string()
                        };
                        tracing::warn!(
                            "auto: OpenCL init panicked (broken AMD ICD?): {}",
                            msg
                        );
                    }
                }
            }
            #[cfg(not(feature = "opencl"))]
            {
                tracing::warn!("auto: OpenCL not compiled in");
            }

            let n = cpu_hashing_threads(&cli);
            let b = CpuBackend::new(n);
            tracing::warn!(
                "auto: SELECTED cpu (no GPU backend usable). hashing_threads={} reserved={}",
                b.threads,
                cli.reserve
            );
            run_stratum(&b, &client, stop, build_mining_config(&cli, true))
        }
    }
}

fn print_build_features() {
    let cuda = cfg!(feature = "cuda");
    let opencl = cfg!(feature = "opencl");
    tracing::info!(
        "build features: cuda={} opencl={}",
        cuda,
        opencl
    );
    if !cuda {
        tracing::info!("  to enable CUDA: cargo build -p cairn-miner --release --features cuda");
    }
}

fn print_devices(json: bool) -> Result<()> {
    if json {
        return print_devices_json();
    }
    println!("=== cairn-miner devices ===");
    println!();
    println!("build features: cuda={} opencl={}", cfg!(feature = "cuda"), cfg!(feature = "opencl"));
    println!();

    #[cfg(feature = "cuda")]
    {
        println!("CUDA:");
        // cudarc 0.19: CudaDevice -> CudaContext.
        match cudarc::driver::CudaContext::device_count() {
            Ok(n) if n > 0 => {
                for i in 0..n {
                    match cudarc::driver::CudaContext::new(i as usize) {
                        Ok(ctx) => {
                            let name = ctx.name().unwrap_or_else(|_| "<unknown>".into());
                            println!("  [{}] {}", i, name);
                        }
                        Err(e) => println!("  [{}] (init failed: {})", i, e),
                    }
                }
            }
            Ok(_) => println!("  (no CUDA devices)"),
            Err(e) => println!("  (CUDA driver not reachable: {})", e),
        }
        println!();
    }
    #[cfg(not(feature = "cuda"))]
    {
        println!("CUDA: backend not compiled in (build with --features cuda)");
        println!();
    }

    #[cfg(feature = "opencl")]
    {
        println!("OpenCL:");
        use opencl3::device::{get_all_devices, Device, CL_DEVICE_TYPE_ALL};
        match get_all_devices(CL_DEVICE_TYPE_ALL) {
            Ok(devs) if !devs.is_empty() => {
                for (i, d) in devs.iter().enumerate() {
                    let dev = Device::new(*d);
                    let name = dev.name().unwrap_or_default();
                    let vendor = dev.vendor().unwrap_or_default();
                    let version = dev.version().unwrap_or_default();
                    println!("  [{}] {} ({}) - {}", i, name, vendor, version);
                }
            }
            Ok(_) => println!("  (no OpenCL devices)"),
            Err(e) => println!("  (OpenCL not reachable: {:?})", e),
        }
    }
    #[cfg(not(feature = "opencl"))]
    {
        println!("OpenCL: backend not compiled in (build with --features opencl)");
    }
    Ok(())
}

/// Machine-readable device list for the launcher: a unified GPU list (each with
/// its backend + the `--device` index to use) plus the CPU core count. NVIDIA
/// GPUs are reported once (via CUDA when compiled in) rather than duplicated
/// through the OpenCL enumeration. Emits a single JSON line on stdout.
fn print_devices_json() -> Result<()> {
    #[derive(serde::Serialize)]
    struct GpuInfo {
        backend: &'static str,
        index: usize,
        name: String,
    }
    #[derive(serde::Serialize)]
    struct CpuInfo {
        logical_cores: usize,
    }
    #[derive(serde::Serialize)]
    struct DeviceList {
        gpus: Vec<GpuInfo>,
        cpu: CpuInfo,
        /// Human-readable reasons a backend found nothing (driver error, not
        /// compiled in, …) so the launcher can explain "no GPUs" instead of
        /// failing silently.
        notes: Vec<String>,
    }

    // `mut` is only needed when a GPU backend is compiled in.
    #[allow(unused_mut)]
    let mut gpus: Vec<GpuInfo> = Vec::new();
    #[allow(unused_mut)]
    let mut notes: Vec<String> = Vec::new();

    #[cfg(feature = "cuda")]
    {
        match cudarc::driver::CudaContext::device_count() {
            Ok(n) if n > 0 => {
                for i in 0..n as usize {
                    match cudarc::driver::CudaContext::new(i) {
                        Ok(ctx) => {
                            let name = ctx.name().unwrap_or_else(|_| "NVIDIA GPU".into());
                            gpus.push(GpuInfo { backend: "cuda", index: i, name });
                        }
                        Err(e) => notes.push(format!("cuda device {i}: {e}")),
                    }
                }
            }
            Ok(_) => notes.push("cuda: driver reachable but no devices".into()),
            Err(e) => notes.push(format!("cuda: {e}")),
        }
    }
    #[cfg(not(feature = "cuda"))]
    notes.push("cuda: not compiled into this build".into());

    #[cfg(feature = "opencl")]
    {
        use opencl3::device::{get_all_devices, Device, CL_DEVICE_TYPE_GPU};
        // Same enumeration the OpenCL backend uses, so the reported index is the
        // one `--device` expects.
        match get_all_devices(CL_DEVICE_TYPE_GPU) {
            Ok(devs) => {
                for (i, d) in devs.iter().enumerate() {
                    let dev = Device::new(*d);
                    let vendor = dev.vendor().unwrap_or_default();
                    // Skip NVIDIA cards here only if CUDA is compiled in (then
                    // they already appear above); otherwise list them via OpenCL.
                    if cfg!(feature = "cuda") && vendor.to_uppercase().contains("NVIDIA") {
                        continue;
                    }
                    let name = dev.name().unwrap_or_default();
                    gpus.push(GpuInfo { backend: "opencl", index: i, name });
                }
            }
            Err(e) => notes.push(format!("opencl: {e:?}")),
        }
    }
    #[cfg(not(feature = "opencl"))]
    notes.push("opencl: not compiled into this build".into());

    let logical_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let list = DeviceList {
        gpus,
        cpu: CpuInfo { logical_cores },
        notes,
    };
    println!("{}", serde_json::to_string(&list)?);
    Ok(())
}

/// Install a Ctrl-C handler that runs `handler` (which sets the stop flag) on
/// interrupt, so the miner shuts down cleanly instead of being hard-killed.
fn ctrlc_lite<F: Fn() + Send + 'static>(handler: F) {
    if let Err(e) = ctrlc::set_handler(move || handler()) {
        tracing::warn!("could not install ctrl-c handler ({e}); Ctrl-C will hard-stop");
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_worker, validate_address};

    #[test]
    fn worker_sanitizer_keeps_safe_chars_and_drops_dots() {
        assert_eq!(sanitize_worker("rig-01_gpu"), "rig-01_gpu");
        assert_eq!(sanitize_worker("my.rig.name"), "myrigname");
        assert_eq!(sanitize_worker("space rig!"), "spacerig");
        assert_eq!(sanitize_worker(&"a".repeat(64)).len(), 32);
    }

    #[test]
    fn accepts_40_lowercase_hex() {
        let addr = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(addr.len(), 40);
        assert_eq!(validate_address(addr).unwrap(), addr);
    }

    #[test]
    fn accepts_0x_prefixed_and_strips_it() {
        let body = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let prefixed = format!("0x{body}");
        assert_eq!(prefixed.len(), 42);
        // The canonical form drops the 0x prefix.
        assert_eq!(validate_address(&prefixed).unwrap(), body);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(validate_address("abcd").is_err()); // too short
        assert!(validate_address(&"a".repeat(39)).is_err()); // 39
        assert!(validate_address(&"a".repeat(41)).is_err()); // 41 (no 0x)
        assert!(validate_address(&format!("0x{}", "a".repeat(39))).is_err()); // 0x + 39
        assert!(validate_address(&format!("0x{}", "a".repeat(41))).is_err()); // 0x + 41
    }

    #[test]
    fn rejects_non_hex() {
        // 'g' is not a hex digit.
        assert!(validate_address("0123456789abcdef0123456789abcdef0123456g").is_err());
    }

    #[test]
    fn rejects_uppercase() {
        // Uppercase hex is rejected (addr20 addresses are lowercase hex).
        assert!(validate_address("0123456789ABCDEF0123456789abcdef01234567").is_err());
    }
}
