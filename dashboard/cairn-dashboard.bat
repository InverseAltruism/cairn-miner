@echo off
REM cairn-dashboard.bat - double-click wrapper for cairn-dashboard.ps1 on Windows.
REM Watches a running cairn-miner (needs the miner started with --stats-port 3380).
REM Pass-through args, e.g.:  cairn-dashboard.bat -Port 3380 -Refresh 2
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0cairn-dashboard.ps1" %*
