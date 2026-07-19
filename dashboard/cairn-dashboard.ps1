<#
  cairn-dashboard.ps1 - watch a running cairn-miner in a Windows terminal.

  Reads the miner's local /stats endpoint (loopback only; the miner must run
  with --stats-port) and shows hashrate, accepted/rejected/stale shares +
  reject %, difficulty, uptime, reconnects, connection state and version, plus
  GPU temp/power when nvidia-smi is present. Read-only - never touches mining.

  Multi-GPU rigs run one worker per card on consecutive ports (BASE, BASE+1,
  ...); this probes upward from -Port and aggregates them.

  Usage:
    powershell -ExecutionPolicy Bypass -File cairn-dashboard.ps1
    ... -Port 3380 -Refresh 2
    ... -Once
  Press Ctrl-C to quit.
#>
param(
  [int]$Port = 3380,
  [int]$Refresh = 2,
  [switch]$Once,
  [int]$MaxGpus = 16
)

$haveNvSmi = [bool](Get-Command nvidia-smi -ErrorAction SilentlyContinue)

function Get-Stats([int]$p) {
  try { return Invoke-RestMethod -Uri "http://127.0.0.1:$p/stats" -TimeoutSec 2 -ErrorAction Stop }
  catch { return $null }
}

function Format-Hr([double]$v) {
  if ($v -ge 1e9) { return ('{0:N2} GH/s' -f ($v/1e9)) }
  elseif ($v -ge 1e6) { return ('{0:N2} MH/s' -f ($v/1e6)) }
  elseif ($v -ge 1e3) { return ('{0:N2} KH/s' -f ($v/1e3)) }
  else { return ('{0:N0} H/s' -f $v) }
}

function Format-Dur([int]$s) {
  $d=[int]($s/86400); $h=[int](($s%86400)/3600); $m=[int](($s%3600)/60); $sec=$s%60
  if ($d -gt 0) { return ('{0}d{1:D2}h' -f $d,$h) }
  elseif ($h -gt 0) { return ('{0}h{1:D2}m' -f $h,$m) }
  else { return ('{0}m{1:D2}s' -f $m,$sec) }
}

function Get-GpuTP([int]$i) {
  if (-not $haveNvSmi) { return '' }
  try {
    $o = & nvidia-smi --query-gpu=temperature.gpu,power.draw --format=csv,noheader,nounits -i $i 2>$null
    if ($o) { $p = ($o -split ','); return ('{0}C {1}W' -f ([int]$p[0].Trim()), [int]([double]$p[1].Trim())) }
  } catch {}
  return ''
}

function Render {
  $rows = @(); $nUp = 0; $totHps = 0.0; $totAcc = 0; $totRej = 0; $totStale = 0
  for ($i = 0; $i -lt $MaxGpus; $i++) {
    $j = Get-Stats ($Port + $i)
    if ($null -eq $j) { break }
    $nUp++
    $dot = if ($j.connected) { 'UP  ' } else { 'DOWN' }
    $wk = if ($j.worker) { $j.worker } else { "gpu$i" }
    $totHps += [double]$j.hashrate_total_hps
    $totAcc += [int]$j.shares_accepted; $totRej += [int]$j.shares_rejected; $totStale += [int]$j.shares_stale
    $tp = Get-GpuTP $i
    $rows += ('  {0,-10} {1} {2,12}  d={3,-8} a={4,-5} r={5,-4} s={6,-4} rc={7,-3} {8}' -f `
      $wk, $dot, (Format-Hr ([double]$j.hashrate_total_hps)), $j.difficulty, `
      $j.shares_accepted, $j.shares_rejected, $j.shares_stale, $j.reconnects, $tp)
  }

  $base = Get-Stats $Port
  Clear-Host
  if ($null -eq $base) {
    Write-Host "  cairn-miner dashboard`n"
    Write-Host "  no miner answering on 127.0.0.1:$Port/stats"
    Write-Host "  start the miner with  --stats-port $Port"
    return
  }
  $rejpct = if (($totAcc + $totRej) -gt 0) { '{0:N1}' -f (100.0*$totRej/($totAcc+$totRej)) } else { '0.0' }
  $lastShare = if ($null -eq $base.last_share_age_secs) { 'no shares yet' } else { "$($base.last_share_age_secs)s ago" }

  Write-Host ('  cairn-miner v{0}  ·  {1}  ·  pool {2}  ·  up {3}' -f $base.version, $base.backend, $base.pool, (Format-Dur ([int]$base.uptime_secs)))
  Write-Host '  ----------------------------------------------------------------------'
  Write-Host ('  TOTAL  {0}   accepted {1}   rejected {2} ({3}%)   stale {4}   last share {5}' -f (Format-Hr $totHps), $totAcc, $totRej, $rejpct, $totStale, $lastShare)
  Write-Host ('  workers ({0}):' -f $nUp)
  $rows | ForEach-Object { Write-Host $_ }
  Write-Host '  ----------------------------------------------------------------------'
  Write-Host ("  refresh {0}s · Ctrl-C to quit" -f $Refresh)
}

if ($Once) { Render; exit 0 }
while ($true) { Render; Start-Sleep -Seconds $Refresh }
