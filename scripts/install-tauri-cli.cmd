@echo off
setlocal

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install-tauri-cli.ps1"
exit /b %ERRORLEVEL%
