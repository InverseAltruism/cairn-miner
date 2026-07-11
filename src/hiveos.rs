//! HiveOS custom-miner stats aggregator.
//!
//! `cairn-miner hiveos-stats --stats-port BASE [--gpus N]` scrapes the loopback
//! `/stats` endpoint of each per-GPU worker (ports `BASE, BASE+1, …`) and prints
//! the exact JSON the HiveOS agent's `h-stats.sh` hook needs:
//!
//! ```json
//! {"khs": <total kH/s>,
//!  "stats": {"hs":[…kH/s per worker], "hs_units":"khs", "temp":[], "fan":[],
//!            "uptime":<max secs>, "ar":[accepted, rejected], "algo":"sha256d",
//!            "ver":"<version>", "bus_numbers":[]}}
//! ```
//!
//! ALL the arithmetic lives here (the H/s → kH/s `/1000` divisor, per-worker
//! sums, uptime max) — unit-tested — so `h-stats.sh` does zero math and can't
//! reintroduce the class of divisor/field bugs that made every earlier HiveOS
//! build report "online but 0 H/s". On ANY error (no worker responding, a
//! partial read, a parse failure) this still prints a valid *alive-zero* object
//! rather than nothing, so a rig reads "online, 0 H/s" instead of "crashed".
//!
//! `temp`/`fan` are left empty on purpose: the HiveOS agent fills them from its
//! own GPU sensors. `bus_numbers` is reserved for later per-GPU alignment.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long to wait for one worker's `/stats` to answer.
const SCRAPE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Cap when auto-probing an unknown number of GPU worker slots.
const MAX_PROBE_SLOTS: u16 = 32;

/// The subset of the miner's `/stats` we consume. Field names match
/// [`crate::stats::StatsSnapshot`]; any extra fields in the response are
/// ignored, and any missing field defaults (so a schema addition never breaks
/// the scrape).
#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
struct WorkerStats {
    #[serde(default)]
    hashrate_total_hps: f64,
    #[serde(default)]
    shares_accepted: u64,
    #[serde(default)]
    shares_rejected: u64,
    #[serde(default)]
    uptime_secs: u64,
    #[serde(default)]
    version: String,
}

/// HiveOS `$stats` object.
#[derive(Serialize, Debug, PartialEq)]
struct HiveStats {
    /// Per-worker hashrate in the unit named by `hs_units`.
    hs: Vec<f64>,
    hs_units: &'static str,
    /// Left empty — the HiveOS agent fills temps/fans from its own sensors.
    temp: Vec<f64>,
    fan: Vec<f64>,
    uptime: u64,
    /// `[accepted, rejected]`.
    ar: [u64; 2],
    algo: &'static str,
    ver: String,
    bus_numbers: Vec<u64>,
}

/// Top-level object printed to stdout: HiveOS wants the total `khs` and the
/// `stats` blob as two separate shell vars, so we hand it both in one line.
#[derive(Serialize, Debug, PartialEq)]
struct HiveOutput {
    khs: f64,
    stats: HiveStats,
}

