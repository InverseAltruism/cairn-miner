//! Live miner telemetry, shared between the Stratum client's reader thread
//! (connection state, accepted/rejected shares, difficulty), the mining loop
//! (hashrate, submitted shares) and the optional loopback stats server
//! ([`crate::stats_server`]) that the native launcher polls.
//!
//! All hot fields are lock-free atomics (`f64`s are stored as their bit
//! pattern in an `AtomicU64`); the handful of descriptive string fields are
//! written once at startup and read rarely, so a `Mutex` is fine for them.
//! The whole thing lives behind an `Arc` so every producer and the reader
//! share one instance.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Shared, thread-safe live counters for one miner process.
pub struct MinerStats {
    start: Instant,
    connected: AtomicBool,
    difficulty_bits: AtomicU64,
    gpu_hps_bits: AtomicU64,
    cpu_hps_bits: AtomicU64,
    shares_submitted: AtomicU64,
    shares_accepted: AtomicU64,
    shares_rejected: AtomicU64,
    /// Shares the pool rejected specifically as stale (reject code 21) — work
    /// that was valid but arrived after the tip moved. Tracked apart from
    /// `shares_rejected` so a rig can tell "too slow / bad tuning" (stale) from
    /// "wrong / low-diff" (rejected).
    shares_stale: AtomicU64,
    /// Count of successful reconnects to the pool after a drop (0 = the link has
    /// held since startup). A climbing value points at a flaky network/pool.
    reconnects: AtomicU64,
    /// Unix seconds of the most recent accepted share (0 = none yet).
    last_share_unix: AtomicU64,
    pool: Mutex<String>,
    worker: Mutex<String>,
    backend: Mutex<String>,
}

impl Default for MinerStats {
    fn default() -> Self {
        Self::new()
    }
}

