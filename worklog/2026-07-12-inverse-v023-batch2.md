# 2026-07-12 - inverse (with Claude)

## Branch `fix/hiveos-and-engine-reliability` - second batch (6 commits, PUSHED for review)

Completes the v0.2.3 scope: everything the 07-11 deep-dive flagged as "must
pair with the engine P0s" plus the go-live plan's ride-alongs. On top of the
07-11 six commits (engine fault propagation, hiveos-stats, h-run rewrite,
CUDA-in-tarball):

1. `5f591de` launcher worker AUTO-RESTART: poll() respawns dead workers
   (same argv/stats port/log subdir), 2s..60s exponential backoff, deadline-
   checked on the UI thread (no sleeps), 5-min stability resets the streak,
   20 consecutive fails parks the worker ("failing repeatedly - check
   Logs"); rows show a restart counter. REQUIRED companion to the engine's
   exit-on-3-GPU-faults - without it a Windows card would exit and stay dead.
2. `7dc647e` GPU-hang watchdog: heartbeat armed strictly around hash_range;
   monitor thread exits(2) when an armed beat stalls >=120s while connected
   (wedged cuStreamSynchronize/clFinish class - the fault exit only helps
   when the call RETURNS). Pure gating fn + unit test; not spawned for cpu.
3. `b3ffd1e` reconnect polish: backoff now grows across endpoint rotations
   (all-endpoints-down no longer hammers at 1-2s); auth-reject is typed,
   loudly logged, and fatal after 3 consecutive (exit 3) - transport
   failures proven not to classify as auth-reject.
4. `866a968` mining.set_extranonce: session xn1/xn2-size re-key + immediate
   chunk-loop break, 3 unit tests. Kills the 100%-reject-behind-proxies gap.
5. `b9c2945` CI: tagged releases FAIL if the CUDA artifact is missing
   (HiveOS tarball + launcher embed can no longer silently ship CPU-only);
   installers (install.sh/.ps1/.bat, mine-auto.sh) become release assets
   covered by SHA256SUMS; standalone linux-opencl asset moved onto the
   zigbuild glibc-2.27 floor.
6. `8a1ca1a` installers: install.ps1 lowercases the address before its
   case-insensitive regex (uppercase paste no longer crash-loops); install.sh
   downloads the binary BEFORE requiring an address (newwallet hint now
   works), lowercases too, kills the `${VARIANT/cuda/cuda}` no-op.

Verification: cargo test --workspace green (68+16+18), hiveos hook suite ALL
PASSED, cuda/opencl/both feature checks compile, release build + selftest
PASS (507.7 MH/s cpu on this box). No pwsh here (ps1 not parse-checked); no
GPU here (watchdog paths unit-tested only).

## Watch on first tagged run / known follow-ups

- zigbuild opencl link (-L /usr/lib/x86_64-linux-gnu for the libOpenCL stub)
  is unverifiable locally; step stays continue-on-error - check the asset
  appears.
- Windows launcher BUILD step is still continue-on-error (pre-existing): a
  tagged release could lack launcher.exe. Consider gating it too.
- Supervisors treat exit 2 (watchdog) and exit 3 (auth-reject) identically;
  teaching mine-auto/h-run to halt on 3 is a nice follow-up.
- Deferred to v0.3.x per plan: GPU geometry auto-tune, OpenCL persistent
  pipeline, TLS, launcher self-update/code signing.

## Next steps (operator)

Review + merge to main, tag v0.2.3, CI, then real-rig validation (HiveOS
package on a vast.ai rig; launcher on the RTX 2080). Release reaches nobody
while the repo is private - D1 (visibility / public mirror / downloads page)
decides the launch.
