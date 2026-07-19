//! Live Stratum v1 client: TCP connect, subscribe/authorize handshake, and a
//! background reader thread that keeps the latest pushed job and the current
//! share difficulty up to date.
//!
//! This is the **protocol/transport** layer only. The reader thread parses
//! `mining.notify` into a [`StratumJob`] (the raw 9-tuple + the session
//! extranonce1) and stashes `mining.set_difficulty` /
//! `mining.set_extranonce` — it does NOT build a
//! [`crate::csd_consensus::WorkTemplate`]; the mapping step does that.
//!
//! Concurrency model:
//!   - The write half of the socket lives behind a `Mutex<TcpStream>` so the
//!     mining thread (`send_submit`) and reconnect path can serialize writes.
//!   - The reader thread owns a `BufReader` over a *cloned* `TcpStream` handle
//!     (a dup of the same OS socket) and loops over `\n`-delimited lines.
//!   - Latest job + difficulty are shared via `Arc<Mutex<..>>` and read by the
//!     mining thread through [`StratumClient::latest_job`] /
//!     [`StratumClient::current_difficulty`].
//!   - On read error/timeout the reader logs and reconnects with capped
//!     backoff, re-running the subscribe/authorize handshake. It never panics.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use super::protocol::{
    authorize_request, serialize_line, subscribe_request, submit_request,
    suggest_difficulty_request, NotifyParams, Notification, Response, SubscribeResult,
};
use crate::stats::MinerStats;

/// How long to wait on `connect()` for the TCP three-way handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Read timeout on the socket so the reader thread can't block forever waiting
/// for a frame from a wedged bridge. ~120s comfortably exceeds the bridge's
/// notify/difficulty cadence, so a timeout means the link is actually dead.
const READ_TIMEOUT: Duration = Duration::from_secs(120);
/// Reconnect backoff bounds.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// A handshake-level authorization rejection: the pool answered our
/// `mining.authorize` and said no. Typed (rather than a bare `anyhow!`) so the
/// reconnect loop can tell it apart from transport failures — a bad payout
/// address never fixes itself, so hammering/rotating forever is wrong there,
/// while a dead pool very much wants the retry loop.
#[derive(Debug)]
struct AuthRejected {
    worker: String,
    detail: String,
}

impl std::fmt::Display for AuthRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pool rejected authorization for {}{}", self.worker, self.detail)
    }
}

impl std::error::Error for AuthRejected {}

/// A job pushed by the pool via `mining.notify`, paired with the session
/// `extranonce1` captured at subscribe time. The notify→header mapping is
/// Task 3; this is the raw material that mapping will consume.
#[derive(Clone, Debug)]
pub struct StratumJob {
    pub notify: NotifyParams,
    pub extranonce1_hex: String,
}

/// A response to one of our `mining.submit` requests: `{id, result, error}`.
/// The pool acks each submitted share with `result:true` (accepted) or
/// `false`/an error (rejected/stale). Submit ids are >= 100 (see `next_id`),
/// which distinguishes them from the id=1/2 handshake replies.
#[derive(serde::Deserialize)]
struct SubmitAck {
    id: Option<u64>,
    result: Option<bool>,
    /// Pool error for a rejected share, e.g. `[21, "Job not found", null]`.
    /// `null` on acceptance or when the pool omits it.
    #[serde(default)]
    error: serde_json::Value,
}

/// Shared state the reader thread writes and the mining thread reads.
struct Shared {
    /// Latest pushed job, if any has arrived yet.
    latest_job: Mutex<Option<StratumJob>>,
    /// Current share difficulty (from `mining.set_difficulty`), bit-encoded as
    /// an `f64` in an `AtomicU64` so reads are lock-free. Defaults to 1.0 until
    /// the pool sends its first `set_difficulty`.
    difficulty_bits: AtomicU64,
    /// Session extranonce1 (hex) captured at subscribe time; refreshed on each
    /// reconnect (the bridge may hand out a new one). Behind a Mutex because a
    /// reconnect mutates it while submits read it.
    extranonce1_hex: Mutex<String>,
    /// extranonce2 byte width from the subscribe result (the bridge sends 4).
    extranonce2_size: AtomicU64,
    /// Difficulty to send via `mining.suggest_difficulty` after authorize,
    /// bit-encoded as an `f64` in an `AtomicU64`; `0` means "not measured yet, do
    /// not suggest". Set once the mining loop has a real hashrate reading (see
    /// `StratumClient::suggest_difficulty`) so every subsequent reconnect
    /// handshake re-sends it and fast rigs skip the vardiff ramp.
    suggested_diff_bits: AtomicU64,
    /// Set on shutdown so the reader loop exits instead of reconnecting.
    shutdown: AtomicBool,
    /// Live telemetry shared with the mining loop and the loopback stats server.
    /// The reader thread updates connection state, difficulty and accepted/
    /// rejected share counts here as frames arrive.
    stats: Arc<MinerStats>,
}

impl Shared {
    fn set_difficulty(&self, d: f64) {
        self.difficulty_bits.store(d.to_bits(), Ordering::Relaxed);
        self.stats.set_difficulty(d);
    }
    fn difficulty(&self) -> f64 {
        f64::from_bits(self.difficulty_bits.load(Ordering::Relaxed))
    }
    /// Cache the difficulty to hint via `mining.suggest_difficulty` on future
    /// (re)connect handshakes. Ignores non-positive/non-finite values.
    fn set_suggested_diff(&self, d: f64) {
        if d.is_finite() && d > 0.0 {
            self.suggested_diff_bits.store(d.to_bits(), Ordering::Relaxed);
        }
    }
    /// The cached suggestion, or `None` if none has been measured yet.
    fn suggested_diff(&self) -> Option<f64> {
        let bits = self.suggested_diff_bits.load(Ordering::Relaxed);
        if bits == 0 {
            return None;
        }
        let d = f64::from_bits(bits);
        (d.is_finite() && d > 0.0).then_some(d)
    }
}

/// Extract the numeric reject code from a Stratum error payload. The pool sends
/// `[code, "message", data]` (e.g. `[21, "Job not found", null]`) on a rejected
/// `mining.submit`; code 21 is the conventional "stale/job-not-found". Returns
/// `None` when the error is absent or not in that shape.
fn stratum_reject_code(error: &serde_json::Value) -> Option<i64> {
    error.as_array()?.first()?.as_i64()
}

