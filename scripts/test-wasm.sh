#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
toolchain=${1:-stable}
mode=${2:-normal}
case "$mode" in normal|shared) ;; *) echo "mode must be normal or shared" >&2; exit 2 ;; esac
node --version
cargo "+$toolchain" --version
rustc "+$toolchain" -Vv
manifest=tools/wasm-check/Cargo.toml
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/tools/wasm-check/target}/$toolchain-$mode"
mkdir -p "$CARGO_TARGET_DIR"
export CARGO_TARGET_DIR="$(cd "$CARGO_TARGET_DIR" && pwd)"
export ZENFLATE_EXPECTED_BYTES="$CARGO_TARGET_DIR/scalar-encoded.bin"
# Compile and execute our scalar implementation with independent test-only oracles.
env -u RUSTFLAGS cargo "+$toolchain" run --locked --manifest-path "$manifest" --release --no-default-features
export RUSTFLAGS='-C target-feature=+bulk-memory,+mutable-globals,+simd128,+relaxed-simd'
args=()
if [[ "$mode" == shared ]]; then
  export RUSTFLAGS="$RUSTFLAGS,+atomics -C link-arg=--shared-memory -C link-arg=--max-memory=1073741824"
  args=(-Z build-std=std,panic_abort)
fi
# Build the entire default-feature library as well as the executable (no thread spawn).
cargo "+$toolchain" build --locked --release --lib --target wasm32-unknown-unknown "${args[@]}"
cargo "+$toolchain" rustc --locked --manifest-path "$manifest" --release --target wasm32-unknown-unknown "${args[@]}" -- -C link-arg=--export=main
node tools/wasm-check/run.mjs "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/zenflate-wasm-check.wasm" "$mode"
