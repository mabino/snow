#!/bin/sh
# Run the Snow web frontend + bridge test suite (macOS zsh/sh or Linux sh).
#
# Expects a release snow_bridge and a built frontend_web/www (see
# frontend_web/build-web.sh); the Docker test image provides both.
# Set SNOW_ROM and SNOW_DISK to also run the System 6 AppleTalk end-to-end
# test (otherwise only the plumbing is tested end-to-end).
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

echo "== Rust unit tests"
# (the CPU single-step suite needs the testdata/m68000 submodule)
cargo test -p snow_core --lib -- --skip singlestep
cargo test -p snow_nat --all-features
cargo test -p snow_bridge
# The public-relay build (no NAT engine)
cargo test -p snow_bridge --no-default-features
cargo test -p snow_frontend_web --lib

cd "$ROOT/frontend_web/tests"
[ -d node_modules ] || npm ci

echo "== JavaScript unit tests"
node --test unit/*.test.mjs

echo "== Bridge protocol test"
node bridge.mjs

echo "== End-to-end browser test"
node e2e.mjs
