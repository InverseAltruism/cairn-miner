# cairn-miner

An open GPU/CPU miner for the **Compute Substrate (CSD)** chain, over Stratum v1.

It mines to the [cairn pool](https://cairn-pool.com) by default, but
the pool is **not** compiled in: `--pool` points it anywhere, and you can list
several for failover. Your address is your account; there is no signup, no
phone-home, and **no bundled relay** that follows a remote blacklist.

```
  auto-detects CUDA -> OpenCL -> CPU     multi-endpoint --pool failover
  interleaved SHA-NI CPU path (~2.8x)    no dev fee, no telemetry, no relay
  Windows / Linux / HiveOS installers    your keys + address never leave the box
```

## Quick start

### Linux (one line)

```sh
curl -fsSL https://raw.githubusercontent.com/InverseAltruism/cairn-miner/main/install.sh | CAIRN_ADDR=<your-addr20> bash
```

Auto-detects your GPU, installs the matching build (or compiles from source if
no prebuilt release fits your arch), saves your address, and starts mining. Add
`--service` to install a systemd/user service, or run `mine-auto.sh` for a
self-updating launcher.

### Windows — the launcher (recommended)

Download the single file **`cairn-miner-launcher.exe`** from the
[latest release](https://github.com/InverseAltruism/cairn-miner/releases/latest)
and run it. That's the whole install — the miner is **embedded inside the
launcher**, so there's nothing else to download or unzip.

In the app you: choose a **mode** (GPU only / GPU + CPU / CPU only), tick the
**GPUs** to mine on (multi-GPU rigs show every card by name), set **CPU
intensity**, enter your address, then **Start/Stop**. It shows aggregated live
performance across every card — total hashrate graph, per-worker rows,
accepted/rejected shares, difficulty, uptime — and a **start-on-login** toggle.
Each GPU runs as its own worker under one window (no more one-console-per-card).

> Prefer the command line? A bare `cairn-miner.exe` needs `--address <addr20>`;
> with no address it just exits.

> **First run on Windows:** the binaries aren't code-signed yet, so Windows
> SmartScreen may show *"Windows protected your PC / unknown publisher."* Click
> **More info → Run anyway**. (Signing is on the roadmap; until then this one-time
> click is expected. You can confirm the download against `SHA256SUMS` on the
> release page if you want to verify it yourself.)

### Windows (scripted install)

Prefer a headless install/service instead of the GUI? Download **`install.bat`**
from the [latest release](https://github.com/InverseAltruism/cairn-miner/releases/latest)
and double-click it. It detects your GPU, downloads the right `cairn-miner.exe`,
asks for your address once, and starts mining. `install.bat` (via `install.ps1`)
also accepts `-Service` to run at logon.

### HiveOS

Flight sheet → Miner: **Custom** →
- Installation URL: `https://github.com/InverseAltruism/cairn-miner/releases/latest/download/cairn-miner-hiveos.tar.gz`
- Miner name in config: `cairn-miner`
- Hash algorithm: `sha256d`
- Wallet and worker template: `%WAL%`  (your addr20)
- Pool URL: `cairn-pool.com:3333`  (or leave blank for the default)
- Pass: `x`

That's it — standard fields, no dummy pool URL, no exact-name gotchas. Multi-GPU
rigs are handled per card.

> The HiveOS package ships the **NVIDIA/CUDA** build (the overwhelming majority of
> HiveOS rigs). It starts on any rig and, if no NVIDIA GPU is usable, falls back to
> CPU rather than crashing. **AMD HiveOS rigs**: the CUDA package will CPU-fall-back,
> so use the generic Linux OpenCL binary instead — `install.sh amd` on the rig, or
> the `cairn-miner-linux-opencl-x86_64` asset — not this package.

### No wallet yet?

```sh
cairn-miner newwallet     # generates an addr20 locally; the private key never leaves your machine
```

## Watch your rig (terminal dashboard)

A read-only live view of a running miner — hashrate, accepted/rejected/**stale**
shares + reject %, difficulty, uptime, reconnects, and GPU temp/power (when
`nvidia-smi` is present). The Windows launcher already shows all this in its GUI;
this is the equivalent for **headless HiveOS/Linux** (and a Windows terminal).

The miner must be running with a stats port (HiveOS sets `--stats-port 3380`
automatically; otherwise add it yourself). Then:

```sh
# Linux / macOS / HiveOS
curl -fsSLO https://github.com/InverseAltruism/cairn-miner/releases/latest/download/cairn-dashboard.sh
chmod +x cairn-dashboard.sh && ./cairn-dashboard.sh          # --port N, --refresh N, --once
```

```powershell
# Windows: download cairn-dashboard.bat (+ .ps1) from the latest release and run it.
```

Multi-GPU rigs are aggregated automatically (one worker per card on consecutive
ports). Press `q` or Ctrl-C to quit. Runs on a stock shell — no extra tools.

## Point it at any pool

```sh
cairn-miner --address <addr20> --pool your.pool.host:3333
cairn-miner --address <addr20> --pool a.pool:3333 --pool b.pool:3333   # failover
cairn-miner --address <addr20> --worker rig-01 --backend cuda
```

Config file (`~/.config/cairn-miner/config.toml`, or `%APPDATA%\cairn-miner\` on
Windows) — see `config.example.toml`:

```toml
address = "your40charaddr20..."
pool = ["cairn-pool.com:3333", "backup.example:3333"]
worker = "rig-01"
backend = "auto"     # auto | cpu | cuda | opencl
cpu_threads = 0      # GPU-only by default; raise on a desktop with headroom
```

## Backends

| Backend | Build | Notes |
|---|---|---|
| CPU     | default | interleaved SHA-NI batch path — ~2.8x a scalar hasher on Alder Lake (`cairn-miner bench` to measure yours) |
| CUDA    | `--features cuda`   | NVIDIA; ships a prebuilt PTX and JITs via the driver — no CUDA Toolkit needed at runtime |
| OpenCL  | `--features opencl` | AMD and other GPUs |

`cairn-miner devices` lists what it can see; `cairn-miner selftest` cross-checks
every backend against the reference sha256d.

## Build from source

```sh
cargo build --release                      # CPU
cargo build --release --features cuda      # + NVIDIA
cargo build --release --features opencl    # + AMD
```

Do **not** build with `RUSTFLAGS=-C target-cpu=native` — it disables the
hand-written SHA-NI path.

## License

MIT OR Apache-2.0 (see `LICENSE-MIT` and `LICENSE-APACHE`). "Compute Substrate"
and "CSD" are used only to state chain compatibility.
