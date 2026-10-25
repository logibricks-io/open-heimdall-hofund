#!/usr/bin/env bash
# Fail when release.yml is not `dist generate` plus the recorded hand edits.
#
# allow-dirty skips this check inside dist itself. Drop that key, regenerate,
# and compare the diff to .github/dist-release.patch.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
tmp="$(mktemp -d)"
cp .github/workflows/release.yml "$tmp/committed.yml"
cp dist-workspace.toml "$tmp/dist-workspace.toml"
cleanup() {
  cp "$tmp/dist-workspace.toml" dist-workspace.toml
  cp "$tmp/committed.yml" .github/workflows/release.yml
}
trap cleanup EXIT
grep -v '^allow-dirty = ' "$tmp/dist-workspace.toml" > dist-workspace.toml
dist generate --mode ci
diff -u \
  --label release.generated.yml \
  --label release.yml \
  .github/workflows/release.yml \
  "$tmp/committed.yml" > "$tmp/actual.patch" || true
if ! cmp -s "$tmp/actual.patch" .github/dist-release.patch; then
  echo "release.yml does not match dist generate plus .github/dist-release.patch" >&2
  diff -u .github/dist-release.patch "$tmp/actual.patch" | head -n 200 >&2
  exit 1
fi
echo "release.yml matches dist generate plus the recorded hand edits"
