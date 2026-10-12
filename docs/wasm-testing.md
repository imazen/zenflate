# Optimized WASM regression and execution

`just test-wasm stable normal` builds the default-feature library and a
standalone test-only executable at opt-level 3 for wasm32-unknown-unknown.
`just test-wasm nightly-2026-10-12 shared` additionally rebuilds std and
panic_abort with atomics, bulk-memory, mutable-globals, simd128 and relaxed-simd,
and links shared memory. Node 24.18.0 instantiates and calls the emitted `main`;
assertions trap on failure. The runner verifies shared memory for that mode.
CI also runs current stable and nightly so compiler updates exercise this gate.

Install the requested Rust toolchain, wasm32-unknown-unknown target and rust-src
(for shared builds), and Node before invoking the recipes. Missing tools,
reference output or malformed WASM are hard failures. No wasm-bindgen shim,
thread spawn, filesystem access or host imports are needed in the WASM module.
Rust's [target documentation](https://doc.rust-lang.org/rustc/platform-support/wasm32-unknown-unknown.html)
explains these standard-library limits.

The separate `tools/wasm-check` package keeps miniz_oxide, crc32fast and
simd-adler32 out of the production dependency graph. It executes the scalar
implementation natively first, then embeds its complete encoded byte sequence
in the WASM build and compares every byte. The scalar result is generated on
the caller's disk and is never committed. The independent readers validate
our encoded payloads; independently encoded zlib streams exercise our decoder.

Coverage includes 32 of 201 new/png efforts (0–31) on small inputs and
13 of 13 libdeflate compatibility levels (0–12), raw/zlib/gzip output, streaming zlib decode, empty
and short inputs, vector tails, match lengths, checksum chunk boundaries,
32 KiB windows and 64 KiB boundaries. Larger inputs exercise representative
efforts through 30. Checksums include continuation and all-255 input.
Truncated streams, corrupted checksums, malformed wrappers and insufficient
output buffers must return errors. The gzip independent payload reader checks
DEFLATE content; our wrapper reader and independent CRC oracle check framing
and checksum separately.

Missing coverage: efforts 32–200, parallel thread execution, browser-specific
integration, every streaming input chunk schedule, exhaustive malformed
bitstreams, full external corpora and performance measurements. Native gates
and existing conformance tests remain in place.

Compiler investigation and released-source reproducer: [WASM compiler regression](wasm-compiler-regression.md).
