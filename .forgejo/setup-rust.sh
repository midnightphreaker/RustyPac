#!/usr/bin/env bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev cmake curl ca-certificates gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 musl-tools
if ! command -v rustup >/dev/null; then
  curl --proto '=https' --tlsv1.2 -fsS https://sh.rustup.rs -o /tmp/release-rustup.sh
  sh /tmp/release-rustup.sh -y --profile minimal --default-toolchain 1.98.1
fi
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
rustup toolchain install 1.98.1 --profile minimal --component rustfmt --component clippy
rustup override set 1.98.1