/// Convert a measured hashrate (hashes/sec) into a Stratum share difficulty that
/// targets roughly `target_secs` seconds per share. A share at difficulty `D`
/// takes ~`D * 2^32` hashes, so `D = hps * target_secs / 2^32`. Returns `None`
/// for a garbage reading (non-finite or <= 0). The result is capped to
/// `[1.0, SUGGEST_DIFF_CAP]` as client-side defense in depth — the pool also
/// clamps to its own vardiff bounds, so a bad benchmark can never request an
/// unsolvable difficulty.
pub fn suggested_difficulty_from_hps(hps: f64, target_secs: f64) -> Option<f64> {
    if !hps.is_finite() || hps <= 0.0 || !target_secs.is_finite() || target_secs <= 0.0 {
        return None;
    }
    let d = hps * target_secs / 4_294_967_296.0; // 2^32
    if !d.is_finite() || d <= 0.0 {
        return None;
    }
    Some(d.clamp(1.0, SUGGEST_DIFF_CAP))
}

/// Upper bound for a client difficulty hint (defense in depth; the pool clamps
/// to its own vardiff max regardless).
const SUGGEST_DIFF_CAP: f64 = 4_000_000.0;

/// Seconds-per-share the startup hint aims for (matches the pool's vardiff
/// target so the pool settles near the hint instead of retargeting away).
pub const SUGGEST_TARGET_SECS: f64 = 12.0;

/// A connected Stratum v1 client.
pub struct StratumClient {
    /// All pool endpoints provided at startup (primary first). The reader
    /// thread walks this list during reconnect: a few failures on the current
    /// endpoint advance `endpoint_idx` round-robin; a successful connect resets
    /// it to 0 (primary).
    endpoints: Vec<String>,
    /// Index into `endpoints` of the endpoint we last connected to (or are
    /// currently trying). Protected by the reconnect loop (single writer).
    endpoint_idx: Arc<AtomicUsize>,
    worker_addr: String,
    shared: Arc<Shared>,
    /// Write half of the socket. The reader thread holds its own cloned read
    /// handle; this one is used by `send_submit` and the reconnect path.
    writer: Arc<Mutex<TcpStream>>,
    /// Monotonic JSON-RPC request id source for outgoing submits.
    next_id: AtomicU64,
    /// Reader-thread handle. `Option` so `Drop` can `join()` after signalling
    /// shutdown.
    reader: Option<JoinHandle<()>>,
}

/// Result of one handshake attempt.
///
/// We hand back the *buffered reader* the handshake used (not a fresh clone)
/// so any server pushes the bridge sent immediately after the authorize reply
/// — and which `BufReader` may have already pulled into its internal buffer —
/// are preserved for the reader thread instead of being silently dropped with
/// the buffer. `write_stream` is a separate clone of the same socket for the
/// writer mutex.
struct Handshake {
    reader: BufReader<TcpStream>,
    write_stream: TcpStream,
    subscribe: SubscribeResult,
    /// Push frames (notify/set_difficulty) the bridge bunched in BEFORE the
    /// authorize reply, consumed by the handshake loop. Replayed through
    /// `dispatch_frame` once `Shared` exists so the first job/difficulty isn't
    /// lost. (Pushes that arrive AFTER the authorize reply stay buffered in
    /// `reader` and are handled by the reader thread directly.)
    early_pushes: Vec<String>,
}

impl StratumClient {
    /// Connect to `endpoint` (`host:port`), run the subscribe + authorize
    /// handshake, capture `extranonce1`/`extranonce2_size`, confirm authorize
    /// returned `true` (bail otherwise), then spawn the reader thread.
    pub fn connect(endpoint: &str, worker_addr: &str) -> Result<Self> {
        Self::connect_with_stats(endpoint, worker_addr, Arc::new(MinerStats::new()))
    }

    /// Like [`connect`](Self::connect) but shares an existing [`MinerStats`] so
    /// the mining loop and the loopback stats server observe the same live
    /// counters this client updates.
    pub fn connect_with_stats(
        endpoint: &str,
        worker_addr: &str,
        stats: Arc<MinerStats>,
    ) -> Result<Self> {
        Self::connect_with_stats_and_endpoints(
            endpoint,
            worker_addr,
            stats,
            vec![endpoint.to_string()],
        )
    }

    /// Full constructor: like [`connect_with_stats`](Self::connect_with_stats)
    /// but accepts the complete ordered failover list so the reader's reconnect
    /// loop can rotate through all endpoints rather than hammering a single one.
    /// `endpoint` is the initial (primary) endpoint to connect to first;
    /// `endpoints` is the full ordered list (primary first).
    pub fn connect_with_stats_and_endpoints(
        endpoint: &str,
        worker_addr: &str,
        stats: Arc<MinerStats>,
        endpoints: Vec<String>,
    ) -> Result<Self> {
        // First connect: no hashrate measured yet, so no difficulty hint. The
        // mining loop measures it and calls `suggest_difficulty()`, which also
        // caches it for every subsequent reconnect handshake.
        let hs = Self::handshake(endpoint, worker_addr, None)
            .with_context(|| format!("stratum handshake to {endpoint}"))?;

        let shared = Arc::new(Shared {
            latest_job: Mutex::new(None),
            difficulty_bits: AtomicU64::new(1.0f64.to_bits()),
            extranonce1_hex: Mutex::new(hs.subscribe.extranonce1_hex.clone()),
            extranonce2_size: AtomicU64::new(hs.subscribe.extranonce2_size as u64),
            suggested_diff_bits: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            stats,
        });
        // Handshake succeeded, so we're live from the caller's point of view.
        shared.stats.set_connected(true);

        let extranonce1 = hs.subscribe.extranonce1_hex.clone();
        let xn2_size = hs.subscribe.extranonce2_size;

        // The reader thread takes the handshake's buffered reader directly (so
        // early pushes already in its buffer aren't lost); the writer mutex
        // takes the separate write-side clone of the same socket.
        let writer = Arc::new(Mutex::new(hs.write_stream));

        // Replay pushes the bridge sent before the authorize reply (consumed by
        // the handshake loop). Done now that `shared` exists and before the
        // reader thread starts, so the first job/difficulty is available
        // immediately instead of waiting for the next push.
        for push in &hs.early_pushes {
            dispatch_frame(push, &shared);
        }

        // The initial connection is to endpoints[0] by construction (main.rs
        // walks the list and calls us with the first endpoint that answered).
        // Record which index that corresponds to so the reconnect loop starts
        // from the right position.
        let initial_idx = endpoints
            .iter()
            .position(|e| e == endpoint)
            .unwrap_or(0);
        let endpoint_idx = Arc::new(AtomicUsize::new(initial_idx));

        let reader = Self::spawn_reader(
            endpoints.clone(),
            Arc::clone(&endpoint_idx),
            worker_addr.to_string(),
            hs.reader,
            Arc::clone(&shared),
            Arc::clone(&writer),
        );

        tracing::info!(
            "stratum: connected to {endpoint} (extranonce1={extranonce1}, xn2_size={xn2_size})"
        );

        Ok(StratumClient {
            endpoints,
            endpoint_idx,
            worker_addr: worker_addr.to_string(),
            shared,
            writer,
            next_id: AtomicU64::new(100),
            reader: Some(reader),
        })
    }

