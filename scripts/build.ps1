param(
    [ValidateSet("dev", "test", "prod", "production")]
    [string]$Env = "prod",

    [string]$Version = ""
)

$ErrorActionPreference = "Stop"

$env:SNACK_DESKTOP_ENV = $Env
if ($Version) {
    $env:SNACK_DESKTOP_VERSION = $Version
}

if ($Version) {
    Write-Host "Building Snack desktop installer for '$Env' environment, version '$Version'..."
}
else {
    Write-Host "Building Snack desktop installer for '$Env' environment..."
}

Push-Location (Join-Path $PSScriptRoot "..\src-tauri")
try {
    cargo tauri build
}
finally {
    Pop-Location
}
