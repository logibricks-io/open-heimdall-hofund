#!/usr/bin/env bash
# Install the GTK/WebKit libs the desktop crate needs to compile on Linux CI.
#
# GitHub-hosted Ubuntu runners periodically fail DNS for security.ubuntu.com
# and sometimes for the Azure archive as well. A single `apt-get update` +
# `apt-get install` with Acquire::Retries can then sit until the workflow
# step's 10-minute cap, which is what kept failing #788 / #789. Bound each
# attempt and retry the whole operation so a transient resolver outage cannot
# consume the entire step.
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  exit 0
fi

packages=(
  libwebkit2gtk-4.1-dev
  libayatana-appindicator3-dev
  librsvg2-dev
  patchelf
  libxdo-dev
)

# When only security.ubuntu.com is unresolvable, use the runner's Azure
# archive for the security pocket too.
rewrite_security_mirror() {
  local f
  shopt -s nullglob
  for f in /etc/apt/sources.list \
           /etc/apt/sources.list.d/ubuntu.sources \
           /etc/apt/sources.list.d/*.sources \
           /etc/apt/apt-mirrors.txt; do
    [ -e "$f" ] || continue
    sudo sed -i -E \
      's#https?://security\.ubuntu\.com/ubuntu#http://azure.archive.ubuntu.com/ubuntu#g' \
      "$f" || true
  done
}

apt_opts=(
  -o Acquire::Retries=2
  -o Acquire::http::Timeout=15
  -o Acquire::https::Timeout=15
  -o Acquire::ForceIPv4=true
  -o Dpkg::Options::=--force-confold
)

rewrite_security_mirror

max=3
attempt=1
while true; do
  echo "linux host libraries: attempt ${attempt}/${max}"
  if timeout 60s sudo DEBIAN_FRONTEND=noninteractive apt-get update "${apt_opts[@]}" \
    && timeout 150s sudo DEBIAN_FRONTEND=noninteractive apt-get install -y "${apt_opts[@]}" "${packages[@]}"; then
    echo "linux host libraries: installed"
    exit 0
  fi
  if [ "$attempt" -ge "$max" ]; then
    echo "linux host libraries: failed after ${max} bounded attempts" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 15
done