    /// One full TCP connect + subscribe + authorize round. Used by both the
    /// initial `connect()` and the reader's reconnect path. Leaves the read
    /// timeout set on the returned stream so subsequent reads can't hang.
    fn handshake(
        endpoint: &str,
        worker_addr: &str,
        suggest_diff: Option<f64>,
    ) -> Result<Handshake> {
        let addr = endpoint
            .to_socket_addrs()
            .with_context(|| format!("resolving stratum endpoint {endpoint}"))?
            .next()
            .ok_or_else(|| anyhow!("no socket address resolved for {endpoint}"))?;

        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .with_context(|| format!("connecting to {endpoint}"))?;
        stream.set_read_timeout(Some(READ_TIMEOUT)).ok();
        stream.set_nodelay(true).ok();

        // A buffered reader over a cloned handle for the synchronous handshake
        // replies. We deliberately hand this same reader to the persistent
        // reader thread afterwards so no early server pushes that landed in its
        // buffer are lost. `writer` is a second clone used for the writer mutex.
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .context("cloning stream for handshake reader")?,
        );
        let mut writer = stream
            .try_clone()
            .context("cloning stream for handshake writer")?;
        // `stream` itself is dropped at end of scope; the two clones above keep
        // the OS socket alive (one for reading, one for writing).

        // --- mining.subscribe ---
        let sub_req = subscribe_request(1);
        writer
            .write_all(serialize_line(&sub_req)?.as_bytes())
            .context("sending mining.subscribe")?;
        writer.flush().ok();

        // --- mining.authorize ---
        let auth_req = authorize_request(2, worker_addr);
        writer
            .write_all(serialize_line(&auth_req)?.as_bytes())
            .context("sending mining.authorize")?;
        writer.flush().ok();

        // --- mining.suggest_difficulty (optional) ---
        // A client hint so the pool starts us near our real hashrate instead of
        // ramping vardiff up from its minimum. `None` on the very first connect
        // (hashrate not measured yet); populated from Shared on every reconnect.
        if let Some(d) = suggest_diff {
            let sug = suggest_difficulty_request(d);
            // Best-effort: a failure here must not abort an otherwise-good
            // handshake — the pool would just start us on its default ramp.
            if let Ok(line) = serialize_line(&sug) {
                let _ = writer.write_all(line.as_bytes());
                writer.flush().ok();
            }
        }

        // Read frames until we've captured both the subscribe result and the
        // authorize result. The bridge may interleave a notify/set_difficulty
        // push before the authorize reply, so match on `id` rather than order.
        let mut subscribe: Option<SubscribeResult> = None;
        let mut authorized: Option<bool> = None;
        // Pushes the bridge interleaves before the authorize reply. We must NOT
        // drop them: the real bridge sends the first set_difficulty + notify
        // right after the subscribe result, i.e. before the id=2 reply we are
        // still waiting for here. Captured raw and replayed via `dispatch_frame`
        // once `Shared` exists (see connect/reconnect), so the first job isn't
        // lost (which manifested as the miner hanging on "waiting for first
        // mining.notify").
        let mut early_pushes: Vec<String> = Vec::new();

        let mut line = String::new();
        while subscribe.is_none() || authorized.is_none() {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .context("reading stratum handshake reply")?;
            if n == 0 {
                return Err(anyhow!("connection closed during handshake"));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Responses to our calls carry our id (1 = subscribe, 2 =
            // authorize). Pushes (notify/set_difficulty) have id:null and a
            // `method`; capture them for replay rather than dropping them.
            let resp: Response = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(_) => {
                    early_pushes.push(trimmed.to_string()); // a push frame, not a response
                    continue;
                }
            };
            match resp.id {
                Some(1) => {
                    if let Some(err) = &resp.error {
                        if !err.is_null() {
                            return Err(anyhow!("mining.subscribe failed: {err}"));
                        }
                    }
                    subscribe = Some(SubscribeResult::parse(&resp.result)?);
                }
                Some(2) => {
                    if let Some(err) = &resp.error {
                        if !err.is_null() {
                            // The pool answered and refused — handshake-level
                            // rejection, not a transport error.
                            return Err(AuthRejected {
                                worker: worker_addr.to_string(),
                                detail: format!(": {err}"),
                            }
                            .into());
                        }
                    }
                    authorized = Some(resp.result.as_bool().unwrap_or(false));
                }
                _ => {
                    early_pushes.push(trimmed.to_string()); // id:null push interleaved in
                    continue;
                }
            }
        }

        let subscribe = subscribe.expect("loop exits only once Some");
        if authorized != Some(true) {
            return Err(AuthRejected {
                worker: worker_addr.to_string(),
                detail: " (mining.authorize returned false)".to_string(),
            }
            .into());
        }

        Ok(Handshake {
            reader,
            write_stream: writer,
            subscribe,
            early_pushes,
        })
    }