impl MinerStats {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            connected: AtomicBool::new(false),
            difficulty_bits: AtomicU64::new(1.0f64.to_bits()),
            gpu_hps_bits: AtomicU64::new(0),
            cpu_hps_bits: AtomicU64::new(0),
            shares_submitted: AtomicU64::new(0),
            shares_accepted: AtomicU64::new(0),
            shares_rejected: AtomicU64::new(0),
            shares_stale: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            last_share_unix: AtomicU64::new(0),
            pool: Mutex::new(String::new()),
            worker: Mutex::new(String::new()),
            backend: Mutex::new(String::new()),
        }
    }

    // --- setters (called from the mining loop / client reader) ---

    pub fn set_connected(&self, up: bool) {
        self.connected.store(up, Ordering::Relaxed);
    }

    pub fn set_difficulty(&self, d: f64) {
        self.difficulty_bits.store(d.to_bits(), Ordering::Relaxed);
    }

    /// Publish the latest measured hash rates, in hashes/second.
    pub fn set_hps(&self, gpu_hps: f64, cpu_hps: f64) {
        self.gpu_hps_bits.store(gpu_hps.to_bits(), Ordering::Relaxed);
        self.cpu_hps_bits.store(cpu_hps.to_bits(), Ordering::Relaxed);
    }

    pub fn on_share_submitted(&self) {
        self.shares_submitted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn on_share_accepted(&self) {
        self.shares_accepted.fetch_add(1, Ordering::Relaxed);
        self.last_share_unix
            .store(now_unix(), Ordering::Relaxed);
    }

    pub fn on_share_rejected(&self) {
        self.shares_rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// A share the pool rejected as stale (reject code 21). Counted here AND in
    /// `shares_rejected` so `shares_rejected` stays the total-rejected figure.
    pub fn on_share_stale(&self) {
        self.shares_stale.fetch_add(1, Ordering::Relaxed);
        self.shares_rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// A successful reconnect after the pool link dropped.
    pub fn on_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    /// One-time descriptive metadata for the dashboard header.
    pub fn set_meta(&self, pool: &str, worker: &str) {
        if let Ok(mut g) = self.pool.lock() {
            *g = pool.to_string();
        }
        if let Ok(mut g) = self.worker.lock() {
            *g = worker.to_string();
        }
    }

    pub fn set_backend(&self, backend: &str) {
        if let Ok(mut g) = self.backend.lock() {
            *g = backend.to_string();
        }
    }

    // --- read side ---

    pub fn snapshot(&self) -> StatsSnapshot {
        let gpu = f64::from_bits(self.gpu_hps_bits.load(Ordering::Relaxed));
        let cpu = f64::from_bits(self.cpu_hps_bits.load(Ordering::Relaxed));
        let last = self.last_share_unix.load(Ordering::Relaxed);
        let last_age = if last == 0 {
            None
        } else {
            Some(now_unix().saturating_sub(last))
        };
        StatsSnapshot {
            connected: self.connected.load(Ordering::Relaxed),
            uptime_secs: self.start.elapsed().as_secs(),
            difficulty: f64::from_bits(self.difficulty_bits.load(Ordering::Relaxed)),
            hashrate_gpu_hps: gpu,
            hashrate_cpu_hps: cpu,
            hashrate_total_hps: gpu + cpu,
            shares_submitted: self.shares_submitted.load(Ordering::Relaxed),
            shares_accepted: self.shares_accepted.load(Ordering::Relaxed),
            shares_rejected: self.shares_rejected.load(Ordering::Relaxed),
            shares_stale: self.shares_stale.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            last_share_age_secs: last_age,
            pool: self.pool.lock().map(|g| g.clone()).unwrap_or_default(),
            worker: self.worker.lock().map(|g| g.clone()).unwrap_or_default(),
            backend: self.backend.lock().map(|g| g.clone()).unwrap_or_default(),
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// JSON-serializable point-in-time view, served at `/stats` and consumed by the
/// launcher. Field names are stable API — the launcher deserializes them.
#[derive(Serialize, Clone, Debug)]
pub struct StatsSnapshot {
    pub connected: bool,
    pub uptime_secs: u64,
    pub difficulty: f64,
    pub hashrate_gpu_hps: f64,
    pub hashrate_cpu_hps: f64,
    pub hashrate_total_hps: f64,
    pub shares_submitted: u64,
    pub shares_accepted: u64,
    pub shares_rejected: u64,
    pub shares_stale: u64,
    pub reconnects: u64,
    pub last_share_age_secs: Option<u64>,
    pub pool: String,
    pub worker: String,
    pub backend: String,
    pub version: &'static str,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_hashrate_and_share_counters() {
        let s = MinerStats::new();
        // Defaults.
        let snap = s.snapshot();
        assert!(!snap.connected);
        assert_eq!(snap.difficulty, 1.0);
        assert_eq!(snap.hashrate_total_hps, 0.0);
        assert_eq!(snap.shares_submitted, 0);
        assert!(snap.last_share_age_secs.is_none());

        // Producers update from several threads' worth of events.
        s.set_connected(true);
        s.set_difficulty(8.0);
        s.set_hps(1_200_000.0, 300_000.0);
        s.on_share_submitted();
        s.on_share_submitted();
        s.on_share_accepted();
        s.on_share_rejected();
        s.set_meta("cairn-pool.com:3333", "rig-01");
        s.set_backend("cuda");

        let snap = s.snapshot();
        assert!(snap.connected);
        assert_eq!(snap.difficulty, 8.0);
        assert_eq!(snap.hashrate_gpu_hps, 1_200_000.0);
        assert_eq!(snap.hashrate_cpu_hps, 300_000.0);
        assert_eq!(snap.hashrate_total_hps, 1_500_000.0);
        assert_eq!(snap.shares_submitted, 2);
        assert_eq!(snap.shares_accepted, 1);
        assert_eq!(snap.shares_rejected, 1);
        // An accepted share stamps last-share; age is small but present.
        assert!(snap.last_share_age_secs.is_some());
        assert_eq!(snap.pool, "cairn-pool.com:3333");
        assert_eq!(snap.worker, "rig-01");
        assert_eq!(snap.backend, "cuda");
    }

    #[test]
    fn snapshot_serializes_to_stable_json_keys() {
        let s = MinerStats::new();
        let json = serde_json::to_string(&s.snapshot()).unwrap();
        // The launcher deserializes these keys; keep them stable.
        for key in [
            "connected",
            "uptime_secs",
            "difficulty",
            "hashrate_total_hps",
            "shares_accepted",
            "shares_rejected",
            "shares_stale",
            "reconnects",
            "backend",
            "version",
        ] {
            assert!(json.contains(key), "missing key {key} in {json}");
        }
    }

    #[test]
    fn stale_counts_apart_from_but_within_rejected() {
        let s = MinerStats::new();
        s.on_share_rejected(); // a plain reject
        s.on_share_stale(); // a stale (bumps stale AND the rejected total)
        let snap = s.snapshot();
        assert_eq!(snap.shares_stale, 1);
        assert_eq!(snap.shares_rejected, 2); // total rejected includes the stale
        s.on_reconnect();
        s.on_reconnect();
        assert_eq!(s.snapshot().reconnects, 2);
    }
}
