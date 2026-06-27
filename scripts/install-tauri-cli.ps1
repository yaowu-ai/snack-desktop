$ErrorActionPreference = "Stop"

Write-Host "Installing Tauri CLI v2..."
cargo install tauri-cli --version "^2" --locked
