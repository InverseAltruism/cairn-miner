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
$DefaultPool = "pool.cairn-substrate.com:3333"

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
if ($Address -notmatch '^[0-9a-f]{40}$') { Die "address must be 40 lowercase hex chars (create one: cairn-miner.exe newwallet)" }

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

# 3. download the matching prebuilt exe
$asset = "cairn-miner-windows-$Backend.exe"
$url   = "https://github.com/$Repo/releases/latest/download/$asset"
try {
  Write-Host "  downloading $asset ..."
  Invoke-WebRequest -Uri $url -OutFile "$bin.tmp" -UseBasicParsing
  Move-Item -Force "$bin.tmp" $bin
  Grn "  [ok] installed $asset"
} catch {
  Die "could not download $asset (no published release yet for this backend?). Try -Backend cpu, or build from source: cargo build --release"
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
