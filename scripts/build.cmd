@echo off
setlocal

set "ENV_ARG=%~1"
if "%ENV_ARG%"=="" set "ENV_ARG=prod"

set "VERSION_ARG=%~2"

if "%VERSION_ARG%"=="" (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" "%ENV_ARG%"
) else (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" "%ENV_ARG%" "%VERSION_ARG%"
)
exit /b %ERRORLEVEL%
