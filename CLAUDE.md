# zenflate

Pure Rust port of libdeflate. DEFLATE/zlib/gzip compression and decompression.

## Architecture
- Port of libdeflate (~14,500 lines C) to safe Rust (forbid(unsafe_code) by default)
- Opt-in `unchecked` feature flag for bounds-check elimination in hot paths
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
- `src/fast_bytes.rs` — Unchecked byte load/store helpers (cfg-gated)
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

## Compression Speed vs C (1MB data)

### Default (safe, forbid(unsafe_code))

| Level | Data | zenflate | libdeflate C | Ratio |
|-------|------|----------|-------------|-------|
| L1 | sequential | 735µs | 655µs | 0.89x |
| L1 | zeros | 728µs | 1657µs | 2.28x |
| L1 | mixed | 6360µs | 4833µs | 0.76x |
| L6 | sequential | 1304µs | 1120µs | 0.86x |
| L6 | mixed | 7684µs | 6176µs | 0.80x |
| L12 | sequential | 23.0ms | 13.3ms | 0.58x |
| L12 | mixed | 26.2ms | 17.9ms | 0.68x |

### With --features unchecked

| Level | Data | zenflate | libdeflate C | Ratio | vs safe |
|-------|------|----------|-------------|-------|---------|
| L1 | sequential | 656µs | 659µs | **1.00x** | -10.7% |
| L1 | zeros | 645µs | 1642µs | **2.55x** | -11.5% |
| L1 | mixed | 6059µs | 4689µs | 0.77x | -4.7% |
| L6 | sequential | 1144µs | 1109µs | **0.97x** | -12.2% |
| L6 | zeros | 1066µs | 964µs | 0.90x | -12.5% |
| L6 | mixed | 7128µs | 6200µs | 0.87x | -7.2% |
| L12 | sequential | 17.3ms | 13.3ms | 0.77x | -24.6% |
| L12 | mixed | 24.0ms | 18.1ms | 0.75x | -8.4% |

Remaining gap on mixed data is register pressure from Rust's fat pointers
(`&[u8]` = ptr+len, 2 regs vs C's 1 reg) and missing software prefetch.

## Known Bugs
(none yet)
