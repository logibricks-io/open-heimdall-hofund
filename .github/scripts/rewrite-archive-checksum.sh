#!/bin/bash
# Replace one archive checksum after the archive bytes change.
#
# cargo-dist writes "<sha256> *<name>" and stores the same hash in
# dist-manifest.json. Signing changes the archive, so both copies have to
# move together before the installer job reads the manifest.
#
# Usage: rewrite-archive-checksum.sh ARCHIVE MANIFEST

set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: rewrite-archive-checksum.sh ARCHIVE MANIFEST" >&2
  exit 2
fi

archive=$1
manifest=$2
name=$(basename "$archive")
sidecar="${archive}.sha256"

if [ ! -f "$archive" ]; then
  echo "archive not found: $archive" >&2
  exit 1
fi
if [ ! -f "$sidecar" ]; then
  echo "checksum not found: $sidecar" >&2
  exit 1
fi
if [ ! -f "$manifest" ]; then
  echo "manifest not found: $manifest" >&2
  exit 1
fi

old=$(awk 'NR == 1 { print $1; exit }' "$sidecar")
if [ -z "$old" ]; then
  echo "checksum file is empty: $sidecar" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  new=$(sha256sum "$archive" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
  new=$(shasum -a 256 "$archive" | awk '{ print $1 }')
else
  new=$(openssl dgst -sha256 "$archive" | awk '{ print $NF }')
fi

printf '%s *%s\n\n' "$new" "$name" >"$sidecar"

tmp=$(mktemp)
sed "s/${old}/${new}/g" "$manifest" >"$tmp"
mv "$tmp" "$manifest"
echo "checksum ${name}: ${new}"
