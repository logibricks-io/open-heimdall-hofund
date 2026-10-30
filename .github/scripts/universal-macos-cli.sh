#!/bin/bash
# Build the universal macOS archives from the arm64 and x86_64 tarballs:
# one for the CLI package (hofund) and one for the daemon package (hofundd).
#
# cargo-dist 0.32 rejects the universal2 triple. The installers keep
# selecting one architecture. This archive is an extra release asset.
#
# Usage: universal-macos-cli.sh ARCHIVE_DIR OUTPUT_DIR

set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: universal-macos-cli.sh ARCHIVE_DIR OUTPUT_DIR" >&2
  exit 2
fi

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
# shellcheck source=macos-codesign.sh
source "$root/.github/scripts/macos-codesign.sh"

archive_dir=$1
output_dir=$2

# Writes OUTPUT_DIR/PACKAGE-universal2-apple-darwin.tar.gz holding BINARY.
universal() {
  local package=$1 binary=$2
  local name="${package}-universal2-apple-darwin"
  local arm= intel= path
  while IFS= read -r path; do
    case $(basename "$path") in
      "${package}-aarch64-apple-darwin.tar.gz") arm=$path ;;
      "${package}-x86_64-apple-darwin.tar.gz") intel=$path ;;
    esac
  done < <(find "$archive_dir" -name "${package}-*-apple-darwin.tar.gz" -type f)
  if [ -z "$arm" ] || [ -z "$intel" ]; then
    echo "both macOS ${package} archives are required" >&2
    exit 1
  fi

  local work
  work=$(mktemp -d)
  mkdir -p "$work/arm" "$work/intel" "$work/uni/$name" "$output_dir"
  tar -xzf "$arm" -C "$work/arm"
  tar -xzf "$intel" -C "$work/intel"
  local arm_bin= intel_bin=
  while IFS= read -r path; do
    arm_bin=$path
    break
  done < <(find "$work/arm" -type f -name "$binary")
  while IFS= read -r path; do
    intel_bin=$path
    break
  done < <(find "$work/intel" -type f -name "$binary")
  if [ -z "$arm_bin" ] || [ -z "$intel_bin" ]; then
    echo "both macOS ${binary} binaries are required" >&2
    exit 1
  fi

  local universal_bin="$work/uni/$name/$binary"
  lipo -create -output "$universal_bin" "$arm_bin" "$intel_bin"
  chmod +x "$universal_bin"

  if signing_secrets_present; then
    echo "Apple signing secrets are present."
    sign_macho "$universal_bin"
  else
    echo "Apple signing secrets are absent; signing is skipped."
  fi

  local archive="$output_dir/${name}.tar.gz" hash
  tar -C "$work/uni" -czf "$archive" "$name"
  if command -v sha256sum >/dev/null 2>&1; then
    hash=$(sha256sum "$archive" | awk '{ print $1 }')
  elif command -v shasum >/dev/null 2>&1; then
    hash=$(shasum -a 256 "$archive" | awk '{ print $1 }')
  else
    hash=$(openssl dgst -sha256 "$archive" | awk '{ print $NF }')
  fi
  printf '%s *%s\n\n' "$hash" "$(basename "$archive")" >"${archive}.sha256"
  echo "checksum $(basename "$archive"): ${hash}"

  if signing_secrets_present && notarization_secrets_present; then
    echo "Apple notarization secrets are present."
    notarize_macho "$universal_bin"
  elif notarization_secrets_present; then
    echo "Notarization needs the signing secrets; notarization is skipped."
  else
    echo "Apple notarization secrets are absent; notarization is skipped."
  fi

  rm -rf "$work"
}

universal hofund-cli hofund
universal hofund-daemon-bin hofundd
