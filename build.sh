#!/usr/bin/env bash
set -euo pipefail

# Ensure rustup toolchain takes priority over Homebrew
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"

# Check for wasm-pack
if ! command -v wasm-pack &> /dev/null; then
    echo "wasm-pack not found. Installing..."
    curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
fi

# Check for wasm32-unknown-unknown target
if ! rustup target list --installed | grep -q wasm32-unknown-unknown; then
    echo "Adding wasm32-unknown-unknown target..."
    rustup target add wasm32-unknown-unknown
fi

echo "Building player-wasm..."
wasm-pack build player-wasm/ \
    --target web \
    --out-dir ../www/pkg \
    --out-name player

echo "Build complete! Output in www/pkg/"
