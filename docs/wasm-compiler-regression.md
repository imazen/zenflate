# WASM compiler regression (2026-10-12)


The unmodified crates.io 0.3.6 archive (SHA-256
`8409543ad469f954834da58409f0d9c463156f01cfaf8a8a03fe277c73b1e0f5`)
reproduces SIGSEGV under Rust 1.99.0, LLVM 23.1.1, with opt-level 3 and
`-C target-feature=+atomics,+bulk-memory,+mutable-globals,+simd128,+relaxed-simd`
and `-Z build-std=std,panic_abort`. Run
`just reproduce-wasm-036 /absolute/absent/scratch 1.99.0` to reproduce;
RUSTC_BOOTSTRAP is used only to reproduce build-std on this stable compiler.
The recipe is intentionally expected to fail on the affected compiler.

A single-codegen-unit opt-bisect trace identifies InstCombine processing
`adler32_impl_wasm128` immediately before the crash. The full LLVM root cause
has not been reduced to an individual intrinsic or submitted upstream.
Changing the two libm log2 calls to std log2 does not eliminate the crash.
Replacing the four constant vector loads with vector constructors also fails.

The SIMD module restructuring already landed in
`a720edf4f52c79707ec1e56ec6ea29397058b7ea`. Transplanting only the current
Adler-32 module arrangement into the released source (with the simd module
unconditional because 0.3.6 has no simd feature) makes the same optimized
build pass. Reproduce that isolated experiment with
`just reproduce-wasm-036 /absolute/absent/scratch 1.99.0 module-workaround`.
The WASM Adler-32 function's arithmetic is identical. This is a
validated source-shape workaround for a compiler fault, not a compiler fix.
Keep the module organization; SIMD remains enabled. Current 0.4.1 requires
no algorithm replacement or optimization downgrade.

A consumer requiring `zenflate = "0.3"` does not select 0.4.1. Upgrade that
manifest requirement explicitly to 0.4.1 (and update its lockfile), or use an
explicitly reviewed compatible backport. For default-features=false users,
compression now requires `compress` and SIMD requires `simd`. Changing only a
sibling 0.4.x dependency cannot fix another dependency's registry 0.3.6 copy.

## Current-nightly regression (2026-10-13)

The 0.4.1 release gates caught the InstCombine crash again on Rust nightly
`e9bbadf68` (2026-10-12), LLVM 23.1.3. Stable 1.99.0 and pinned
nightly-2026-10-12 passed; the rolling nightly gate failed on the next compiler.
`just test-wasm nightly shared` reproduces the failure on main before the fix.
A single-codegen-unit opt-bisect trace again ends at InstCombine processing
`adler32_impl_wasm128`, immediately after loop rotation. The underlying LLVM
fault is still not reduced to a single intrinsic.

The WASM Adler-32 implementation now initializes its position sums and first
byte sum from the first 32-byte pair, then processes the remaining pairs in
the existing SIMD loop. This removes the loop-carried zero initialization
from those sums without changing weights, chunk bounds, output, SIMD features
or optimization. Native implementations and public signatures are unchanged.
The repaired source passes the current-nightly optimized shared-memory build
and executes the independent checksum/codec oracles and scalar-byte parity
checks. This is a further source-shape workaround, not an LLVM fix.
