@echo off
setlocal
where node >nul 2>nul
if errorlevel 1 (
  echo Node.js 22.12 or newer is required.
  exit /b 1
)
node "%~dp0scripts\services.mjs" start
exit /b %errorlevel%
