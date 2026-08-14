@echo off
chcp 65001 >nul
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\launch-user.ps1"
if errorlevel 1 (
  echo.
  echo Failed to launch. See message above.
  pause
)
