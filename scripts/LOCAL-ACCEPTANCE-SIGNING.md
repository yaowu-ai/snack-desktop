# Local standard-note acceptance signing

The isolated bundle ID is `cn.yaowutech.snack.standardqa`. This local acceptance
app uses ad hoc signing by user choice. No keychain unlock or distribution
certificate is required. Keep the installed binary unchanged when only updating
the web page; a native rebuild can invalidate the previous macOS privacy grant.

## Update procedure

1. Preserve the installed app and copy the next bundle to a staging directory
   using `ditto`. Preserve permissions and record hashes before replacement.
2. Run `bash scripts/sign-local-acceptance.sh STAGED_APP`. This defaults to
   `codesign --sign -` and verifies the full bundle. An existing certificate
   SHA-1 can optionally be passed as the second argument if later requested.
3. Verify no recording is active, completely quit the acceptance app, recheck
   the installed files against the original hashes, then replace it with the
   verified staged bundle. Retain the old bundle as a rollback. Leave the
   production Snack app and its data untouched.
4. Launch the acceptance app. Microphone and System Audio Recording Only
   are separate OS permissions. A changed native binary may need reauthorization.
   If system audio capture reports permission denied after a grant, completely quit
   and reopen the app. Never edit the TCC database or reset unrelated apps.
5. Validate recording with both microphone and system output, then a second
   start to check for repeated prompts. Signature verification alone does not
   establish device acceptance.

## Rollback

Quit the acceptance app, keep the unsuccessful updated bundle separately, and
restore the retained original at its original path. Leave the app's user-data
directory untouched. Restoring an old signature may require OS reauthorization.

## Core Audio Taps recording (2026-09-15)

Meeting recording requires macOS 14.2 or later and uses a Core Audio process tap
for system audio plus CPAL for the microphone. The binary contains the tap
bridge, so both prompts identify the acceptance app. Include
`NSAudioCaptureUsageDescription` when updating the staged Info.plist. Do not
request screen permission for recording or automatically open System Settings
on capture failure. The optional legacy meeting detector only runs when screen
permission is already granted; it cannot request that permission in background.

The previous acceptance bundle is retained under the output directory's
`core-audio-tap-originals/previous-app.bundle`. Actual OS prompts and system-output
capture require device validation after the user grants access; fixture tests
only cover the page's sequencing and recovery interactions.
