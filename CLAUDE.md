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
  - Adler-32: AVX2 (v3) + AVX-512 VNNI (modern) + NEON (aarch64) — 105 GiB/s (0.88x C)
  - CRC-32: PCLMULQDQ 128-bit (v2) + VPCLMULQDQ 512-bit zmm (modern) + PMULL (aarch64) — 78 GiB/s (1.01x C)
  - Decompression fastloop + optimized match copy
- [x] Phase 6: Benchmarks + Polish (criterion benchmarks, README, doc examples, #[non_exhaustive] errors)
- [x] Phase 7: Ecosystem benchmarks (flate2, miniz_oxide), justfile, Dockerfile, CI bench checks

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

| Level | Data | zenflate | libdeflate C | Ratio |
|-------|------|----------|-------------|-------|
| L1 | sequential | 701µs | 651µs | **0.93x** |
| L1 | zeros | 686µs | 1659µs | **2.42x** |
| L1 | mixed | 5.88ms | 4.70ms | 0.80x |
| L6 | sequential | 1074µs | 1114µs | **1.04x** |
| L6 | zeros | 950µs | 962µs | **1.01x** |
| L6 | mixed | 7.43ms | 6.03ms | 0.81x |
| L12 | sequential | 14.6ms | 13.4ms | **0.92x** |
| L12 | zeros | 14.3ms | 13.4ms | 0.94x |
| L12 | mixed | 20.1ms | 17.6ms | 0.88x |

### All levels, photo bitmap (3 MiB, built-in reproducible data)

| Level | Ratio | Safe | Unchecked | C | vs C |
|-------|-------|------|-----------|---|------|
| L1 | 91.69% | 134 MiB/s | 149 MiB/s | 185 MiB/s | 0.81x |
| L2 | 92.36% | 104 MiB/s | 109 MiB/s | 128 MiB/s | 0.85x |
| L3 | 92.36% | 105 MiB/s | 108 MiB/s | 128 MiB/s | 0.84x |
| L4 | 92.36% | 98 MiB/s | 106 MiB/s | 128 MiB/s | 0.83x |
| L5 | 92.31% | 99 MiB/s | 103 MiB/s | 123 MiB/s | 0.84x |
| L6 | 92.31% | 102 MiB/s | 105 MiB/s | 120 MiB/s | 0.88x |
| L7 | 92.31% | 103 MiB/s | 105 MiB/s | 126 MiB/s | 0.83x |
| L8 | 92.31% | 101 MiB/s | 104 MiB/s | 114 MiB/s | 0.91x |
| L9 | 92.31% | 102 MiB/s | 104 MiB/s | 119 MiB/s | 0.87x |
| L10 | 91.97% | 38 MiB/s | 47 MiB/s | 54 MiB/s | 0.87x |
| L11 | 91.88% | 37 MiB/s | 47 MiB/s | 52 MiB/s | 0.90x |
| L12 | 91.80% | 33 MiB/s | 39 MiB/s | 44 MiB/s | 0.89x |

Byte-identical output at every level. Speed gap is 0.81-0.91x C.
`unchecked` helps most at L10-12 (+18-27%), modest at L1-9 (+2-11%).
Ratio is flat L2-9 (92.31-92.36%); near-optimal L10-12 squeezes to 91.80%.

### Ecosystem comparison (3 MiB photo bitmap, unchecked)

| Library | Level | Speed | Ratio |
|---------|-------|-------|-------|
| zenflate | 6-9 | 104-105 MiB/s | 92.31% |
| zenflate | 12 | 39 MiB/s | 91.80% |
| flate2 | 1 | 291 MiB/s | 91.70% |
| flate2 | 4-9 (best) | 55 MiB/s | 91.58% |
| miniz_oxide | 4-9 (best) | 55 MiB/s | 91.58% |

zenflate L6-9 is ~2x faster than flate2 at comparable ratios.
On this photo data, flate2 L9 (91.58%) slightly beats zenflate L12 (91.80%)
on ratio — different algorithms have different strengths on different data.

### Parallel Compression (4MB mixed data, --features unchecked)

| Level | 1 thread | 2 threads | 4 threads | Speedup (4T) |
|-------|----------|-----------|-----------|--------------|
| L1 | 23.7ms | 12.8ms | 7.1ms | **3.3x** |
| L6 | 28.8ms | 15.5ms | 8.7ms | **3.3x** |
| L12 | 82.9ms | 45.2ms | 28.3ms | **2.9x** |

Parallel compression uses pigz-style chunking: equal-sized chunks with 32KB
dictionary overlap, sync flush at boundaries, combined CRC-32 via GF(2) matrix.

### Decompression (1MB, compressed at L6, --features unchecked)

| Data type | zenflate | libdeflate (C) | flate2 | miniz_oxide |
|-----------|----------|----------------|--------|-------------|
| Sequential | 27.7 GiB/s | 31.6 GiB/s | 7.2 GiB/s | 6.6 GiB/s |
| Zeros | 34.6 GiB/s | 14.5 GiB/s | 26.6 GiB/s | 17.2 GiB/s |
| Mixed | 717 MiB/s | 795 MiB/s | 585 MiB/s | 571 MiB/s |

### Checksums (1MB sequential, --features unchecked)

| Algorithm | zenflate | libdeflate (C) | Ratio |
|-----------|----------|----------------|-------|
| Adler-32 | 105 GiB/s | 120 GiB/s | 0.88x |
| CRC-32 | 78 GiB/s | 77 GiB/s | **1.01x** |

## Investigation Notes

### L1 +48% instruction overhead (callgrind)
- NOT panic-related (zero panic calls in assembly)
- Root cause: register pressure from fat pointers + separate hash table allocation
- 18 stack spills in Rust hot loop vs 2 in C
- Stack frame: 232 bytes Rust vs 104 bytes C (2.2x)
- Raw pointers DON'T help (LLVM already proves bounds for simple 2-entry hash table)
- Embedding HtMatchfinder inline in Compressor struct does NOT help — regressed +14.8%
  - 805 asm lines (vs 744), 124 stack refs (vs 112), 248 byte frame (vs 232)
  - Hash table at offset 0x11f8 from self means larger displacements everywhere
  - LLVM may lose noalias reasoning (Box = separate object, inline = same object)
- `ht.rs` has `longest_match_raw`/`skip_bytes_raw` available but unused

### Callgrind instruction counts (all levels, unchecked, 1MB sequential)
| Level | zenflate | C | Overhead |
|-------|---------|---|----------|
| L1 | 86.8M | 58.5M | +48% |
| L6 | 116.0M | 93.1M | +25% |
| L9 | 117.9M | 96.3M | +22% |
| L12 | 361.4M | 302.3M | +20% |

Cachegrind: D1 cache misses nearly identical. Gap is pure instruction count.

## Known Bugs
(none yet)