/// Aggregate per-worker snapshots into the HiveOS output.
///
/// `workers[i]` is `Some` when GPU slot `i` answered, `None` when it did not
/// (a starting or dead worker) — a `None` slot contributes a `0.0` hs entry so
/// HiveOS shows the card present-but-idle instead of silently dropping it and
/// misaligning the remaining cards. An empty `workers` slice yields the
/// alive-zero object (`hs:[]`, `khs:0`).
fn aggregate(workers: &[Option<WorkerStats>], fallback_ver: &str) -> HiveOutput {
    let mut hs = Vec::with_capacity(workers.len());
    let mut acc: u64 = 0;
    let mut rej: u64 = 0;
    let mut uptime: u64 = 0;
    let mut ver = String::new();

    for w in workers {
        match w {
            Some(s) => {
                hs.push(s.hashrate_total_hps / 1000.0); // H/s → kH/s (the one divisor)
                acc = acc.saturating_add(s.shares_accepted);
                rej = rej.saturating_add(s.shares_rejected);
                uptime = uptime.max(s.uptime_secs);
                if ver.is_empty() && !s.version.is_empty() {
                    ver = s.version.clone();
                }
            }
            None => hs.push(0.0),
        }
    }
    if ver.is_empty() {
        ver = fallback_ver.to_string();
    }
    // Normalize -0.0 (the identity an empty sum can carry) to +0.0 so the JSON
    // reads "0.0", not "-0.0".
    let khs: f64 = hs.iter().sum();
    let khs = if khs == 0.0 { 0.0 } else { khs };

    HiveOutput {
        khs,
        stats: HiveStats {
            hs,
            hs_units: "khs",
            temp: Vec::new(),
            fan: Vec::new(),
            uptime,
            ar: [acc, rej],
            algo: "sha256d",
            ver,
            bus_numbers: Vec::new(),
        },
    }
}

/// GET `http://127.0.0.1:<port>/stats` and parse it. Returns `None` on any
/// connect/timeout/read/parse error — a non-answering worker is not fatal.
fn scrape(port: u16) -> Option<WorkerStats> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, SCRAPE_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(SCRAPE_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(SCRAPE_TIMEOUT)).ok()?;
    stream
        .write_all(b"GET /stats HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // Split headers from body on the blank line; the miner's stats server
    // replies with a `Connection: close` HTTP/1.1 response and a JSON body.
    let body = text.split("\r\n\r\n").nth(1)?;
    serde_json::from_str::<WorkerStats>(body.trim()).ok()
}

