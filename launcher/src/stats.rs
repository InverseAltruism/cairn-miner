//! Client for the miner's loopback stats endpoint (`--stats-port`).
//!
//! The response is a single small JSON object over HTTP/1.0 with
//! `Connection: close`, so we hand-roll the GET with `std::net` instead of
//! pulling in an async HTTP stack.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde::Deserialize;

/// Mirror of the miner's `stats::StatsSnapshot`. Field names must stay in sync
/// (there's a serialization-key test on the miner side).
#[derive(Deserialize, Clone, Debug, Default)]
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
    pub last_share_age_secs: Option<u64>,
    pub pool: String,
    pub worker: String,
    pub backend: String,
    pub version: String,
}

impl StatsSnapshot {
    /// Rejected shares as a percentage of all acked shares (0 if none yet).
    pub fn reject_pct(&self) -> f64 {
        let total = self.shares_accepted + self.shares_rejected;
        if total == 0 {
            0.0
        } else {
            100.0 * self.shares_rejected as f64 / total as f64
        }
    }
}

/// Poll `http://127.0.0.1:<port>/stats`, returning the parsed snapshot or
/// `None` if the miner isn't up yet / the request failed. Never blocks longer
/// than ~1.5s so the UI thread stays responsive.
pub fn fetch(port: u16) -> Option<StatsSnapshot> {
    let addr = ("127.0.0.1", port)
        .to_socket_addrs()
        .ok()?
        .next()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(1000))).ok();
    stream.set_write_timeout(Some(Duration::from_millis(500))).ok();

    stream
        .write_all(b"GET /stats HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .ok()?;

    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok()?;
    let body = http_body(&raw)?;
    serde_json::from_str(body).ok()
}

/// Return the body of an HTTP response (everything after the header terminator).
fn http_body(response: &str) -> Option<&str> {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .or_else(|| response.split_once("\n\n").map(|(_, body)| body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_body_after_headers() {
        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"connected\":true}";
        assert_eq!(http_body(resp), Some("{\"connected\":true}"));
    }

    #[test]
    fn deserializes_a_full_snapshot() {
        let json = r#"{"connected":true,"uptime_secs":10,"difficulty":8.0,
            "hashrate_gpu_hps":1200000.0,"hashrate_cpu_hps":0.0,"hashrate_total_hps":1200000.0,
            "shares_submitted":5,"shares_accepted":4,"shares_rejected":1,
            "last_share_age_secs":3,"pool":"cairn-pool.com:3333","worker":"rig",
            "backend":"cuda","version":"0.1.0"}"#;
        let s: StatsSnapshot = serde_json::from_str(json).unwrap();
        assert!(s.connected);
        assert_eq!(s.shares_accepted, 4);
        assert_eq!(s.backend, "cuda");
        assert!((s.reject_pct() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn reject_pct_zero_when_no_shares() {
        assert_eq!(StatsSnapshot::default().reject_pct(), 0.0);
    }
}
