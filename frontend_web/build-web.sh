#!/bin/zsh
# Build the Snow web frontend into frontend_web/www (ready to serve with
# `snow-bridge --www frontend_web/www`).
#
# Requires the Emscripten SDK (EMSDK set, or ~/emsdk) and the
# wasm32-unknown-emscripten Rust target. Pass --debug for a debug build.
set -euo pipefail

ROOT=${0:A:h:h}
PROFILE=release
CARGO_FLAGS=(--release)
if [[ ${1:-} == --debug ]]; then
    PROFILE=debug
    CARGO_FLAGS=()
fi

if ! command -v emcc >/dev/null; then
    source "${EMSDK:-$HOME/emsdk}/emsdk_env.sh" >/dev/null
fi
rustup target add wasm32-unknown-emscripten >/dev/null

cd "$ROOT"
cargo build -p snow_frontend_web --bin snow_web --target wasm32-unknown-emscripten "${CARGO_FLAGS[@]}"

OUT="$ROOT/target/wasm32-unknown-emscripten/$PROFILE"
cp "$OUT/snow_web.js" "$OUT/snow_web.wasm" "$ROOT/frontend_web/www/"
[[ -f "$OUT/snow_web.wasm.map" ]] && cp "$OUT/snow_web.wasm.map" "$ROOT/frontend_web/www/"
echo "Built frontend_web/www ($PROFILE). Serve it with:"
echo "  cargo run --release -p snow_bridge -- --www frontend_web/www"