    /// Spawn the background reader thread. It loops over `\n`-delimited frames,
    /// dispatching `mining.notify` → latest_job and `mining.set_difficulty` →
    /// difficulty. On any read error/timeout/EOF it reconnects with capped
    /// backoff (re-subscribe/re-authorize) unless shutdown was signalled.
    ///
    /// `endpoints` is the full ordered failover list; `endpoint_idx` tracks
    /// which one we are currently on so the reconnect loop can rotate forward.
    fn spawn_reader(
        endpoints: Vec<String>,
        endpoint_idx: Arc<AtomicUsize>,
        worker_addr: String,
        initial_reader: BufReader<TcpStream>,
        shared: Arc<Shared>,
        writer: Arc<Mutex<TcpStream>>,
    ) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("stratum-reader".to_string())
            .spawn(move || {
                let mut reader = initial_reader;
                let mut backoff = BACKOFF_MIN;
                let mut line = String::new();

                loop {
                    if shared.shutdown.load(Ordering::Relaxed) {
                        return;
                    }

                    line.clear();
                    let read = reader.read_line(&mut line);
                    let needs_reconnect = match read {
                        Ok(0) => {
                            // Clean EOF: peer closed the connection.
                            if shared.shutdown.load(Ordering::Relaxed) {
                                return;
                            }
                            let ep = &endpoints[endpoint_idx.load(Ordering::Relaxed)];
                            tracing::warn!(
                                "stratum: connection closed by {ep}, reconnecting"
                            );
                            true
                        }
                        Ok(_) => {
                            dispatch_frame(line.trim(), &shared);
                            backoff = BACKOFF_MIN; // healthy read resets backoff
                            false
                        }
                        Err(e) => {
                            if shared.shutdown.load(Ordering::Relaxed) {
                                return;
                            }
                            let ep = &endpoints[endpoint_idx.load(Ordering::Relaxed)];
                            tracing::warn!(
                                "stratum: read error from {ep}: {e}; reconnecting"
                            );
                            true
                        }
                    };

                    if needs_reconnect
                        && !reconnect(
                            &endpoints,
                            &endpoint_idx,
                            &worker_addr,
                            &shared,
                            &writer,
                            &mut reader,
                            &mut backoff,
                        )
                    {
                        return; // shutdown requested mid-reconnect
                    }
                }
            })
            .expect("spawning stratum reader thread")
    }

    /// Latest job pushed by the pool, or `None` if none has arrived yet.
    pub fn latest_job(&self) -> Option<StratumJob> {
        self.shared
            .latest_job
            .lock()
            .ok()
            .and_then(|g| g.clone())
    }

    /// Current share difficulty (defaults to 1.0 until the pool sends one).
    pub fn current_difficulty(&self) -> f64 {
        self.shared.difficulty()
    }

    /// Handle to the shared live telemetry this client updates. The mining loop
    /// clones it to publish hashrate; the stats server reads it.
    pub fn stats(&self) -> Arc<MinerStats> {
        Arc::clone(&self.shared.stats)
    }

    /// Session extranonce1 (hex), refreshed on reconnect.
    pub fn extranonce1_hex(&self) -> String {
        self.shared
            .extranonce1_hex
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// extranonce2 byte width the bridge expects (4).
    pub fn extranonce2_size(&self) -> usize {
        self.shared.extranonce2_size.load(Ordering::Relaxed) as usize
    }

    /// The pool endpoint this client is currently connected to.
    pub fn endpoint(&self) -> &str {
        &self.endpoints[self.endpoint_idx.load(Ordering::Relaxed)]
    }

    /// The worker (csd1) address this client authorized as.
    pub fn worker_addr(&self) -> &str {
        &self.worker_addr
    }

    /// Send a `mining.submit` line for a found share. Serializes writes through
    /// the writer mutex. Errors bubble up so the caller can log/account them.
    pub fn send_submit(
        &self,
        worker: &str,
        job_id: &str,
        xn2_hex: &str,
        ntime_hex: &str,
        nonce_hex: &str,
    ) -> Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = submit_request(id, worker, job_id, xn2_hex, ntime_hex, nonce_hex);
        let line = serialize_line(&req)?;
        let mut w = self
            .writer
            .lock()
            .map_err(|_| anyhow!("stratum writer mutex poisoned"))?;
        w.write_all(line.as_bytes())
            .context("writing mining.submit")?;
        w.flush().ok();
        // Count the share as submitted; the pool's async accept/reject ack is
        // tallied later by the reader thread in `dispatch_frame`.
        self.shared.stats.on_share_submitted();
        Ok(())
    }

    /// Hint the pool at our real difficulty via `mining.suggest_difficulty`.
    /// Called once by the mining loop after it has a genuine hashrate reading:
    /// caches the value so every future reconnect handshake re-sends it (no more
    /// vardiff ramp after a drop), and sends it now on the live connection so the
    /// current session benefits too. Best-effort — a write error is ignored (the
    /// pool just keeps us on its default vardiff). Non-finite/<=0 is dropped.
    pub fn suggest_difficulty(&self, difficulty: f64) {
        if !difficulty.is_finite() || difficulty <= 0.0 {
            return;
        }
        self.shared.set_suggested_diff(difficulty);
        let req = suggest_difficulty_request(difficulty);
        if let Ok(line) = serialize_line(&req) {
            if let Ok(mut w) = self.writer.lock() {
                let _ = w.write_all(line.as_bytes());
                let _ = w.flush();
            }
        }
        tracing::info!(
            "stratum: suggested start difficulty {:.0} (from measured hashrate)",
            difficulty
        );
    }
}

