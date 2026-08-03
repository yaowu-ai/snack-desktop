#!/usr/bin/env bash

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SOURCE_DIR="${SNACK_RECORD_SOURCE_DIR:-}"

if [[ -z "${SOURCE_DIR}" ]]; then
  for candidate in \
    "${REPO_ROOT}/../snack-record" \
    "${REPO_ROOT}/../../../snack-record"
  do
    if [[ -f "${candidate}/build.sh" ]]; then
      SOURCE_DIR="${candidate}"
      break
    fi
  done
fi

if [[ -z "${SOURCE_DIR}" || ! -f "${SOURCE_DIR}/build.sh" ]]; then
  echo "Snack Record runtime source was not found. Set SNACK_RECORD_SOURCE_DIR." >&2
  exit 1
fi

DESTINATION_ROOT="${REPO_ROOT}/src-tauri/embedded-runtime"
DESTINATION_APP="${DESTINATION_ROOT}/Snack Recording Service.app"
SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:-${SIGN_IDENTITY:--}}"

mkdir -p "${DESTINATION_ROOT}"
rm -rf "${DESTINATION_APP}"

SIGN_IDENTITY="${SIGNING_IDENTITY}" \
SNACK_RECORD_APP_PATH="${DESTINATION_APP}" \
SNACK_RECORD_EMBEDDED=1 \
zsh "${SOURCE_DIR}/build.sh" >/dev/null

if [[ ! -x "${DESTINATION_APP}/Contents/MacOS/Snack Record" ]]; then
  echo "Embedded Snack Record runtime was not built: ${DESTINATION_APP}" >&2
  exit 1
fi

printf '%s\n' "${DESTINATION_APP}"
