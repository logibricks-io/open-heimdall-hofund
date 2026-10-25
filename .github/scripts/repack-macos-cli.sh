#!/bin/bash
# Sign the macOS CLI binaries cargo-dist just archived, then rebuild those
# archives and their checksums. Installers are generated from the manifest
# in a later job, so the hash written here is the one they publish.
#
# An empty signing group skips the job's macOS work. Linux and Windows
# legs have no Mach-O in target/distrib and skip as well.

set -euo pipefail

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
cd "$root"
# shellcheck source=macos-codesign.sh
source "$root/.github/scripts/macos-codesign.sh"

if ! signing_secrets_present; then
  echo "Apple signing secrets are absent; signing is skipped."
  exit 0
fi

shopt -s nullglob
bins=(target/distrib/*-apple-darwin/hofund)
if [ "${#bins[@]}" -eq 0 ]; then
  echo "No macOS CLI binary was built in this job; signing is skipped."
  exit 0
fi

if [ "$(uname -s)" != "Darwin" ]; then
  echo "Apple signing runs on macOS; this runner is $(uname -s)." >&2
  exit 1
fi

echo "Apple signing secrets are present."
for binary in "${bins[@]}"; do
  sign_macho "$binary"
  dir=$(basename "$(dirname "$binary")")
  archive="target/distrib/${dir}.tar.gz"
  tar -C target/distrib -czf "$archive" "$dir"
  "$root/.github/scripts/rewrite-archive-checksum.sh" "$archive" dist-manifest.json
  if notarization_secrets_present; then
    echo "Apple notarization secrets are present."
    notarize_macho "$binary"
  else
    echo "Apple notarization secrets are absent; notarization is skipped."
  fi
done