impl Drop for StratumClient {
    fn drop(&mut self) {
        // Signal the reader to stop and nudge the socket so a blocked
        // `read_line` returns promptly, then join.
        self.shared.shutdown.store(true, Ordering::Relaxed);
        if let Ok(w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

/// Parse one received line and update [`Shared`] accordingly. Recognizes the
/// three server pushes we care about (`mining.notify`, `mining.set_difficulty`,
/// `mining.set_extranonce`) and silently ignores everything else (submit acks,
/// keep-alives, blank lines, unparseable junk) — a malformed push must never
/// take the reader down.
///
/// Kept as a free function taking `&Shared` so it is unit-testable without a
/// socket: feed it a canned line and assert the resulting state.
fn dispatch_frame(line: &str, shared: &Shared) {
    if line.is_empty() {
        return;
    }
    let note: Notification = match serde_json::from_str(line) {
        Ok(n) => n,
        // Not a notification. It may be the pool's ack to one of our
        // `mining.submit`s (`{id,result,error}`, id >= 100) — tally accepted vs
        // rejected shares from it. Anything else (junk, keep-alives) is ignored.
        Err(_) => {
            if let Ok(ack) = serde_json::from_str::<SubmitAck>(line) {
                if ack.id.map(|id| id >= 100).unwrap_or(false) {
                    match ack.result {
                        Some(true) => shared.stats.on_share_accepted(),
                        _ => {
                            // Stratum reject code 21 ("stale"/"job not found") is
                            // valid work that lost the tip race — count it apart
                            // from real rejects so a rig can tell "too slow"
                            // (stale) from "wrong" (rejected). `on_share_stale`
                            // still bumps the rejected total.
                            let is_stale = stratum_reject_code(&ack.error) == Some(21);
                            if is_stale {
                                shared.stats.on_share_stale();
                            } else {
                                shared.stats.on_share_rejected();
                            }
                            // Emit a visible warning so headless operators
                            // (HiveOS/systemd) see rejects in the log rather
                            // than only in the counters. Include the pool's
                            // error payload when present.
                            let err_detail = if ack.error.is_null() {
                                String::new()
                            } else {
                                format!(" (pool error: {})", ack.error)
                            };
                            let snap = shared.stats.snapshot();
                            let label = if is_stale { "STALE" } else { "REJECTED" };
                            tracing::warn!(
                                "stratum: share {label}{err_detail} \
                                 [accepted={} rejected={} stale={}]",
                                snap.shares_accepted, snap.shares_rejected, snap.shares_stale
                            );
                        }
                    }
                }
            }
            return;
        }
    };

    match note.method.as_str() {
        "mining.notify" => match NotifyParams::parse(&note.params) {
            Ok(notify) => {
                let extranonce1_hex = shared
                    .extranonce1_hex
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let job_id = notify.job_id.clone();
                let clean = notify.clean_jobs;
                if let Ok(mut slot) = shared.latest_job.lock() {
                    *slot = Some(StratumJob {
                        notify,
                        extranonce1_hex,
                    });
                }
                tracing::debug!("stratum: new job {job_id} (clean_jobs={clean})");
            }
            Err(e) => tracing::warn!("stratum: bad mining.notify, ignoring: {e}"),
        },
        "mining.set_difficulty" => {
            match note
                .params
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_f64())
            {
                Some(d) => {
                    shared.set_difficulty(d);
                    tracing::debug!("stratum: set_difficulty = {d}");
                }
                None => tracing::warn!(
                    "stratum: bad mining.set_difficulty params, ignoring: {}",
                    note.params
                ),
            }
        }
        // Session re-key: `[extranonce1_hex, extranonce2_size]`. Our pool never
        // sends this mid-session, but Stratum proxies do when they re-key a
        // downstream connection — dropping it would make every share we build
        // from the stale xn1 invalid until the next reconnect.
        "mining.set_extranonce" => {
            let arr = note.params.as_array();
            let xn1 = arr
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty() && hex::decode(s).is_ok());
            match xn1 {
                Some(xn1) => {
                    // Second param (xn2 byte width) is optional on the wire;
                    // keep the subscribe-time size when absent or non-numeric.
                    let xn2_size = arr.and_then(|a| a.get(1)).and_then(|v| v.as_u64());
                    if let Ok(mut g) = shared.extranonce1_hex.lock() {
                        *g = xn1.to_string();
                    }
                    if let Some(sz) = xn2_size {
                        shared.extranonce2_size.store(sz, Ordering::Relaxed);
                    }
                    // Re-key the stored job too: the mining loop compares the
                    // job's extranonce1 against its own and rebuilds the
                    // coinbase bases on mismatch (same path as a job change).
                    if let Ok(mut slot) = shared.latest_job.lock() {
                        if let Some(job) = slot.as_mut() {
                            job.extranonce1_hex = xn1.to_string();
                        }
                    }
                    tracing::info!(
                        "stratum: set_extranonce xn1={xn1} xn2_size={}",
                        xn2_size.map_or_else(|| "(kept)".into(), |s| s.to_string()),
                    );
                }
                None => tracing::warn!(
                    "stratum: bad mining.set_extranonce params, ignoring: {}",
                    note.params
                ),
            }
        }
        // client.reconnect, client.show_message, etc.: ignore.
        _ => {}
    }
}

/// How many consecutive failures on one endpoint before we rotate to the next.
/// Two attempts gives the current endpoint a fair chance to recover from a
/// brief restart before we give up on it.
const ENDPOINT_TRIES_BEFORE_ROTATE: u32 = 2;

/// How many consecutive authorize rejections before the process exits. A pool
/// that answers and says "no" three times in a row means the payout address is
/// wrong — retrying forever just hammers the pool while the operator sees a
/// rig that looks merely "reconnecting".
const MAX_CONSECUTIVE_AUTH_REJECTS: u32 = 3;

/// Distinct exit code for "the pool rejected our worker address" so
/// supervisors/operators can tell it apart from a generic crash.
const EXIT_CODE_AUTH_REJECTED: i32 = 3;

/// Reconnect with capped exponential backoff and jitter: sleep, re-run the
/// handshake, and on success swap in the fresh write half + reader stream and
/// refresh the session extranonce1/size. Returns `false` iff shutdown was
/// requested (so the reader loop should exit); `true` once reconnected.
///
/// **Endpoint rotation**: after [`ENDPOINT_TRIES_BEFORE_ROTATE`] consecutive
/// failures on the current endpoint the function advances `endpoint_idx`
/// round-robin to the next one in `endpoints`. Rotation deliberately does NOT
/// reset the backoff — only a SUCCESSFUL connect does — so when every endpoint
/// is down the sleep keeps climbing toward [`BACKOFF_MAX`] across rotation
/// cycles instead of hammering the whole list at [`BACKOFF_MIN`] forever.
/// A successful connect resets the index to 0 (primary) and the backoff.
///
/// **Auth rejection**: a handshake that fails with [`AuthRejected`] (the pool
/// answered `mining.authorize` with an error/false — NOT a transport failure)
/// is loudly logged, and after [`MAX_CONSECUTIVE_AUTH_REJECTS`] in a row the
/// process exits with [`EXIT_CODE_AUTH_REJECTED`]: a wrong payout address
/// never fixes itself, so restart-loop visibility beats silent hammering. Any
/// transport failure resets that counter.
///
/// **Jitter**: each backoff sleep is multiplied by a random factor in
/// `0.5..1.5` drawn from `rand::thread_rng()`. This breaks the thundering-herd
/// of many miners reconnecting in sync after a pool restart: their sleeps
/// spread out by up to ±50 % instead of all firing simultaneously.
///
/// The backoff doubles each failed attempt up to [`BACKOFF_MAX`]; a successful
/// reconnect resets it (the caller also resets on the next healthy read).
fn reconnect(
    endpoints: &[String],
    endpoint_idx: &Arc<AtomicUsize>,
    worker_addr: &str,
    shared: &Arc<Shared>,
    writer: &Arc<Mutex<TcpStream>>,
    reader: &mut BufReader<TcpStream>,
    backoff: &mut Duration,
) -> bool {
    use rand::Rng;

    // We're between sockets until the handshake below succeeds.
    shared.stats.set_connected(false);

    // Consecutive failures against the current endpoint; triggers rotation.
    let mut tries_on_current: u32 = 0;
    // Consecutive handshake-level authorize rejections (across endpoints);
    // reset by any transport failure or success. See MAX_CONSECUTIVE_AUTH_REJECTS.
    let mut consecutive_auth_rejects: u32 = 0;

    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            return false;
        }

