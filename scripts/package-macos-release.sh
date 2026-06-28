#!/usr/bin/env bash

set -euo pipefail

: "${TARGET_TRIPLE:?TARGET_TRIPLE is required}"
: "${APP_VERSION:?APP_VERSION is required}"
: "${APPLE_SIGNING_IDENTITY:?APPLE_SIGNING_IDENTITY is required}"
: "${APPLE_ID:?APPLE_ID is required}"
: "${APPLE_PASSWORD:?APPLE_PASSWORD is required}"
: "${APPLE_TEAM_ID:?APPLE_TEAM_ID is required}"
: "${DMG_ARCH_SUFFIX:?DMG_ARCH_SUFFIX is required}"

APP_NAME="Snack"
BUNDLE_ROOT="src-tauri/target/${TARGET_TRIPLE}/release/bundle"
APP_PATH="${BUNDLE_ROOT}/macos/${APP_NAME}.app"
APP_ZIP_PATH="${BUNDLE_ROOT}/macos/${APP_NAME}.zip"
DMG_DIR="${BUNDLE_ROOT}/dmg"
if [[ "${DMG_ARCH_SUFFIX}" == "apple-silicon" ]]; then
  RELEASE_ARCH_SUFFIX="arm64"
else
  RELEASE_ARCH_SUFFIX="x64"
fi
DMG_PATH="${DMG_DIR}/${APP_NAME}_${APP_VERSION}_macos_${RELEASE_ARCH_SUFFIX}.dmg"
UPDATER_ARCHIVE_PATH="${BUNDLE_ROOT}/macos/${APP_NAME}_${APP_VERSION}_macos_${RELEASE_ARCH_SUFFIX}.app.tar.gz"
UPDATER_SIGNATURE_PATH="${UPDATER_ARCHIVE_PATH}.sig"

submit_for_notarization() {
  local artifact_path="$1"
  local artifact_label="$2"
  local output_file
  local submission_id
  local status

  output_file="$(mktemp)"

  xcrun notarytool submit "${artifact_path}" \
    --apple-id "${APPLE_ID}" \
    --password "${APPLE_PASSWORD}" \
    --team-id "${APPLE_TEAM_ID}" \
    --wait \
    --output-format json | tee "${output_file}"

  submission_id="$(python3 - <<'PY' "${output_file}"
import json, sys
with open(sys.argv[1], 'r', encoding='utf-8') as f:
    data = json.load(f)
print(data.get('id', ''))
PY
)"

  status="$(python3 - <<'PY' "${output_file}"
import json, sys
with open(sys.argv[1], 'r', encoding='utf-8') as f:
    data = json.load(f)
print(data.get('status', ''))
PY
)"

  if [[ -n "${submission_id}" ]]; then
    echo "===== Notary log for ${artifact_label} (${submission_id}) ====="
    xcrun notarytool log "${submission_id}" \
      --apple-id "${APPLE_ID}" \
      --password "${APPLE_PASSWORD}" \
      --team-id "${APPLE_TEAM_ID}" || true
    echo
  fi

  rm -f "${output_file}"

  if [[ "${status}" != "Accepted" ]]; then
    echo "Notarization failed for ${artifact_label} with status: ${status}" >&2
    exit 1
  fi
}

rm -f "${APP_ZIP_PATH}" "${DMG_PATH}" "${UPDATER_ARCHIVE_PATH}" "${UPDATER_SIGNATURE_PATH}"

echo "===== Build environment ====="
uname -a || true
sw_vers || true
xcodebuild -version || true
rustc -Vv || true
node --version || true
npm --version || true
echo "TARGET_TRIPLE=${TARGET_TRIPLE}"
echo

CI=true npm run build -- --bundles app --target "${TARGET_TRIPLE}" --no-sign

if [[ ! -d "${APP_PATH}" ]]; then
  echo "Expected app bundle not found: ${APP_PATH}" >&2
  exit 1
fi

xattr -crs "${APP_PATH}"

codesign \
  --force \
  --timestamp \
  --options runtime \
  --sign "${APPLE_SIGNING_IDENTITY}" \
  --entitlements src-tauri/Entitlements.plist \
  "${APP_PATH}"

codesign --verify --deep --strict --verbose=2 "${APP_PATH}"

ditto -c -k --keepParent --sequesterRsrc "${APP_PATH}" "${APP_ZIP_PATH}"
submit_for_notarization "${APP_ZIP_PATH}" "${APP_NAME}.zip"
xcrun stapler staple "${APP_PATH}"

tar -czf "${UPDATER_ARCHIVE_PATH}" -C "${BUNDLE_ROOT}/macos" "${APP_NAME}.app"
SIGN_OUTPUT="$(npx tauri signer sign \
  -k "${TAURI_SIGNING_PRIVATE_KEY}" \
  ${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:+-p "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD}"} \
  "${UPDATER_ARCHIVE_PATH}")"
printf '%s\n' "${SIGN_OUTPUT}" | awk '/^Signature:/{getline; print; exit}' > "${UPDATER_SIGNATURE_PATH}"
if [[ ! -s "${UPDATER_SIGNATURE_PATH}" ]]; then
  echo "Failed to extract updater signature." >&2
  printf '%s\n' "${SIGN_OUTPUT}" >&2
  exit 1
fi

mkdir -p "${DMG_DIR}"
STAGING_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "${STAGING_DIR}"
}
trap cleanup EXIT

ditto "${APP_PATH}" "${STAGING_DIR}/${APP_NAME}.app"
ln -s /Applications "${STAGING_DIR}/Applications"

hdiutil create \
  -volname "${APP_NAME}" \
  -srcfolder "${STAGING_DIR}" \
  -ov \
  -format UDZO \
  "${DMG_PATH}"

codesign \
  --force \
  --timestamp \
  --sign "${APPLE_SIGNING_IDENTITY}" \
  "${DMG_PATH}"

submit_for_notarization "${DMG_PATH}" "$(basename "${DMG_PATH}")"
xcrun stapler staple "${DMG_PATH}"
