#!/usr/bin/env bash
# Public-safe released-source compiler regression. Scratch must be supplied explicitly.
set -euo pipefail
scratch=${1:?usage: reproduce-wasm-036.sh ABSOLUTE_SCRATCH [TOOLCHAIN]}
toolchain=${2:-1.99.0}
workaround=${3:-original}
case "$workaround" in original|module-workaround) ;; *) echo 'expected original or module-workaround' >&2; exit 2 ;; esac
project=$(cd "$(dirname "$0")/.." && pwd)
[[ "$scratch" == /* && ! -e "$scratch" ]] || { echo 'scratch must be an absent absolute path' >&2; exit 2; }
mkdir -p "$scratch"
export TMPDIR="$scratch"
curl --fail --location https://static.crates.io/crates/zenflate/zenflate-0.3.6.crate -o "$scratch/source.crate"
printf '%s  %s\n' 8409543ad469f954834da58409f0d9c463156f01cfaf8a8a03fe277c73b1e0f5 "$scratch/source.crate" | sha256sum --check
tar xf "$scratch/source.crate" -C "$scratch"
if [[ "$workaround" == module-workaround ]]; then
  python3 - "$project/src/checksum/adler32.rs" "$scratch/zenflate-0.3.6/src/checksum/adler32.rs" <<'PYTHON'
import sys
source = open(sys.argv[1]).read()
start = source.index("pub fn adler32(")
end = source.index("/// Combine two Adler", start)
source = source[:start] + "pub fn adler32(adler: u32, data: &[u8]) -> u32 {\n    simd::adler32_dispatch(adler, data)\n}\n\n" + source[end:]
source = source.replace('#[cfg(feature = "simd")]\nmod simd', 'mod simd')
open(sys.argv[2], "w").write(source)
PYTHON
fi
export CARGO_TARGET_DIR="$scratch/target"
export RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals,+simd128,+relaxed-simd'
rustc "+$toolchain" -Vv
# BOOTSTRAP is only for reproducing nightly build-std with the affected stable compiler.
if [[ "$toolchain" != nightly* ]]; then export RUSTC_BOOTSTRAP=1; fi
cargo "+$toolchain" build --locked --release --lib --target wasm32-unknown-unknown -Z build-std=std,panic_abort --manifest-path "$scratch/zenflate-0.3.6/Cargo.toml"
