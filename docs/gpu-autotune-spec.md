# GPU kernel geometry auto-tune — spec for a real-hardware implementation

Status: **spec / needs GPU hardware to implement + validate.** Author handoff to
mits (or anyone with NVIDIA/AMD rigs). Everything here is buildable headless
*except* the actual on-silicon timing, which must run on real cards.

## Why

Our GPU launch geometry is fixed at `--blocks 560 --threads-per-block 256
--nonces-per-thread 4096` (587M nonces/launch). That default is a reasonable
middle ground, but the optimal geometry is card-specific — SM count, occupancy,
and memory behaviour differ across Pascal→Blackwell. A competitor miner reports
**+10–17% on big cards** from a startup geometry sweep. This is the single
biggest raw-hashrate lever we have left; the kernels themselves are already well
tuned (2-stream pipelined, funnelshift ROTR, host-side midstate precompute,
precompiled PTX — see `src/kernels/sha256d.cu`).

## Correctness invariant (state and keep)

Launch geometry changes only **how the nonce space is partitioned across
threads** — it never changes the hash. So a re-tuned rig is bit-identical to the
default one. The existing `selftest` cross-check (`cairn-miner selftest`, every
backend vs the canonical CPU sha256d) still guards this and MUST pass after any
geometry change. No consensus, pool, or share-validity impact. This is a
performance-only change.

## Design

### 1. When
At startup, after backend init and **before** the mining loop begins accepting
work — mirrors the existing `--backend auto` probe timing in `src/main.rs`. One
`~Ns tuning...` log line so headless operators know the pause is expected.
Cap the whole sweep to ~30–50 s (competitor uses ~50 s); a hung candidate must
time out, not wedge the miner.

### 2. What to sweep
A bounded candidate grid of `(blocks, threads_per_block, nonces_per_thread)`.
Suggested bounds (tighten on real data):
- `threads_per_block ∈ {128, 256, 512}` (warp-multiple; 256 is the current default)
- `blocks ∈ {SM_count × k}` for `k ∈ {2,4,8,16}` — scale to the card, don't sweep absolute values blind
- `nonces_per_thread ∈ {2048, 4096, 8192}`
Prune combinations whose total nonces/launch fall outside a sane launch-duration
window (the existing chunk auto-tune already targets ~400 ms/launch — reuse that
target as the filter so candidates stay responsive).

### 3. How to measure
For each candidate: run a fixed **nonce budget** (not wall-clock) through
`MiningBackend::hash_range` against a synthetic max-difficulty template (never
finds — pure throughput), time it, compute nonces/sec. Pick the max. Discard a
candidate that errors. Take the median of 2–3 trials per candidate to damp noise.
**This timing loop is the piece that needs a real GPU** — everything around it is
plain code.

### 4. Persist per card
Cache the winner keyed by GPU identity (name + index from `cairn-miner devices
--json`, already implemented in `src/main.rs`/backends) under the config dir
(`~/.config/cairn-miner/autotune.toml` / `%APPDATA%\cairn-miner\`). On subsequent
starts, load the cached geometry and skip the full sweep (optionally a quick
single-candidate re-validate). Invalidate on driver/miner-version change or a
`--retune` flag. Format: one `[gpu."<name>"]` table per card with the three ints
+ the measured nonces/sec + a schema version.

### 5. Fallback & escape hatches
- On ANY sweep failure (timeout, error, no candidate beats the default): keep the
  `560×256×4096` default and log a warning. Never leave the miner unable to mine.
- `--auto-tune` / `--no-auto-tune` CLI flag + config key. **Recommended default:
  ON** (match the competitor; the +10–17% is free and the one-time pause is
  acceptable), but ship it OFF-by-default for the first release if we want a
  cautious rollout, then flip once field data confirms stability.
- Explicit `--blocks/--threads-per-block/--nonces-per-thread` on the CLI must
  **override** auto-tune (power users pin geometry).
- Surface the chosen geometry per card in `cairn-miner devices`.

## What is already scaffolded / can be written headless (no GPU)

I can land these now so mits only fills in the timing:
- the `--auto-tune`/`--no-auto-tune`/`--retune` flags + config keys (same pattern
  as `--no-suggest-difficulty` added in v0.2.6);
- the candidate-grid generator (pure, unit-testable) + the launch-duration filter
  reusing the chunk-target constant;
- the `autotune.toml` cache load/store + schema-version invalidation (pure,
  unit-testable);
- the fallback-to-default control flow and the `devices` surfacing;
- unit tests for the grid + cache round-trip.
The only GPU-gated piece is the per-candidate `hash_range` timing in step 3 and
the on-hardware validation (`selftest` bit-exactness + an accepted GPU share
after tuning) across Pascal→Blackwell (incl. sm_120).

## Acceptance (on real hardware)

1. `selftest` PASS after tuning (bit-exact — non-negotiable).
2. Measured nonces/sec with the tuned geometry ≥ the `560×256×4096` default on the
   same card (or it falls back to default, never regresses).
3. A tuned rig mines and lands accepted shares against the pool.
4. Cache hit on the second start (no re-sweep) and correct invalidation on
   `--retune`.
5. Total tuning time within the ~30–50 s cap; a deliberately-broken candidate
   times out without wedging the miner.
