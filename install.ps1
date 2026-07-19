# cairn-miner - Windows one-click installer (PowerShell).
#
# Double-click install.bat (which calls this), or run:
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Address <addr20>
#
# Detects your GPU, downloads the matching cairn-miner.exe from GitHub Releases,
# saves your payout address + config, and starts mining. Optionally installs a
# Windows service with -Service.
param(
  [string]$Address = $env:CAIRN_ADDR,
  [string]$Pool    = $env:CAIRN_POOL,
  [string]$Worker  = $env:CAIRN_WORKER,
  [ValidateSet("","auto","cpu","cuda","opencl")][string]$Backend = $env:CAIRN_BACKEND,
  [switch]$Service,
  [switch]$NoRun
)
$ErrorActionPreference = "Stop"
$Repo = "InverseAltruism/cairn-miner"
$DefaultPool = "cairn-pool.com:3333"

function Grn($m){ Write-Host $m -ForegroundColor Green }
function Die($m){ Write-Host "[x] $m" -ForegroundColor Red; exit 1 }

Grn "  === cairn-miner installer (Windows) ==="
$dataDir = Join-Path $env:LOCALAPPDATA "cairn-miner"
$cfgDir  = Join-Path $env:APPDATA "cairn-miner"
New-Item -ItemType Directory -Force $dataDir,$cfgDir | Out-Null
$bin = Join-Path $dataDir "cairn-miner.exe"
$cfg = Join-Path $cfgDir "config.toml"

# 1. address
if (-not $Address -and (Test-Path $cfg)) {
  $m = Select-String -Path $cfg -Pattern 'address\s*=\s*"([0-9a-fx]+)"' | Select-Object -First 1
  if ($m) { $Address = $m.Matches[0].Groups[1].Value }
}
if (-not $Address) { $Address = Read-Host "  your CSD payout address (addr20, 40 hex)" }
$Address = $Address -replace '^0x',''
# The miner hard-rejects uppercase hex (crash-loop), and PowerShell -match is
# case-INSENSITIVE so uppercase would slip through the check below. Normalize
# first, exactly like hiveos/h-config.sh does.
$Address = $Address.ToLower()
if ($Address -notmatch '^[0-9a-f]{40}$') { Die "address must be 40 hex chars (create one: cairn-miner.exe newwallet)" }

# 2. backend
if (-not $Backend -or $Backend -eq "auto") {
  $gpu = (Get-CimInstance Win32_VideoController).Name -join ','
  if     ($gpu -match 'NVIDIA')          { $Backend = "cuda" }
  elseif ($gpu -match 'AMD|Radeon')      { $Backend = "opencl" }
  else                                   { $Backend = "cpu" }
}
Write-Host "  backend:  $Backend"
Write-Host "  address:  $Address"
Write-Host ("  pool:     " + $(if($Pool){$Pool}else{$DefaultPool}))

# 3. download the matching prebuilt exe + verify it against the release SHA256SUMS
$asset    = "cairn-miner-windows-$Backend.exe"
$url      = "https://github.com/$Repo/releases/latest/download/$asset"
$sumsUrl  = "https://github.com/$Repo/releases/latest/download/SHA256SUMS"
try {
  Write-Host "  downloading $asset ..."
  Invoke-WebRequest -Uri $url -OutFile "$bin.tmp" -UseBasicParsing

  # FAIL CLOSED: fetch SHA256SUMS, find this asset's line, compare. Never keep an
  # unverified binary. Mirrors install.sh / mine-auto.sh.
  $sumsTmp = "$bin.sums.tmp"
  Invoke-WebRequest -Uri $sumsUrl -OutFile $sumsTmp -UseBasicParsing
  $want = (Select-String -Path $sumsTmp -Pattern ("[0-9a-fA-F]{64}\s+\*?" + [regex]::Escape($asset) + "$") |
           Select-Object -First 1).Line -replace '\s.*$',''
  Remove-Item -Force $sumsTmp -ErrorAction SilentlyContinue
  $got = (Get-FileHash -Algorithm SHA256 "$bin.tmp").Hash
  if (-not $want -or ($want.ToLower() -ne $got.ToLower())) {
    Remove-Item -Force "$bin.tmp" -ErrorAction SilentlyContinue
    Die "checksum verification FAILED for $asset (want=$want got=$got). Not installing an unverified binary."
  }
  Move-Item -Force "$bin.tmp" $bin
  Grn "  [ok] installed $asset (sha256 verified)"
} catch {
  Remove-Item -Force "$bin.tmp" -ErrorAction SilentlyContinue
  Die "could not download/verify $asset (no published release yet for this backend, or verification failed). Try -Backend cpu, or build from source: cargo build --release"
}

# Microsoft VC++ runtime (GPU builds need it); install via winget if missing.
if ($Backend -ne "cpu") {
  try { winget install --id Microsoft.VCRedist.2015+.x64 -e --silent --accept-source-agreements --accept-package-agreements 2>$null } catch {}
}

# 4. config
$lines = @("# cairn-miner config (written by install.ps1)","address = `"$Address`"")
if ($Pool)   { $lines += "pool = `"$Pool`"" }
if ($Worker) { $lines += "worker = `"$Worker`"" }
$lines += "backend = `"$Backend`""
$lines += "cpu_threads = 0   # GPU-only by default"
Set-Content -Path $cfg -Value $lines -Encoding UTF8
Write-Host "  config:   $cfg"

$runArgs = @("--config", $cfg, "--log-dir", (Join-Path $dataDir "logs"))

# 5. service or run
if ($Service) {
  # Prefer a scheduled task (no extra deps) that runs at logon and restarts.
  $action  = New-ScheduledTaskAction -Execute $bin -Argument ($runArgs -join ' ')
  $trigger = New-ScheduledTaskTrigger -AtLogOn
  $set     = New-ScheduledTaskSettingsSet -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -StartWhenAvailable
  Register-ScheduledTask -TaskName "cairn-miner" -Action $action -Trigger $trigger -Settings $set -Force | Out-Null
  Start-ScheduledTask -TaskName "cairn-miner"
  Grn "  [ok] scheduled task 'cairn-miner' installed + started (runs at logon)"
  exit 0
}
if (-not $NoRun) {
  Grn "  starting cairn-miner - close this window to stop."
  & $bin @runArgs
} else {
  Write-Host "  installed. start with:  `"$bin`" --config `"$cfg`""
}
