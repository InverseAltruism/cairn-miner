//! Minimal loopback HTTP server exposing [`MinerStats`] as JSON.
//!
//! The native launcher spawns the miner with `--stats-port <p>` and polls
//! `http://127.0.0.1:<p>/stats` a couple of times a second to drive its live
//! dashboard. This is deliberately tiny and dependency-free: it hand-writes a
//! single HTTP/1.1 response per connection using `std::net`, so the miner
//! doesn't grow an HTTP-server dependency.
//!
//! Security: it binds `127.0.0.1` ONLY — never a routable interface. It exposes
//! read-only telemetry (no addresses, keys, or controls), but keeping it on
//! loopback means nothing on the network can reach it regardless.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use crate::stats::MinerStats;

/// Spawn the stats server on `127.0.0.1:<port>` in a background thread.
///
/// A bind failure (e.g. port already in use) is logged and otherwise ignored —
/// the miner keeps mining without a stats endpoint rather than refusing to
/// start. Returns immediately; the thread runs for the life of the process.
pub fn spawn(stats: Arc<MinerStats>, port: u16) {
    std::thread::Builder::new()
        .name("stats-server".into())
        .spawn(move || match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                tracing::info!("stats server listening on http://127.0.0.1:{port}/stats");
                serve(listener, stats);
            }
            Err(e) => {
                tracing::warn!("stats server: could not bind 127.0.0.1:{port} ({e}); disabled");
            }
        })
        .ok();
}

fn serve(listener: TcpListener, stats: Arc<MinerStats>) {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                // One request per connection; a slow/dead client must not wedge
                // the accept loop, so cap the read.
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .ok();
                if let Err(e) = handle(stream, &stats) {
                    tracing::debug!("stats server: connection error: {e}");
                }
            }
            Err(e) => tracing::debug!("stats server: accept error: {e}"),
        }
    }
}

fn handle(mut stream: TcpStream, stats: &Arc<MinerStats>) -> std::io::Result<()> {
    // Read just enough to see the request line. We don't care about headers or
    // bodies; browsers/pollers send the method + path first.
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    if path.starts_with("/stats") || path == "/" {
        let body = serde_json::to_string(&stats.snapshot())
            .unwrap_or_else(|_| "{}".to_string());
        write_response(
            &mut stream,
            "200 OK",
            "application/json",
            &body,
        )
    } else {
        write_response(&mut stream, "404 Not Found", "text/plain", "not found")
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    // `Access-Control-Allow-Origin: *` so a local browser dashboard (or the
    // pool page) could also read it; harmless on a loopback-only socket.
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}
