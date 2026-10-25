#!/bin/bash
# Shared macOS signing and notarization for the CLI archives.
#
# Each function no-ops, with a message, when its secret group is empty.
# A present group that the platform rejects fails the caller.

set -euo pipefail

signing_secrets_present() {
  [ -n "${CODESIGN_CERTIFICATE:-}" ] &&
    [ -n "${CODESIGN_CERTIFICATE_PASSWORD:-}" ] &&
    [ -n "${CODESIGN_IDENTITY:-}" ]
}

notarization_secrets_present() {
  [ -n "${APPLE_ID:-}" ] &&
    [ -n "${APPLE_PASSWORD:-}" ] &&
    [ -n "${APPLE_TEAM_ID:-}" ]
}

# Import the Developer ID certificate and sign one Mach-O.
sign_macho() {
  local binary=$1
  local keychain="${RUNNER_TEMP:-/tmp}/hofund-signing.keychain-db"
  local password
  local p12="${RUNNER_TEMP:-/tmp}/hofund-developer-id.p12"

  password=$(uuidgen)
  security create-keychain -p "$password" "$keychain"
  security set-keychain-settings -lut 21600 "$keychain"
  security unlock-keychain -p "$password" "$keychain"
  printf '%s' "$CODESIGN_CERTIFICATE" | base64 --decode >"$p12"
  security import "$p12" -P "$CODESIGN_CERTIFICATE_PASSWORD" -A -t cert -f pkcs12 -k "$keychain" -T /usr/bin/codesign
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$password" "$keychain" >/dev/null
  security list-keychains -d user -s "$keychain"
  codesign --force --sign "$CODESIGN_IDENTITY" --options runtime --timestamp --keychain "$keychain" "$binary"
  rm -f "$p12"
  security delete-keychain "$keychain"
}

# Submit a zip copy. The published archive is not modified, so its checksum stays valid.
notarize_macho() {
  local binary=$1
  local zip="${RUNNER_TEMP:-/tmp}/hofund-notarize.zip"
  rm -f "$zip"
  ditto -c -k --keepParent "$binary" "$zip"
  xcrun notarytool submit "$zip" \
    --apple-id "$APPLE_ID" \
    --password "$APPLE_PASSWORD" \
    --team-id "$APPLE_TEAM_ID" \
    --wait
  rm -f "$zip"
}
