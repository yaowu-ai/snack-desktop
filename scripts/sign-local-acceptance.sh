#!/usr/bin/env bash
# Sign the isolated acceptance bundle locally; defaults to ad hoc per user choice.
# Call on a staged copy; keep the installed bundle as the rollback until verified.
set -euo pipefail

app_path="${1:?Usage: sign-local-acceptance.sh APP_PATH [CERTIFICATE_SHA1|-]}"
identity="${2:--}"
repo_root="$(cd "$(dirname "$0")/.." && pwd)"

if [[ "$identity" != '-' && ! "$identity" =~ ^[[:xdigit:]]{40}$ ]]; then
  echo 'Use - for ad hoc signing or an existing certificate SHA-1.' >&2
  exit 1
fi
bundle_id=$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "$app_path/Contents/Info.plist")
if [[ "$bundle_id" != 'cn.yaowutech.snack.standardqa' ]]; then
  echo 'Only the isolated standard-note acceptance app may be signed here.' >&2
  exit 1
fi

if ! codesign --force --sign "$identity" --timestamp=none \
  --entitlements "$repo_root/src-tauri/Entitlements.plist" "$app_path"; then
  echo 'Signing failed; keep the installed app unchanged.' >&2
  exit 1
fi
codesign --verify --deep --strict "$app_path"
requirement=$(codesign -d -r- "$app_path" 2>&1)
if [[ "$identity" != '-' && ( "$requirement" == *'cdhash '* || "$requirement" != *'certificate'* ) ]]; then
  echo 'The resulting signing requirement is not bound to a stable certificate.' >&2
  exit 1
fi
printf '%s\n' "$requirement"
