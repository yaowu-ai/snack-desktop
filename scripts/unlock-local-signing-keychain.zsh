#!/bin/zsh
set -euo pipefail

signing_dir="${SNACK_LOCAL_SIGNING_DIR:-$HOME/Library/Application Support/Snack Record/Signing}"
keychain_path="${SNACK_LOCAL_SIGNING_KEYCHAIN:-$signing_dir/snack-record.keychain-db}"
password_file="${SNACK_LOCAL_SIGNING_PASSWORD_FILE:-$signing_dir/keychain-password}"

if [[ ! -f "$keychain_path" ]]; then
  exit 0
fi

if [[ ! -f "$password_file" ]]; then
  print -u2 "The managed signing keychain exists, but its password file is missing."
  exit 1
fi

password="$(<"$password_file")"
security unlock-keychain -p "$password" "$keychain_path"

listed="$(security list-keychains -d user | /usr/bin/sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//')"
if ! print -r -- "$listed" | /usr/bin/grep -Fx "$keychain_path" >/dev/null; then
  keychains=("${(@f)listed}")
  security list-keychains -d user -s "${keychains[@]}" "$keychain_path"
fi
