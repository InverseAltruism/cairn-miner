//! The mining engine: spawns and supervises one `cairn-miner` process per
//! selected GPU (plus an optional CPU worker), polls each one's loopback stats
//! endpoint, and aggregates them into a single dashboard view.
//!
//! One process mines exactly one device (the CUDA/OpenCL backends are
//! single-device), so real multi-GPU means several child processes — which the
//! incumbent only does as N separate console windows. Here they're one managed
//! fleet under one window.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use crate::stats::{self, StatsSnapshot};

/// First respawn delay after a worker dies; doubles per consecutive failure.
const RESTART_BACKOFF_START: Duration = Duration::from_secs(2);
/// Ceiling for the respawn backoff.
const RESTART_BACKOFF_CAP: Duration = Duration::from_secs(60);
/// A worker that stays alive this long earns a fresh failure streak — a crash
/// after hours of mining is a new incident, not strike N of the old one.
const RESTART_STABLE_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Consecutive failures after which the engine stops respawning a worker and
/// flags it in the UI instead (something is structurally wrong — bad driver,
/// missing DLL, dead card — and a restart loop would just burn cycles).
const RESTART_MAX_CONSECUTIVE_FAILS: u32 = 20;

/// Respawn delay for the `n`-th consecutive failure (1-based): 2s doubling,
/// capped at [`RESTART_BACKOFF_CAP`].
fn restart_backoff(consecutive_fails: u32) -> Duration {
    let exp = consecutive_fails.saturating_sub(1).min(6); // 2s * 2^6 already exceeds the cap
    (RESTART_BACKOFF_START * 2u32.saturating_pow(exp)).min(RESTART_BACKOFF_CAP)
}

/// What a worker mines. Fields beyond `index` are carried for completeness /
/// future per-worker detail even though the compact label already encodes them.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum WorkerKind {
    Gpu { backend: String, index: usize, name: String },
    Cpu { threads: usize },
}

/// One selected GPU to spawn a worker for.
#[derive(Clone, Debug)]
pub struct GpuSpec {
    pub backend: String,
    pub index: usize,
    pub name: String,
}

/// The resolved plan the UI hands to the engine.
pub struct StartSpec {
    pub miner_exe: PathBuf,
    pub address: String,
    pub worker_base: String,
    pub pools: Vec<String>,
    pub gpus: Vec<GpuSpec>,
    /// `Some(n)` spawns a CPU worker with `n` hashing threads.
    pub cpu_threads: Option<usize>,
    pub log_dir: PathBuf,
}

struct Worker {
    label: String,
    kind: WorkerKind,
    child: Child,
    stats_port: u16,
    log_path: PathBuf,
    last: Option<StatsSnapshot>,
    alive: bool,
    /// Respawn material: the miner exe + the exact final argv (same stats
    /// port, same log subdir), so a restarted worker is indistinguishable
    /// from the original to the stats poller and the log tailer.
    miner: PathBuf,
    args: Vec<String>,
    /// Successful respawns so far (surfaced in the worker row).
    restarts: u64,
    /// Failures (exit or failed spawn) since the last stable run; drives the
    /// exponential backoff and the give-up threshold.
    consecutive_fails: u32,
    /// When the next respawn attempt is allowed. `None` while alive or once
    /// given up. Checked (never slept on) from the UI-thread `poll()`.
    next_retry_at: Option<Instant>,
    /// When the current child was spawned; a run longer than
    /// [`RESTART_STABLE_WINDOW`] resets the failure streak.
    last_spawn: Instant,
    /// Set after [`RESTART_MAX_CONSECUTIVE_FAILS`]: the engine stops
    /// respawning and the UI shows "failing repeatedly — check logs".
    gave_up: bool,
}

