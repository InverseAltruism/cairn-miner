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

Download **`cairn-miner-launcher-windows.zip`** from the
[latest release](https://github.com/InverseAltruism/cairn-miner/releases/latest),
extract it anywhere, and run **`cairn-miner-launcher.exe`**. The native app lets
you set your address, pick a backend, **Start/Stop** mining, toggle
**start-on-login**, and watch live performance — hashrate graph, accepted/rejected
shares, difficulty, uptime. The miner runs headless behind it (all backends
bundled; `auto` picks CUDA → OpenCL → CPU).

> Double-clicking a bare `cairn-miner.exe` with no settings just exits — it needs
> your payout address. The launcher is the friendly way in; if you do run the
> miner from a terminal, pass `--address <your-addr20>`.

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

### No wallet yet?

```sh
cairn-miner newwallet     # generates an addr20 locally; the private key never leaves your machine
```

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
