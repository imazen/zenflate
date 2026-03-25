# Changelog

## 0.3.2 (2026-03-25)

### Fixed
- **Stack overflow in `Compressor::clone()` with `unchecked` feature.** `NearOptimalState` (~9MB of fixed arrays) was placed on the stack by the derived `Clone`, overflowing the default 8MB thread stack. Downstream crates (zenpng) hit this in beam search filter evaluation.

### Changed
- `NearOptimalState` and `BtMatchfinder` now use `Vec` for large tables regardless of feature flags. The `unchecked` feature controls access patterns (`get_unchecked`, raw pointers), not storage layout. This eliminates the `Box::new_uninit()` construction path and all associated unsafe code.
- Performance benchmarks in the README were measured with 0.3.1. The Vec-based layout may show different performance characteristics at NearOptimal levels (10-12) and should be re-benchmarked.

## 0.3.1 (2026-03-25)

### Fixed
- Cooperative `Stop` checking granularity improved to <10ms in all compression strategy inner loops (was unbounded)
- Added `scalar` fallback tier to `incant!` dispatch in Adler-32 and CRC-32, fixing archmage deprecation warnings

### Changed
- Bumped `archmage` 0.9.9 → 0.9.12
- Bumped `enough` 0.4 → 0.4.2
- Bumped `libm` 0.2 → 0.2.16

### Added
- Corpus benchmarks (Canterbury, Silesia, photos) with fresh measurements and safe-mode columns

## 0.3.0

### Added
- **FullOptimal compression** (Zopfli-style iterative optimization)
  - Katajainen bounded package-merge for optimal Huffman codes
  - Accurate block cost estimation with early-exit max-bits sweep
  - Chunked prefix-sum histograms for O(64) block split cost
  - Lightweight `block_cost_simple` for fast block splitting decisions
  - Squeeze optimizations ported from zenzop
  - Configurable iteration count per effort level
- **Enhanced Huffman optimization**
  - Multi-strategy Huffman code optimization (A2)
  - Exhaustive precode tree header search (A1)
  - Near-optimal parser diversification (A3)
  - `optimize_huffman_for_rle` functions (Brotli-inspired + Zopfli-style)
- `CompressorSnapshot` and cost estimation for incremental API
- `#[must_use]`, `#[non_exhaustive]`, and `Debug` impls on public types
- Byte-identical parity tests for all libdeflate compat levels (0-12) across
  deflate, gzip, and zlib formats with multiple data patterns
- README badges, MSRV section, and AI disclosure

### Fixed
- L1 `compress_fastest`: `next_hash` not persisted across block boundaries,
  causing different output than C libdeflate on multi-block inputs
- gzip header XFL byte not set based on compression level (should be 0x04 for
  fastest, 0x02 for best, matching C libdeflate)
- zlib header FLEVEL mapped level 7 to SLOWEST instead of DEFAULT
- Lazy2 off-by-one in incremental compression skip count
- Hash update guard against OOB in greedy match skip loops
- Swap (dist, length) -> (length, dist) return order from match loop
- `fuse_7` precode encoding counted 8 positions instead of 7
- ECT optimizations suppressed in libdeflate compat mode
- `no_std + alloc` compilation: `f64::log2()` in full-optimal replaced with
  `libm::log2`, unused imports removed

### Changed
- Project description updated: no longer described as "a port of libdeflate"
  but as its own implementation with credited origins
- Module-level doc comments updated to distinguish ported core from extensions
- Removed PNG cost bias from core zenflate (moved to codec layer)
- Reuse `HuffmanScratch` in block splitting, use `FnMut`
- Edition 2024, MSRV 1.89
- Bumped `safe_unaligned_simd` minimum to 0.2.5
- Updated archmage/magetypes to 0.9
- Added `libm` dependency for `no_std` floating-point math
- Removed `safe_unaligned_simd` direct dependency (use archmage prelude re-exports)

## 0.2.1

Fix aarch64 stable compilation (removed nightly-only intrinsics).

## 0.2.0

Initial release.

### Compression (`src/compress/`)
- Bitstream writer, Huffman construction, block flushing (`bitstream.rs`,
  `huffman.rs`, `block.rs`, `sequences.rs`) — ported from libdeflate
- Block splitting with 10-category observation system (`block_split.rs`)
  — ported from libdeflate
- Level 1: `compress_fastest` with ht_matchfinder (`mod.rs`,
  `matchfinder/ht.rs`) — ported from libdeflate
- Levels 2-9: greedy, lazy, lazy2 strategies with hc_matchfinder
  (`mod.rs`, `matchfinder/hc.rs`) — ported from libdeflate
- Levels 10-12: near-optimal parsing with bt_matchfinder
  (`near_optimal.rs`, `matchfinder/bt.rs`) — ported from libdeflate
- Effort-based `CompressionLevel` (0-30) with six strategies and named
  presets replacing libdeflate's fixed 0-12 levels
- Turbo matchfinder (`matchfinder/turbo.rs`) — original, single-entry
  hash with limited skip updates for efforts 1-4
- FastHt matchfinder (`matchfinder/fast_ht.rs`) — original, 2-entry hash
  with limited skip updates for efforts 5-7
- `good_match`/`max_lazy` early-out optimizations
- Parallel gzip compression with pigz-style chunking, 32KB dictionary
  overlap, and CRC-32 combine via GF(2) matrix
- `Clone` for `Compressor` + incremental compression API

### Decompression (`src/decompress/`)
- Core decompressor with decode tables and fastloop (`mod.rs`) — ported
  from libdeflate's `deflate_decompress.c` and `decompress_template.h`
- gzip/zlib wrapper handling with DEFLATE/zlib/gzip format support
- Optimized match copy in fastloop
- `skip_checksum` flag for skipping verification
- Streaming decompression (`streaming.rs`) — original, pull-based API
  with `InputSource` trait, works in `no_std + alloc`
- `BufReadSource` for `std::io::BufRead` integration

### Checksums (`src/checksum/`)
- Adler-32 scalar — ported from libdeflate's `adler32.c`
- Adler-32 SIMD: AVX-512 VNNI, AVX-512, AVX2, NEON, WASM simd128
  — original implementations via archmage
- CRC-32 scalar + slice-by-8 — ported from libdeflate's `crc32.c`
- CRC-32 SIMD: PCLMULQDQ 128-bit, VPCLMULQDQ 512-bit, aarch64 PMULL
  — folding constants from libdeflate, implementations via archmage
- `adler32_combine` and `crc32_combine` for parallel checksum merging
- `Adler32Hasher` and `Crc32Hasher` wrapper structs

### Infrastructure
- `#![forbid(unsafe_code)]` by default, opt-in `unchecked` feature
- `no_std` + `alloc` support (decompression fully stack-allocated)
- `enough` crate integration for `Stop` / cancellation trait
- Criterion benchmarks vs libdeflate, flate2, miniz_oxide, fdeflate, zlib-rs
- GitHub Actions CI with x86_64, i686, aarch64, WASM targets
- Miri CI for unsafe soundness checking
- cargo-fuzz infrastructure
- Justfile and Dockerfile