        // Apply jitter: multiply the base backoff by a random factor in 0.5..1.5
        // so reconnecting miners don't all wake simultaneously after a pool restart.
        let jitter: f64 = rand::thread_rng().gen_range(0.5..1.5);
        let sleep_dur = backoff.mul_f64(jitter).min(BACKOFF_MAX);
        std::thread::sleep(sleep_dur);

        if shared.shutdown.load(Ordering::Relaxed) {
            return false;
        }

        let idx = endpoint_idx.load(Ordering::Relaxed);
        let endpoint = &endpoints[idx];

        match StratumClient::handshake(endpoint, worker_addr, shared.suggested_diff()) {
            Ok(hs) => {
                // Refresh session params the bridge may have rotated.
                if let Ok(mut x) = shared.extranonce1_hex.lock() {
                    *x = hs.subscribe.extranonce1_hex.clone();
                }
                shared
                    .extranonce2_size
                    .store(hs.subscribe.extranonce2_size as u64, Ordering::Relaxed);

                // Replay pushes bunched in before the authorize reply (consumed
                // by the handshake), so a reconnect re-seeds the current
                // job/difficulty immediately too.
                for push in &hs.early_pushes {
                    dispatch_frame(push, shared);
                }

                // Install the new socket: the reader continues from the fresh
                // handshake's buffered reader (preserving any early pushes), and
                // the shared writer is swapped to the new write-side stream.
                *reader = hs.reader;
                if let Ok(mut w) = writer.lock() {
                    // Best-effort close of the dead socket before swap.
                    let _ = w.shutdown(std::net::Shutdown::Both);
                    *w = hs.write_stream;
                }

                // Successful connect: reset to the primary endpoint and clear backoff.
                endpoint_idx.store(0, Ordering::Relaxed);
                *backoff = BACKOFF_MIN;
                shared.stats.set_connected(true);
                shared.stats.on_reconnect();
                tracing::info!("stratum: reconnected to {endpoint}");
                return true;
            }
            Err(e) => {
                if e.downcast_ref::<AuthRejected>().is_some() {
                    // The pool is alive and said NO. Loud + actionable: this is
                    // almost always a typo'd/uppercase payout address, and no
                    // amount of retrying fixes it.
                    consecutive_auth_rejects += 1;
                    tracing::error!(
                        "stratum: {e:#} ({consecutive_auth_rejects}/{MAX_CONSECUTIVE_AUTH_REJECTS}) \
                         — check your payout address"
                    );
                    if consecutive_auth_rejects >= MAX_CONSECUTIVE_AUTH_REJECTS {
                        tracing::error!(
                            "stratum: pool rejected worker {worker_addr} \
                             {MAX_CONSECUTIVE_AUTH_REJECTS} times in a row — the payout \
                             address is almost certainly wrong; exiting (code \
                             {EXIT_CODE_AUTH_REJECTED}) instead of hammering the pool"
                        );
                        std::process::exit(EXIT_CODE_AUTH_REJECTED);
                    }
                } else {
                    // Transport failure (dead pool, DNS, timeout): says nothing
                    // about the address, so the auth-reject streak resets.
                    consecutive_auth_rejects = 0;
                    tracing::warn!("stratum: reconnect to {endpoint} failed: {e}");
                }
                tries_on_current += 1;

                // After a couple of failures, rotate to the next endpoint so a
                // dead pool doesn't block us from the backup indefinitely. The
                // backoff is NOT reset here (only a successful connect resets
                // it): with every endpoint down, a reset per rotation kept the
                // loop hammering the whole list at BACKOFF_MIN forever.
                if tries_on_current >= ENDPOINT_TRIES_BEFORE_ROTATE && endpoints.len() > 1 {
                    let next = (idx + 1) % endpoints.len();
                    endpoint_idx.store(next, Ordering::Relaxed);
                    tries_on_current = 0;
                    tracing::info!(
                        "stratum: rotating to next endpoint {} ({}/{})",
                        endpoints[next],
                        next + 1,
                        endpoints.len(),
                    );
                }
            }
        }

