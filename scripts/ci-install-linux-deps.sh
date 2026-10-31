#!/usr/bin/env bash
# Installs the Linux system libraries the Tauri 2 shell (crates/app-tauri) needs to
# compile and bundle. Used by CI (.github/workflows/*.yml) and safe to run locally on
# Debian/Ubuntu. No-op on other operating systems.
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "ci-install-linux-deps: not Linux, nothing to do"
  exit 0
fi

packages=(
  libwebkit2gtk-4.1-dev
  libgtk-3-dev
  libsoup-3.0-dev
  libayatana-appindicator3-dev
  librsvg2-dev
  # Needed by the bundler (AppImage/deb) and common build scripts.
  build-essential
  file
  patchelf
  pkg-config
  libssl-dev
  libxdo-dev
)

if ! command -v apt-get >/dev/null 2>&1; then
  echo "ci-install-linux-deps: apt-get not found; install these packages manually:" >&2
  printf '  %s\n' "${packages[@]}" >&2
  exit 1
fi

SUDO=""
if [[ "$(id -u)" -ne 0 ]]; then
  SUDO="sudo"
fi

export DEBIAN_FRONTEND=noninteractive
$SUDO apt-get update -q
$SUDO apt-get install -y -q --no-install-recommends "${packages[@]}"

pkg-config --exists webkit2gtk-4.1 && echo "ci-install-linux-deps: webkit2gtk-4.1 OK"
