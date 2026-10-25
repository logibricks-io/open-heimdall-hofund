#!/bin/bash
# Build the universal macOS CLI archive from the arm64 and x86_64 tarballs.
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
name=hofund-cli-universal2-apple-darwin

arm=
intel=
while IFS= read -r path; do
  case $(basename "$path") in
    hofund-cli-aarch64-apple-darwin.tar.gz) arm=$path ;;
    hofund-cli-x86_64-apple-darwin.tar.gz) intel=$path ;;
  esac
done < <(find "$archive_dir" -name 'hofund-cli-*-apple-darwin.tar.gz' -type f)
if [ -z "$arm" ] || [ -z "$intel" ]; then
  echo "both macOS CLI archives are required" >&2
  exit 1
fi

work=$(mktemp -d)
mkdir -p "$work/arm" "$work/intel" "$work/uni/$name" "$output_dir"
tar -xzf "$arm" -C "$work/arm"
tar -xzf "$intel" -C "$work/intel"
arm_bin=
intel_bin=
while IFS= read -r path; do
  arm_bin=$path
  break
done < <(find "$work/arm" -type f -name hofund)
while IFS= read -r path; do
  intel_bin=$path
  break
done < <(find "$work/intel" -type f -name hofund)
if [ -z "$arm_bin" ] || [ -z "$intel_bin" ]; then
  echo "both macOS CLI binaries are required" >&2
  exit 1
fi

lipo -create -output "$work/uni/$name/hofund" "$arm_bin" "$intel_bin"
chmod +x "$work/uni/$name/hofund"

if signing_secrets_present; then
  echo "Apple signing secrets are present."
  sign_macho "$work/uni/$name/hofund"
else
  echo "Apple signing secrets are absent; signing is skipped."
fi

archive="$output_dir/${name}.tar.gz"
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
  notarize_macho "$work/uni/$name/hofund"
elif notarization_secrets_present; then
  echo "Notarization needs the signing secrets; notarization is skipped."
else
  echo "Apple notarization secrets are absent; notarization is skipped."
fi

rm -rf "$work"
