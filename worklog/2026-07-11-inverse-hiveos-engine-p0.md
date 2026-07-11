# cairn-miner P0: HiveOS rescue + engine reliability — plan & worklog

Branch: `fix/hiveos-and-engine-reliability` (off `origin/main` = v0.2.2, `1a9698d`).
Author: inverse (Claude). Status: IN PROGRESS. **Not pushed** — hold for review + mits coordination (repo is shared + private).

Source of findings: `/opt/cairn_substrate/MINER-DEEP-DIVE-2026-07-11.md` (13-agent verified audit, 35 confirmed findings).

## Scope (this branch)
Two engine P0s + the HiveOS rescue. Deliberately OUT of scope (follow-up branch): `set_extranonce`
(P1 correctness), GPU geometry auto-tune (P1 perf), OpenCL persistent pipeline (P2), launcher worker
auto-restart (P1), NVML temps/`bus_numbers` (P2), address-normalization unification across installers
(P1), TLS (gated on pool). Kept out to keep this diff reviewable and each change sound.

## A. Engine P0-A — GPU errors are swallowed (`backend.rs:24`)
`MiningBackend::hash_range` returns `Option<MiningResult>`; every CUDA/OpenCL runtime failure returns
`None`, indistinguishable from "swept, found nothing" → a dead GPU mines nothing forever, the process
never exits, and no supervisor can rescue it.

**Fix:** change the trait to return `anyhow::Result<HashOutcome>` where
`HashOutcome { result: Option<MiningResult>, nonces_done: u64 }`.
- Backends turn every device/driver/mutex error into `Err(..)` instead of `None`.
- The mining loop counts consecutive `Err`s; after `MAX_CONSECUTIVE_GPU_ERRORS = 3` it `bail!`s out of
  `run_stratum` → `main` exits non-zero → systemd `Restart=always` / `mine-auto.sh` / HiveOS agent
  restart the process. A single transient error = warn + 500 ms backoff, no hashrate credited.

## B. Engine P0-B — phantom hashrate (`loop_stratum.rs:394`)
`gpu_swept = gpu_range.1 - gpu_range.0` credits the FULL requested range even when the GPU did less
(errored / stopped early) → a dead card reports an absurd fake rate on the dashboard.

**Fix:** credit `HashOutcome.nonces_done` (nonces the backend ACTUALLY hashed). Each backend reports it:
CPU sums a per-thread counter; CUDA/OpenCL report `next_start - nonce_start` at return.

Touch list for A+B: `src/backend.rs` (new `HashOutcome`, trait sig), `src/backends/{cpu,cuda,opencl}.rs`
(impls), `src/selftest.rs:245` (call site), `src/stratum/loop_stratum.rs` (call site ~382-394 + the
`NullGpu` test mock ~656 + the `.hash_range().is_none()` assert ~750).

## C. HiveOS rescue
1. **Drop `--stats-bind`** (`hiveos/h-run.sh`) — not a real flag → clap exit-2 crash-loop today. (P0)
2. **Ship a GPU binary, not the CPU seed** (`release.yml` + `packaging/build-hiveos-package.sh`) — build
   an all-backends (cuda+opencl) or cuda Linux binary against the glibc-2.27 floor and package THAT,
   CPU fallback only if the GPU build failed. (P0)
3. **`cairn-miner hiveos-stats` subcommand** (new `src/hiveos.rs` + `main.rs` + `lib.rs`) — scrapes the
   per-GPU `/stats` loopback ports and emits `{ "khs": <total>, "stats": <HiveOS-shaped JSON> }`
   (hs[] per-GPU kH/s, ar=[acc,rej], uptime=max, algo, ver=CARGO_PKG_VERSION); prints a valid
   alive-zero object on any error. All the arithmetic + the /1000 divisor lives here (unit-tested),
   retiring the 4 shell-contract bugs (wrong port env, `/summary` vs `/stats`, `.hashrate_total` vs
   `.hashrate_total_hps`, share-count grep). `h-stats.sh` becomes a thin wrapper. (P0)
4. **Per-GPU spawn, brick-safe + supervised** (`hiveos/h-run.sh`, `hiveos/h-stop.sh`) — detect GPU count
   (`nvidia-smi -L`), background-launch a restart-supervised worker per card (`--device i
   --stats-port BASE+i`) recorded in a pidfile, then `exec` device 0 (satisfies HiveOS's exec-rename
   contract). Any detection failure OR an explicit `--device`/`--backend cpu` in Extra args → single
   exec (never a brick). `h-stop.sh` kills the supervisor pidfile first (stops respawn) then the miners.
5. **Lowercase the address** in `hiveos/h-config.sh` (miner hard-rejects uppercase → crash-loop today).
6. **Bump `CUSTOM_VERSION`** in `hiveos/h-manifest.conf` (stale 0.1.0) + stamp it from the tag in CI.

## STATUS: implemented + tested on branch (4 commits), NOT pushed. Adversarial review in progress.

Commits (on `fix/hiveos-and-engine-reliability`, base 1a9698d):
- `48cbeee` fix(engine): surface GPU faults + honest hashrate (P0)
- `d45f38a` feat(hiveos): hiveos-stats subcommand aggregates per-GPU stats (P0)
- `1bd9b63` fix(hiveos): drop --stats-bind, per-GPU spawn, real stats + lowercase addr (P0)
- `1d828d2` fix(ci): ship a GPU binary (not the CPU seed) in the HiveOS tarball (P0)

Verification done: `cargo test` 76 green (incl. new `run_stratum_exits_after_repeated_gpu_errors`
+ a live `/stats` scrape test pinning the field-name contract + hiveos aggregate math);
`cargo run -- selftest` byte-identical (CPU 48 MH/s); `cargo check` clean for default, `--features
cuda`, `--features opencl`, and combined `cuda,opencl`; `hiveos/test-hiveos-hooks.sh` all pass
(per-GPU spawn on ports 3380..3382, no --stats-bind, address lowercased, clean stop + no respawn);
packaging verified locally (tarball layout + version stamp). Could NOT bench real GPU hardware here.

## Test plan (executed — see STATUS above)
- `cargo test` green after A+B and after C-3 (add unit tests for `hiveos` aggregation).
- `cargo build --release` (CPU) green; `cargo build --release --features cuda`/`opencl` attempted
  locally (may need GPU libs; CI is the source of truth — note if it can't build here).
- `cairn-miner selftest` still passes (byte-identical CPU/GPU vs reference).
- Shell: drive `h-run.sh`/`h-stats.sh`/`h-stop.sh`/`h-config.sh` in a temp dir against a fake
  `cairn-miner` stub (no GPU) — assert: no `--stats-bind`, N workers spawned, stats aggregate correct,
  stop reaps everything and nothing respawns, uppercase address lowercased, single-GPU fallback.
- Adversarial code review of the full diff before finalizing.
