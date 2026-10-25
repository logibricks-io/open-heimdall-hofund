#!/usr/bin/env bash
# Print the directory Tauri wrote this build's bundles into.
#
# hofund-desktop is a workspace member, so Cargo writes target/ at the
# workspace root (or at CARGO_TARGET_DIR), not under the package directory.
# `cargo metadata` is the path Cargo used.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
if command -v python3 >/dev/null 2>&1; then
  py=python3
else
  py=python
fi
cargo metadata --format-version 1 --no-deps \
  | "$py" -c 'import json,sys; print(json.load(sys.stdin)["target_directory"].replace("\\", "/") + "/release/bundle")'
