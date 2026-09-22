#!/usr/bin/env bash
set -euo pipefail

missing=0

check_command() {
  local cmd="$1"
  if command -v "$cmd" >/dev/null 2>&1; then
    printf "[ok]   %-14s %s\n" "$cmd" "$($cmd --version 2>/dev/null | head -n1 || true)"
  else
    printf "[miss] %s\n" "$cmd"
    missing=1
  fi
}

check_pkg() {
  local pkg="$1"
  if pkg-config --exists "$pkg"; then
    printf "[ok]   %-14s %s\n" "$pkg" "$(pkg-config --modversion "$pkg" 2>/dev/null || true)"
  else
    printf "[miss] %s\n" "$pkg"
    missing=1
  fi
}

check_command cargo
check_command rustc
check_command pkg-config
check_command clang

if command -v pkg-config >/dev/null 2>&1; then
  check_pkg fontconfig
  check_pkg libusb-1.0
  check_pkg libudev
  check_pkg xkbcommon
  check_pkg gbm
  check_pkg alsa
  check_pkg libavcodec
  check_pkg libavformat
  check_pkg libavutil
  check_pkg libswscale
fi

if [[ $missing -ne 0 ]]; then
  echo
  echo "One or more build dependencies are missing."
  echo "Run: ./scripts/install_pi5.sh"
  echo 'Then: source "$HOME/.cargo/env"'
  exit 1
fi

echo
echo "Required Rust/native build dependencies are available."