impl Worker {
    /// Record one failure (child exited, or a respawn attempt itself failed)
    /// and either schedule the next attempt with exponential backoff or give
    /// up for good.
    fn note_failure(&mut self, now: Instant) {
        self.consecutive_fails = self.consecutive_fails.saturating_add(1);
        if self.consecutive_fails >= RESTART_MAX_CONSECUTIVE_FAILS {
            self.gave_up = true;
            self.next_retry_at = None;
        } else {
            self.next_retry_at = Some(now + restart_backoff(self.consecutive_fails));
        }
    }
}

pub struct Engine {
    workers: Vec<Worker>,
}

/// Aggregated view across all workers.
#[derive(Default, Clone)]
pub struct Aggregate {
    pub connected: bool,
    pub hashrate_total_hps: f64,
    pub shares_accepted: u64,
    pub shares_rejected: u64,
    pub shares_submitted: u64,
    pub difficulty: f64,
    pub uptime_secs: u64,
    pub pool: String,
    pub workers_alive: usize,
    pub workers_total: usize,
}

impl Aggregate {
    pub fn reject_pct(&self) -> f64 {
        let t = self.shares_accepted + self.shares_rejected;
        if t == 0 { 0.0 } else { 100.0 * self.shares_rejected as f64 / t as f64 }
    }
}

/// One row per worker for the performance table.
pub struct WorkerRow {
    pub label: String,
    pub connected: bool,
    pub alive: bool,
    pub hashrate_hps: f64,
    pub accepted: u64,
    pub rejected: u64,
    /// Times the engine has respawned this worker after it died.
    pub restarts: u64,
    /// True once the engine stopped respawning it (kept failing).
    pub gave_up: bool,
}

