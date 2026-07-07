@echo off
REM cairn-miner - Windows one-click installer.
REM Double-click this file. It launches the PowerShell installer, which detects
REM your GPU, downloads the matching cairn-miner.exe, asks for your payout
REM address once, and starts mining.
setlocal
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
if errorlevel 1 (
  echo.
  echo Installer exited with an error. See the message above.
  pause
)
endlocal