        // Bump backoff, capped — on every failed attempt, rotation included, so
        // a fully-dark endpoint list climbs toward BACKOFF_MAX.
        *backoff = (*backoff * 2).min(BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn suggested_difficulty_maps_hashrate_to_target() {
        // ~10 GH/s at a 12s target ⇒ D = 10e9*12/2^32 ≈ 27.9.
        let d = suggested_difficulty_from_hps(10e9, 12.0).unwrap();
        assert!((d - 27.9).abs() < 0.5, "got {d}");
        // A modest CPU rig (~20 MH/s) floors at the min difficulty 1.0.
        assert_eq!(suggested_difficulty_from_hps(20e6, 12.0).unwrap(), 1.0);
    }

    #[test]
    fn suggested_difficulty_rejects_and_caps_garbage() {
        // Non-finite / non-positive readings produce no hint.
        assert!(suggested_difficulty_from_hps(f64::NAN, 12.0).is_none());
        assert!(suggested_difficulty_from_hps(f64::INFINITY, 12.0).is_none());
        assert!(suggested_difficulty_from_hps(0.0, 12.0).is_none());
        assert!(suggested_difficulty_from_hps(-5.0, 12.0).is_none());
        assert!(suggested_difficulty_from_hps(10e9, 0.0).is_none());
        // An absurd reading is capped, never unbounded (defense in depth vs the
        // pool's own clamp), so it can't request an unsolvable difficulty.
        let capped = suggested_difficulty_from_hps(1e30, 12.0).unwrap();
        assert_eq!(capped, SUGGEST_DIFF_CAP);
    }

    #[test]
    fn reject_code_extracts_21_for_stale() {
        assert_eq!(
            stratum_reject_code(&serde_json::json!([21, "Job not found", null])),
            Some(21)
        );
        assert_eq!(
            stratum_reject_code(&serde_json::json!([23, "Low difficulty", null])),
            Some(23)
        );
        assert_eq!(stratum_reject_code(&serde_json::Value::Null), None);
        assert_eq!(stratum_reject_code(&serde_json::json!("oops")), None);
        assert_eq!(stratum_reject_code(&serde_json::json!([])), None);
    }

    #[test]
    fn shared_suggested_diff_roundtrip() {
        let s = fresh_shared("11223344", 4);
        assert_eq!(s.suggested_diff(), None); // unset by default
        s.set_suggested_diff(64.0);
        assert_eq!(s.suggested_diff(), Some(64.0));
        s.set_suggested_diff(f64::NAN); // ignored, keeps prior value
        assert_eq!(s.suggested_diff(), Some(64.0));
    }

    /// Build a fresh `Shared` with defaults, as `connect()` would.
    fn fresh_shared(xn1: &str, xn2_size: u64) -> Arc<Shared> {
        Arc::new(Shared {
            latest_job: Mutex::new(None),
            difficulty_bits: AtomicU64::new(1.0f64.to_bits()),
            extranonce1_hex: Mutex::new(xn1.to_string()),
            extranonce2_size: AtomicU64::new(xn2_size),
            suggested_diff_bits: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            stats: Arc::new(MinerStats::new()),
        })
    }

    #[test]
    fn dispatch_notify_updates_latest_job() {
        let shared = fresh_shared("cafef00d", 4);
        let line = r#"{"id":null,"method":"mining.notify","params":["jX","00ff","aa","bb",["cc"],"01000000","1d00ffff","60c0babe",true]}"#;
        dispatch_frame(line, &shared);
        let job = shared.latest_job.lock().unwrap().clone().unwrap();
        assert_eq!(job.notify.job_id, "jX");
        assert_eq!(job.notify.ntime_hex, "60c0babe");
        // extranonce1 is stitched in from the session at dispatch time.
        assert_eq!(job.extranonce1_hex, "cafef00d");
    }

    #[test]
    fn dispatch_set_difficulty_updates_difficulty() {
        let shared = fresh_shared("00", 4);
        assert_eq!(shared.difficulty(), 1.0); // default before any push
        let line = r#"{"id":null,"method":"mining.set_difficulty","params":[2048.5]}"#;
        dispatch_frame(line, &shared);
        assert_eq!(shared.difficulty(), 2048.5);
    }

    #[test]
    fn dispatch_ignores_junk_and_unknown_methods() {
        let shared = fresh_shared("00", 4);
        // None of these should panic or mutate state.
        dispatch_frame("", &shared);
        dispatch_frame("not json at all", &shared);
        dispatch_frame(r#"{"id":7,"result":true,"error":null}"#, &shared); // submit ack
        dispatch_frame(
            r#"{"id":null,"method":"client.show_message","params":["hi"]}"#,
            &shared,
        );
        assert!(shared.latest_job.lock().unwrap().is_none());
        assert_eq!(shared.difficulty(), 1.0);
    }

    #[test]
    fn dispatch_set_extranonce_rekeys_session_and_pending_job() {
        let shared = fresh_shared("cafef00d", 4);
        // Seed a job so we can verify it gets re-keyed in place.
        dispatch_frame(
            r#"{"id":null,"method":"mining.notify","params":["j1","00ff","aa","bb",["cc"],"01000000","1d00ffff","60c0babe",true]}"#,
            &shared,
        );
        dispatch_frame(
            r#"{"id":null,"method":"mining.set_extranonce","params":["ab12cd34",8]}"#,
            &shared,
        );
        // Session xn1 + xn2 size updated…
        assert_eq!(shared.extranonce1_hex.lock().unwrap().clone(), "ab12cd34");
        assert_eq!(shared.extranonce2_size.load(Ordering::Relaxed), 8);
        // …and the stored job carries the new xn1 so the mining loop rebuilds
        // its coinbase bases (job-change path) instead of mining stale work.
        let job = shared.latest_job.lock().unwrap().clone().unwrap();
        assert_eq!(job.extranonce1_hex, "ab12cd34");
        assert_eq!(job.notify.job_id, "j1");
    }

    #[test]
    fn dispatch_set_extranonce_without_size_keeps_current_size() {
        let shared = fresh_shared("cafef00d", 4);
        // Single-param form (some proxies omit the size): xn1 updates, size kept.
        dispatch_frame(
            r#"{"id":null,"method":"mining.set_extranonce","params":["deadbeef"]}"#,
            &shared,
        );
        assert_eq!(shared.extranonce1_hex.lock().unwrap().clone(), "deadbeef");
        assert_eq!(shared.extranonce2_size.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn dispatch_set_extranonce_rejects_bad_params() {
        let shared = fresh_shared("cafef00d", 4);
        // Non-hex, empty, wrong-typed and missing params must all be ignored
        // (never clobber the session xn1 with garbage).
        for line in [
            r#"{"id":null,"method":"mining.set_extranonce","params":["not-hex",4]}"#,
            r#"{"id":null,"method":"mining.set_extranonce","params":["",4]}"#,
            r#"{"id":null,"method":"mining.set_extranonce","params":[42,4]}"#,
            r#"{"id":null,"method":"mining.set_extranonce","params":[]}"#,
            r#"{"id":null,"method":"mining.set_extranonce","params":{}}"#,
        ] {
            dispatch_frame(line, &shared);
        }
        assert_eq!(shared.extranonce1_hex.lock().unwrap().clone(), "cafef00d");
        assert_eq!(shared.extranonce2_size.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn dispatch_bad_notify_does_not_clobber_state() {
        let shared = fresh_shared("00", 4);
        // First a good job, then a malformed notify (wrong arity) — the good
        // job must survive (we don't overwrite with garbage).
        dispatch_frame(
            r#"{"id":null,"method":"mining.notify","params":["good","p","a","b",[],"01000000","1d00ffff","60c0babe",true]}"#,
            &shared,
        );
        dispatch_frame(
            r#"{"id":null,"method":"mining.notify","params":["bad","p","a"]}"#,
            &shared,
        );
        let job = shared.latest_job.lock().unwrap().clone().unwrap();
        assert_eq!(job.notify.job_id, "good");
    }

    /// Drive one `handshake()` against a fake bridge that replies to subscribe
    /// normally and answers the authorize (id=2) with `reply`. Returns the
    /// handshake error for classification asserts.
    fn handshake_err_with_authorize_reply(reply: &'static [u8]) -> anyhow::Error {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut br = BufReader::new(sock.try_clone().unwrap());
            let mut line = String::new();
            br.read_line(&mut line).unwrap(); // subscribe
            line.clear();
            br.read_line(&mut line).unwrap(); // authorize
            sock.write_all(
                b"{\"id\":1,\"result\":[[[\"mining.notify\",\"1\"]],\"abcd1234\",4],\"error\":null}\n",
            )
            .unwrap();
            sock.write_all(reply).unwrap();
            sock.flush().unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let err = match StratumClient::handshake(&addr.to_string(), "csd1badaddr", None) {
            Ok(_) => panic!("handshake must fail on an authorize rejection"),
            Err(e) => e,
        };
        let _ = server.join();
        err
    }

    #[test]
    fn authorize_false_and_error_classify_as_auth_rejected() {
        // Both rejection shapes — result:false and an error triple — must
        // surface as the typed AuthRejected so the reconnect loop can go fatal
        // instead of hammering the pool forever.
        for reply in [
            b"{\"id\":2,\"result\":false,\"error\":null}\n".as_slice(),
            b"{\"id\":2,\"result\":null,\"error\":[24,\"Invalid worker address\",null]}\n".as_slice(),
        ] {
            let err = handshake_err_with_authorize_reply(reply);
            assert!(
                err.downcast_ref::<AuthRejected>().is_some(),
                "expected AuthRejected, got: {err:#}"
            );
        }
    }

    #[test]
    fn transport_failure_is_not_classified_as_auth_rejected() {
        // Connect to a port that just closed: a pure transport failure must
        // NOT count toward the fatal auth-reject streak.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().unwrap();
        drop(listener); // port now refuses connections
        let err = match StratumClient::handshake(&addr.to_string(), "csd1worker", None) {
            Ok(_) => panic!("connect to a closed port must fail"),
            Err(e) => e,
        };
        assert!(err.downcast_ref::<AuthRejected>().is_none());
    }

    /// End-to-end-ish smoke test against a localhost listener that plays the
    /// bridge: it accepts one connection, replies to subscribe + authorize, and
    /// pushes one set_difficulty + one notify. Asserts `connect()` completes
    /// the handshake and the reader surfaces the job + difficulty. Kept tightly
    /// scoped and self-contained so it stays reliable in CI.
    #[test]
    fn connect_handshake_and_first_job_against_fake_bridge() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut br = BufReader::new(sock.try_clone().unwrap());

            // Read the two handshake requests (subscribe id=1, authorize id=2).
            let mut req_line = String::new();
            br.read_line(&mut req_line).unwrap(); // subscribe
            req_line.clear();
            br.read_line(&mut req_line).unwrap(); // authorize

            // Reply: subscribe result (xn1="abcd1234", xn2_size=4), then
            // authorize true, then a set_difficulty and a notify push.
            sock.write_all(
                b"{\"id\":1,\"result\":[[[\"mining.notify\",\"1\"]],\"abcd1234\",4],\"error\":null}\n",
            )
            .unwrap();
            sock.write_all(b"{\"id\":2,\"result\":true,\"error\":null}\n")
                .unwrap();
            sock.write_all(b"{\"id\":null,\"method\":\"mining.set_difficulty\",\"params\":[512.0]}\n")
                .unwrap();
            sock.write_all(
                b"{\"id\":null,\"method\":\"mining.notify\",\"params\":[\"fakejob\",\"00ff\",\"aa\",\"bb\",[\"cc\"],\"01000000\",\"1d00ffff\",\"60c0babe\",true]}\n",
            )
            .unwrap();
            sock.flush().unwrap();

            // Hold the connection open briefly so the client reader can consume
            // the pushes before EOF would trigger a reconnect attempt.
            std::thread::sleep(Duration::from_millis(300));
        });

        let client =
            StratumClient::connect(&addr.to_string(), "csd1testaddr").expect("connect ok");
        assert_eq!(client.extranonce1_hex(), "abcd1234");
        assert_eq!(client.extranonce2_size(), 4);

        // Poll briefly for the pushed job/difficulty to land (reader is async).
        let mut job = None;
        for _ in 0..50 {
            if let Some(j) = client.latest_job() {
                job = Some(j);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let job = job.expect("reader surfaced the pushed job");
        assert_eq!(job.notify.job_id, "fakejob");
        assert_eq!(job.extranonce1_hex, "abcd1234");
        assert_eq!(client.current_difficulty(), 512.0);

        drop(client); // triggers reader shutdown + join
        let _ = server.join();
    }

    /// Regression test for the "waiting for first mining.notify" hang.
    ///
    /// The real bridge bunches `subscribe-result -> set_difficulty -> notify`
    /// and sends the first notify BEFORE the authorize reply. The handshake
    /// loop reads those pushes while still waiting for the id=2 response, so it
    /// must CAPTURE them (not discard) — otherwise the first job is lost and the
    /// miner waits forever for a notify that already arrived.
    #[test]
    fn connect_captures_early_notify_sent_before_authorize_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut br = BufReader::new(sock.try_clone().unwrap());
            let mut req_line = String::new();
            br.read_line(&mut req_line).unwrap(); // subscribe
            req_line.clear();
            br.read_line(&mut req_line).unwrap(); // authorize

            // Real-bridge order: subscribe result, THEN set_difficulty + notify,
            // THEN the authorize reply LAST. The pushes land while the handshake
            // loop is still waiting for the id=2 authorize response.
            sock.write_all(
                b"{\"id\":1,\"result\":[[[\"mining.notify\",\"1\"]],\"abcd1234\",4],\"error\":null}\n",
            )
            .unwrap();
            sock.write_all(b"{\"id\":null,\"method\":\"mining.set_difficulty\",\"params\":[512.0]}\n")
                .unwrap();
            sock.write_all(
                b"{\"id\":null,\"method\":\"mining.notify\",\"params\":[\"earlyjob\",\"00ff\",\"aa\",\"bb\",[\"cc\"],\"01000000\",\"1d00ffff\",\"60c0babe\",true]}\n",
            )
            .unwrap();
            sock.write_all(b"{\"id\":2,\"result\":true,\"error\":null}\n")
                .unwrap();
            sock.flush().unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });

        let client =
            StratumClient::connect(&addr.to_string(), "csd1testaddr").expect("connect ok");

        // The early notify (sent before the authorize reply) must be surfaced.
        let mut job = None;
        for _ in 0..50 {
            if let Some(j) = client.latest_job() {
                job = Some(j);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let job = job.expect("early notify (pre-authorize) must be surfaced, not discarded");
        assert_eq!(job.notify.job_id, "earlyjob");
        assert_eq!(client.current_difficulty(), 512.0);

        drop(client);
        let _ = server.join();
    }
}
