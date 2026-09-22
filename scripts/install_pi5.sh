#!/usr/bin/env bash
set -euo pipefail

sudo apt-get update
sudo apt-get install -y \
  build-essential \
  ca-certificates \
  curl \
  git \
  gpsd gpsd-clients \
  pkg-config \
  cmake \
  clang \
  libclang-dev \
  ffmpeg \
  libasound2-dev libavcodec-dev \
  libavformat-dev \
  libavutil-dev \
  libswscale-dev \
  libfontconfig-dev \
  libssl-dev \
  libusb-1.0-0-dev \
  libudev-dev \
  libxkbcommon-dev \
  libgbm-dev \
  libseat-dev \
  libwayland-dev \
  libinput-dev \
  libx11-dev \
  libx11-xcb-dev \
  libxcb1-dev \
  libxcb-shape0-dev \
  libxcb-xfixes0-dev \
  libxcursor-dev

if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  echo 'Rust installed. Start a new shell or run: source "$HOME/.cargo/env"'
fi