impl Engine {
    /// Spawn a worker per GPU spec plus an optional CPU worker.
    pub fn start(spec: &StartSpec) -> Result<Engine, String> {
        if spec.gpus.is_empty() && spec.cpu_threads.is_none() {
            return Err("nothing selected to mine — pick a GPU or enable CPU".into());
        }
        let mut workers = Vec::new();

        for g in &spec.gpus {
            let key = format!("{}{}", g.backend, g.index);
            let wname = format!("{}-gpu{}", spec.worker_base, g.index);
            let label = format!("GPU{} · {}", g.index, short_name(&g.name));
            let mut args = base_args(&spec.address, &wname, &spec.pools);
            args.push("--backend".into());
            args.push(g.backend.clone());
            args.push("--device".into());
            args.push(g.index.to_string());
            // GPU workers never dual-mine the CPU (avoids the incumbent's
            // oversubscription-by-default); the CPU is its own worker.
            args.push("--cpu-threads".into());
            args.push("0".into());
            let w = spawn_worker(
                &spec.miner_exe,
                &key,
                label,
                WorkerKind::Gpu { backend: g.backend.clone(), index: g.index, name: g.name.clone() },
                args,
                &spec.log_dir,
            )
            .map_err(|e| format!("failed to start GPU{} ({}): {e}", g.index, g.name))?;
            workers.push(w);
        }

        if let Some(threads) = spec.cpu_threads {
            let wname = format!("{}-cpu", spec.worker_base);
            let label = format!("CPU · {threads} threads");
            let mut args = base_args(&spec.address, &wname, &spec.pools);
            args.push("--backend".into());
            args.push("cpu".into());
            args.push("--threads".into());
            args.push(threads.to_string());
            let w = spawn_worker(
                &spec.miner_exe,
                "cpu",
                label,
                WorkerKind::Cpu { threads },
                args,
                &spec.log_dir,
            )
            .map_err(|e| format!("failed to start CPU worker: {e}"))?;
            workers.push(w);
        }

        Ok(Engine { workers })
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Poll every worker's stats endpoint, reap any that exited, and respawn
    /// dead ones (same command, same stats port, same log subdir) with
    /// exponential backoff. The miner exits BY DESIGN on persistent GPU
    /// faults / wedges so a supervisor restarts it — on Windows, this engine
    /// IS that supervisor, so reap-without-respawn would leave the card dead
    /// for the rest of the session.
    ///
    /// Runs on the UI thread (egui immediate mode): it never sleeps — a
    /// not-yet-due respawn is simply skipped until a later poll tick.
    pub fn poll(&mut self) {
        let now = Instant::now();
        for w in &mut self.workers {
            if w.alive {
                if matches!(w.child.try_wait(), Ok(Some(_))) {
                    // Worker exited (GPU-fault exit, watchdog exit(2), crash…).
                    w.alive = false;
                    w.last = None;
                    // A long healthy run means this is a NEW incident: reset
                    // the streak before counting this exit as failure #1.
                    if now.duration_since(w.last_spawn) >= RESTART_STABLE_WINDOW {
                        w.consecutive_fails = 0;
                    }
                    w.note_failure(now);
                    continue;
                }
                // Still running; a long-enough run clears the failure streak.
                if w.consecutive_fails > 0
                    && now.duration_since(w.last_spawn) >= RESTART_STABLE_WINDOW
                {
                    w.consecutive_fails = 0;
                }
                if let Some(s) = stats::fetch(w.stats_port) {
                    w.last = Some(s);
                }
                continue;
            }

            // Dead: attempt a respawn once its backoff deadline has passed.
            if w.gave_up || !w.next_retry_at.map(|t| now >= t).unwrap_or(false) {
                continue;
            }
            match launch(&w.miner, &w.args) {
                Ok(child) => {
                    w.child = child;
                    w.alive = true;
                    w.last = None;
                    w.last_spawn = now;
                    w.next_retry_at = None;
                    w.restarts = w.restarts.saturating_add(1);
                }
                // Spawn itself failed (exe missing/locked?): same escalation
                // path as an exit, so this can't tight-loop either.
                Err(_) => w.note_failure(now),
            }
        }
    }

    pub fn aggregate(&self) -> Aggregate {
        let mut a = Aggregate {
            workers_total: self.workers.len(),
            ..Default::default()
        };
        for w in &self.workers {
            if w.alive {
                a.workers_alive += 1;
            }
            if let Some(s) = &w.last {
                a.connected |= s.connected;
                a.hashrate_total_hps += s.hashrate_total_hps;
                a.shares_accepted += s.shares_accepted;
                a.shares_rejected += s.shares_rejected;
                a.shares_submitted += s.shares_submitted;
                a.difficulty = a.difficulty.max(s.difficulty);
                a.uptime_secs = a.uptime_secs.max(s.uptime_secs);
                if a.pool.is_empty() && !s.pool.is_empty() {
                    a.pool = s.pool.clone();
                }
            }
        }
        a
    }

    pub fn rows(&self) -> Vec<WorkerRow> {
        self.workers
            .iter()
            .map(|w| {
                let s = w.last.as_ref();
                WorkerRow {
                    label: w.label.clone(),
                    connected: s.map(|s| s.connected).unwrap_or(false),
                    alive: w.alive,
                    hashrate_hps: s.map(|s| s.hashrate_total_hps).unwrap_or(0.0),
                    accepted: s.map(|s| s.shares_accepted).unwrap_or(0),
                    rejected: s.map(|s| s.shares_rejected).unwrap_or(0),
                    restarts: w.restarts,
                    gave_up: w.gave_up,
                }
            })
            .collect()
    }

    /// A merged tail of every worker's log, each line prefixed with its worker.
    pub fn tail_logs(&self, per_worker: usize) -> Vec<String> {
        let mut out = Vec::new();
        for w in &self.workers {
            let tag = short_tag(&w.kind);
            for line in tail_file(&w.log_path, per_worker) {
                out.push(format!("[{tag}] {line}"));
            }
        }
        out
    }

    pub fn stop(&mut self) {
        for w in &mut self.workers {
            let _ = w.child.kill();
            let _ = w.child.wait();
        }
        self.workers.clear();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn base_args(address: &str, worker: &str, pools: &[String]) -> Vec<String> {
    let mut a = vec![
        "--address".into(),
        address.to_string(),
        "--worker".into(),
        worker.to_string(),
    ];
    for p in pools {
        a.push("--pool".into());
        a.push(p.clone());
    }
    a
}

fn spawn_worker(
    miner: &Path,
    key: &str,
    label: String,
    kind: WorkerKind,
    mut args: Vec<String>,
    base_log_dir: &Path,
) -> std::io::Result<Worker> {
    let port = free_port();
    // Each worker logs into its own subdir so their rotating `cairn-miner`
    // logs don't clobber each other.
    let log_dir = base_log_dir.join(key);
    std::fs::create_dir_all(&log_dir).ok();
    let log_path = log_dir.join("cairn-miner.current.log");

    args.push("--stats-port".into());
    args.push(port.to_string());
    args.push("--log-dir".into());
    args.push(log_dir.to_string_lossy().into_owned());

    let child = launch(miner, &args)?;
    Ok(Worker {
        label,
        kind,
        child,
        stats_port: port,
        log_path,
        last: None,
        alive: true,
        miner: miner.to_path_buf(),
        args,
        restarts: 0,
        consecutive_fails: 0,
        next_retry_at: None,
        last_spawn: Instant::now(),
        gave_up: false,
    })
}

/// Spawn one miner child process. Shared by the initial spawn and `poll()`'s
/// respawn path so a restarted worker runs the exact same command.
fn launch(miner: &Path, args: &[String]) -> std::io::Result<Child> {
    let mut cmd = Command::new(miner);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.spawn()
}

fn short_tag(kind: &WorkerKind) -> String {
    match kind {
        WorkerKind::Gpu { index, .. } => format!("gpu{index}"),
        WorkerKind::Cpu { .. } => "cpu".into(),
    }
}

/// Trim a long GPU name for compact labels ("NVIDIA GeForce RTX 2080 SUPER" →
/// "RTX 2080 SUPER").
fn short_name(name: &str) -> String {
    name.trim()
        .trim_start_matches("NVIDIA GeForce ")
        .trim_start_matches("NVIDIA ")
        .trim_start_matches("AMD ")
        .to_string()
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

/// Last `max_lines` lines of a file, reading only the tail chunk.
fn tail_file(path: &Path, max_lines: usize) -> Vec<String> {
    const MAX_BYTES: u64 = 32 * 1024;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(MAX_BYTES);
    let seeked = start > 0;
    if seeked {
        let _ = file.seek(SeekFrom::Start(start));
    }
    let mut lines: VecDeque<String> = VecDeque::new();
    let mut skip_partial = seeked;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if skip_partial {
            skip_partial = false;
            continue;
        }
        if lines.len() == max_lines {
            lines.pop_front();
        }
        lines.push_back(strip_ansi(&line));
    }
    lines.into()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_name_trims_vendor_prefixes() {
        assert_eq!(short_name("NVIDIA GeForce RTX 2080 SUPER"), "RTX 2080 SUPER");
        assert_eq!(short_name("AMD Radeon RX 6800"), "Radeon RX 6800");
    }

    #[test]
    fn base_args_include_pools_and_worker() {
        let a = base_args("addr", "rig-gpu0", &["p1:3333".into(), "p2:3333".into()]);
        assert!(a.windows(2).any(|w| w == ["--worker", "rig-gpu0"]));
        assert_eq!(a.iter().filter(|x| *x == "--pool").count(), 2);
    }

    #[test]
    fn free_port_nonzero() {
        assert_ne!(free_port(), 0);
    }

    #[test]
    fn restart_backoff_doubles_and_caps() {
        assert_eq!(restart_backoff(1), Duration::from_secs(2));
        assert_eq!(restart_backoff(2), Duration::from_secs(4));
        assert_eq!(restart_backoff(5), Duration::from_secs(32));
        assert_eq!(restart_backoff(6), RESTART_BACKOFF_CAP); // 64s clamps to 60s
        assert_eq!(restart_backoff(50), RESTART_BACKOFF_CAP);
        // Defensive: a zero count (shouldn't happen) still yields the floor.
        assert_eq!(restart_backoff(0), Duration::from_secs(2));
    }

    /// Drive the real respawn path with a child that dies instantly
    /// (/bin/false stands in for the miner): the engine must schedule a
    /// backed-off respawn, actually perform it once due, and — after the
    /// failure streak hits the cap — park the worker with `gave_up` instead
    /// of spinning forever. Unix-only (needs /bin/false).
    #[cfg(unix)]
    #[test]
    fn poll_respawns_dead_worker_then_gives_up_after_repeated_failures() {
        let spec = StartSpec {
            miner_exe: PathBuf::from("/bin/false"),
            address: "0123456789abcdef0123456789abcdef01234567".into(),
            worker_base: "restarttest".into(),
            pools: Vec::new(),
            gpus: Vec::new(),
            cpu_threads: Some(1),
            log_dir: std::env::temp_dir().join("cairn-engine-restart-test"),
        };
        let mut engine = Engine::start(&spec).expect("engine start");
        assert_eq!(engine.worker_count(), 1);

        // Let the child exit, then poll: the death must be noted and a retry
        // scheduled — not the old reap-and-forget.
        std::thread::sleep(Duration::from_millis(200));
        engine.poll();
        assert!(!engine.workers[0].alive);
        assert!(!engine.workers[0].gave_up);
        assert_eq!(engine.workers[0].consecutive_fails, 1);
        assert!(engine.workers[0].next_retry_at.is_some());

        // Fast-forward the deadline (tests don't wait out real backoff): the
        // next poll must respawn the same command on the same stats port.
        let port_before = engine.workers[0].stats_port;
        engine.workers[0].next_retry_at = Some(Instant::now());
        engine.poll();
        assert!(engine.workers[0].alive, "worker must be respawned when due");
        assert_eq!(engine.workers[0].restarts, 1);
        assert_eq!(engine.workers[0].stats_port, port_before);

        // Keep fast-forwarding: the streak must reach the cap and park the
        // worker rather than respawning forever.
        let mut guard = 0;
        while !engine.workers[0].gave_up {
            std::thread::sleep(Duration::from_millis(30));
            if let Some(t) = engine.workers[0].next_retry_at.as_mut() {
                *t = Instant::now();
            }
            engine.poll();
            guard += 1;
            assert!(
                guard < 10 * RESTART_MAX_CONSECUTIVE_FAILS,
                "engine never gave up on a permanently failing worker"
            );
        }
        assert!(!engine.workers[0].alive);
        assert!(engine.workers[0].next_retry_at.is_none());
        let rows = engine.rows();
        assert!(rows[0].gave_up, "the row must surface the give-up for the UI");
        assert!(rows[0].restarts >= 1);
        engine.stop();
    }

    /// End-to-end: spawn a real CPU worker against the live pool via the engine
    /// and confirm it connects and aggregates a nonzero hashrate. Networked +
    /// needs the release miner built, so it's ignored by default:
    ///   cargo test -p cairn-miner-launcher --release --ignored engine_cpu_worker
    #[test]
    #[ignore]
    fn engine_cpu_worker_reports_live_stats() {
        let miner = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/release/cairn-miner");
        assert!(miner.exists(), "build the release miner first (cargo build --release)");
        let spec = StartSpec {
            miner_exe: miner,
            address: "03ec5155c0153e5f95fabcc09b6a181465adceb4".into(),
            worker_base: "enginetest".into(),
            pools: vec!["cairn-pool.com:3333".into()],
            gpus: Vec::new(),
            cpu_threads: Some(2),
            log_dir: std::env::temp_dir().join("cairn-engine-test"),
        };
        let mut engine = Engine::start(&spec).expect("engine start");
        assert_eq!(engine.worker_count(), 1);
        let mut ok = false;
        for _ in 0..15 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            engine.poll();
            let a = engine.aggregate();
            if a.connected && a.hashrate_total_hps > 0.0 && a.workers_alive == 1 {
                ok = true;
                break;
            }
        }
        engine.stop();
        assert!(ok, "expected connected + nonzero aggregated hashrate from the CPU worker");
    }
}
