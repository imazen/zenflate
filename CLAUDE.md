# zenflate

Pure Rust port of libdeflate. DEFLATE/zlib/gzip compression and decompression.

## Architecture
- Port of libdeflate (~14,500 lines C) to safe Rust with `#![forbid(unsafe_code)]`
- SIMD via archmage/magetypes
- Side-by-side testing against C via `libdeflater` crate

## Source Reference
- C source: `/home/lilith/work/libdeflate-src/lib/`

## Module Map
- `src/constants.rs` — DEFLATE format constants (from deflate_constants.h)
- `src/error.rs` — Error types
- `src/checksum/` — Adler-32 and CRC-32 (scalar + SIMD)
- `src/decompress/` — Decompression (bitstream reader, Huffman tables, inflate loop)
- `src/compress/` — Compression (bitstream writer, Huffman construction, block flushing, strategies)
- `src/matchfinder/` — Hash table, hash chain, binary tree matchfinders
- `src/decompress/mod.rs` — gzip/zlib wrappers integrated into Decompressor

## Implementation Status
- [x] Phase 1: Foundation + Checksums (Adler-32, CRC-32 scalar, 23 parity tests)
- [x] Phase 2: Decompression (generic loop, all 3 formats, 10 parity tests at all levels)
- [x] Phase 3: Compression Core (bitstream writer, Huffman construction, block flushing, 55 tests)
- [x] Phase 4: Compression Strategies (levels 0-12: fastest, greedy, lazy, lazy2, near-optimal; 97 tests)
- [x] Phase 5: SIMD Acceleration
  - Adler-32: AVX2 (v3) + AVX-512 VNNI (modern) — 99 GiB/s (0.82x C)
  - CRC-32: PCLMULQDQ 128-bit (v2) + VPCLMULQDQ 512-bit zmm (modern) — 78 GiB/s (1.00x C)
  - Decompression fastloop + optimized match copy
- [x] Phase 6: Benchmarks + Polish (criterion benchmarks, README, doc examples, #[non_exhaustive] errors)

## Archmage Patches (local only)
The following files in `~/.cargo/registry/src/` were patched to add `pclmulqdq` to X64V2Token:
- `archmage-macros-0.7.0/src/generated/registry.rs` — added pclmulqdq to V2+ feature lists
- `archmage-0.7.0/src/tokens/generated/x86.rs` — V2 runtime detection + const strings
These must be re-applied after any `cargo update` of archmage.

## Known Bugs
(none yet)
