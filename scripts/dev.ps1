param(
    [ValidateSet("dev", "test", "prod", "production")]
    [string]$Env = "dev",

    [string]$Version = ""
)

$ErrorActionPreference = "Stop"

$env:SNACK_DESKTOP_ENV = $Env
if ($Version) {
    $env:SNACK_DESKTOP_VERSION = $Version
}

if ($Version) {
    Write-Host "Starting Snack desktop in '$Env' environment, version '$Version'..."
}
else {
    Write-Host "Starting Snack desktop in '$Env' environment..."
}

Push-Location (Join-Path $PSScriptRoot "..\src-tauri")
try {
    cargo run
}
finally {
    Pop-Location
}