/// Probe worker slots and print the HiveOS JSON to stdout.
///
/// With `gpus = Some(n)` it scrapes exactly ports `base..base+n` (producing an
/// n-length `hs` array with `0.0` for any non-answering card). With `gpus =
/// None` it auto-probes contiguous ports from `base`, stopping after two
/// consecutive misses. Always prints a valid object, never errors out.
pub fn run(base_port: u16, gpus: Option<usize>) -> anyhow::Result<()> {
    let fallback_ver = env!("CARGO_PKG_VERSION");

    let workers: Vec<Option<WorkerStats>> = match gpus {
        Some(n) if n > 0 => (0..n)
            .map(|i| scrape(base_port.saturating_add(i as u16)))
            .collect(),
        _ => {
            // Unknown slot count: walk contiguous ports until two in a row are
            // silent (tolerates a single transient gap between live workers).
            let mut v: Vec<Option<WorkerStats>> = Vec::new();
            let mut misses = 0u8;
            let mut i: u16 = 0;
            while i < MAX_PROBE_SLOTS {
                match scrape(base_port.saturating_add(i)) {
                    Some(s) => {
                        v.push(Some(s));
                        misses = 0;
                    }
                    None => {
                        misses += 1;
                        if misses >= 2 {
                            break;
                        }
                        v.push(None);
                    }
                }
                i += 1;
            }
            // Drop the single trailing gap that preceded the second miss so an
            // all-silent probe yields hs:[] (the clean alive-zero), not [0.0].
            while matches!(v.last(), Some(None)) {
                v.pop();
            }
            v
        }
    };

    let out = aggregate(&workers, fallback_ver);
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(hps: f64, acc: u64, rej: u64, up: u64, ver: &str) -> WorkerStats {
        WorkerStats {
            hashrate_total_hps: hps,
            shares_accepted: acc,
            shares_rejected: rej,
            uptime_secs: up,
            version: ver.to_string(),
        }
    }

    #[test]
    fn aggregate_sums_and_converts_to_khs() {
        // Two GPUs at 2.0 GH/s and 1.0 GH/s → 2_000 and 1_000 kH/s.
        let workers = vec![
            Some(w(2_000_000_000.0, 10, 1, 100, "0.2.2")),
            Some(w(1_000_000_000.0, 5, 0, 250, "0.2.2")),
        ];
        let out = aggregate(&workers, "fallback");
        assert_eq!(out.stats.hs, vec![2_000_000.0, 1_000_000.0]);
        assert_eq!(out.khs, 3_000_000.0);
        assert_eq!(out.stats.ar, [15, 1]);
        assert_eq!(out.stats.uptime, 250); // max, not sum
        assert_eq!(out.stats.ver, "0.2.2");
        assert_eq!(out.stats.hs_units, "khs");
        assert_eq!(out.stats.algo, "sha256d");
    }

    #[test]
    fn dead_slot_shows_as_zero_not_dropped() {
        // Card 1 is down: hs must keep three entries (0.0 in the middle) so the
        // live cards stay aligned to their slot index.
        let workers = vec![
            Some(w(500_000_000.0, 3, 0, 60, "0.2.2")),
            None,
            Some(w(500_000_000.0, 2, 0, 90, "0.2.2")),
        ];
        let out = aggregate(&workers, "fallback");
        assert_eq!(out.stats.hs, vec![500_000.0, 0.0, 500_000.0]);
        assert_eq!(out.khs, 1_000_000.0);
        assert_eq!(out.stats.ar, [5, 0]);
        assert_eq!(out.stats.uptime, 90);
    }

    #[test]
    fn nothing_responding_is_valid_alive_zero() {
        let out = aggregate(&[], "0.2.2");
        assert_eq!(out.khs, 0.0);
        assert!(out.stats.hs.is_empty());
        assert_eq!(out.stats.ar, [0, 0]);
        assert_eq!(out.stats.uptime, 0);
        assert_eq!(out.stats.ver, "0.2.2"); // falls back to the binary version
        // And it serializes to the exact HiveOS-shaped keys.
        let j = serde_json::to_string(&out).unwrap();
        for key in ["\"khs\"", "\"hs\"", "\"hs_units\"", "\"ar\"", "\"uptime\"", "\"algo\"", "\"ver\"", "\"bus_numbers\""] {
            assert!(j.contains(key), "missing {key} in {j}");
        }
    }

    #[test]
    fn version_falls_back_when_workers_omit_it() {
        let workers = vec![Some(w(1_000.0, 0, 0, 1, ""))];
        let out = aggregate(&workers, "9.9.9");
        assert_eq!(out.stats.ver, "9.9.9");
    }

    /// End-to-end: `scrape()` must read the miner's REAL `/stats` server and
    /// deserialize the actual field names. This pins the stats contract — if a
    /// `/stats` field is renamed, this breaks (which is the whole point: the old
    /// HiveOS hook silently read `.hashrate_total` instead of
    /// `.hashrate_total_hps` and always reported 0).
    #[test]
    fn scrape_reads_the_live_stats_server() {
        use crate::stats::MinerStats;
        use std::sync::Arc;

        // Grab a free loopback port, then hand it to the stats server.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let stats = Arc::new(MinerStats::new());
        stats.set_hps(2_000_000.0, 500_000.0); // total 2.5 MH/s
        stats.on_share_accepted();
        stats.on_share_accepted();
        stats.on_share_rejected();
        crate::stats_server::spawn(stats.clone(), port);
        std::thread::sleep(Duration::from_millis(150)); // let it bind

        let got = scrape(port).expect("scrape must read the live /stats server");
        assert_eq!(got.hashrate_total_hps, 2_500_000.0);
        assert_eq!(got.shares_accepted, 2);
        assert_eq!(got.shares_rejected, 1);
        assert_eq!(got.version, env!("CARGO_PKG_VERSION"));

        // And the full aggregate over one live worker is well-formed.
        let out = aggregate(&[Some(got)], "fallback");
        assert_eq!(out.stats.hs, vec![2_500.0]); // kH/s
        assert_eq!(out.khs, 2_500.0);
    }
}
