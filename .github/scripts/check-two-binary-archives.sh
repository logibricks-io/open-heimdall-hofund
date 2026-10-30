#!/bin/bash
# Prove that the CLI and daemon archives cargo-dist built form one working
# install: for each target in DISTRIB_DIR, both archives and their checksums
# exist, and unpacking both into one directory yields hofund and hofundd side
# by side. For the runner's own target, `hofund serve` must then start the
# hofundd next to it: a missing config makes hofundd fail with its own
# `hofundd: ` prefix, which the CLI can only show if it ran that binary.
#
# cargo-dist 0.32 makes one archive per package, so the two binaries ship in
# two archives that install into the same directory.
#
# Usage: check-two-binary-archives.sh DISTRIB_DIR

set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: check-two-binary-archives.sh DISTRIB_DIR" >&2
  exit 2
fi

distrib=$1
host=$(rustc -vV | sed -n 's/^host: //p')
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

unpack() {
  local archive=$1 into=$2
  mkdir -p "$into"
  case $archive in
    *.tar.gz) tar -xzf "$archive" -C "$into" ;;
    *.zip)
      if command -v unzip >/dev/null 2>&1; then
        unzip -q "$archive" -d "$into"
      else
        pwsh -NoProfile -Command "Expand-Archive -LiteralPath '$archive' -DestinationPath '$into'"
      fi
      ;;
    *) echo "unknown archive format: $archive" >&2; exit 1 ;;
  esac
}

# Copies the one file named NAME under DIR into DEST.
take() {
  local dir=$1 name=$2 dest=$3 found
  found=$(find "$dir" -type f -name "$name" | head -n 1)
  if [ -z "$found" ]; then
    echo "$name is missing from its archive" >&2
    exit 1
  fi
  cp "$found" "$dest/$name"
  chmod +x "$dest/$name"
}

shopt -s nullglob
checked=0
for cli in "$distrib"/hofund-cli-*.tar.gz "$distrib"/hofund-cli-*.zip; do
  base=$(basename "$cli")
  case $base in
    *.tar.gz) ext=.tar.gz ;;
    *) ext=.zip ;;
  esac
  target=${base#hofund-cli-}
  target=${target%"$ext"}
  case $target in
    *windows*) suffix=.exe ;;
    *) suffix= ;;
  esac
  daemon="$distrib/hofund-daemon-bin-${target}${ext}"
  for archive in "$cli" "$daemon"; do
    if [ ! -f "$archive" ]; then
      echo "missing archive: $archive" >&2
      exit 1
    fi
    if [ ! -f "${archive}.sha256" ]; then
      echo "missing checksum: ${archive}.sha256" >&2
      exit 1
    fi
  done

  install="$work/$target/install"
  mkdir -p "$install"
  unpack "$cli" "$work/$target/cli"
  unpack "$daemon" "$work/$target/daemon"
  take "$work/$target/cli" "hofund${suffix}" "$install"
  take "$work/$target/daemon" "hofundd${suffix}" "$install"
  echo "$target: hofund${suffix} and hofundd${suffix} unpack side by side"

  if [ "$target" = "$host" ]; then
    set +e
    stderr=$(env -u HOFUND_CONFIG -u HOFUND_LOG \
      "$install/hofund${suffix}" serve --config "$work/missing.toml" 2>&1 >/dev/null)
    code=$?
    set -e
    if [ "$code" -ne 1 ] || [ "${stderr#hofundd: }" = "$stderr" ]; then
      echo "hofund serve did not run the hofundd beside it (exit $code):" >&2
      echo "$stderr" >&2
      exit 1
    fi
    echo "$target: hofund serve started the hofundd beside it"
  fi
  checked=$((checked + 1))
done

if [ "$checked" -eq 0 ]; then
  echo "no hofund-cli archives in $distrib" >&2
  exit 1
fi
