# zenflate

Pure Rust DEFLATE/zlib/gzip compression and decompression.

## Architecture
- Built on libdeflate's core algorithms, extended with Zopfli-style optimal parsing, multi-strategy Huffman optimization, and original matchfinder designs
- Safe Rust (forbid(unsafe_code) by default)
- Opt-in `unchecked` feature flag for bounds-check elimination in hot paths
- SIMD via archmage/magetypes
- Side-by-side testing against C via `libdeflater` crate

## Source Reference
- C source: `/home/lilith/work/libdeflate-src/lib/`

## Module Map
- `src/constants.rs` — DEFLATE format constants (from deflate_constants.h)
- `src/error.rs` — Error types
- `src/checksum/` — Adler-32 and CRC-32 (scalar + SIMD)
- `src/decompress/` — Decompression (bitstream reader, Huffman tables, inflate loop, streaming)
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
  - Adler-32: AVX-512 VNNI 512-bit (v4x) + AVX-512 (v4) + AVX2 (v3) + NEON + WASM simd128 — 123 GiB/s (1.01x C)
  - CRC-32: PCLMULQDQ 128-bit (v2) + VPCLMULQDQ 512-bit zmm (modern) + PMULL (aarch64, NeonAes) — 78 GiB/s (1.00x C)
  - Decompression fastloop + optimized match copy
- [x] Phase 6: Benchmarks + Polish (criterion benchmarks, README, doc examples, #[non_exhaustive] errors)
- [x] Phase 7: Ecosystem benchmarks (flate2, miniz_oxide), justfile, Dockerfile, CI bench checks
- [x] Phase 8: Streaming decompression (StreamDecompressor, InputSource trait, fill/peek/advance API, BufRead/Read impls, 15 tests)
- [x] Phase 9: Effort-based compression (0-30) with new strategies
  - CompressionLevel::new(effort) with effort 0-30, Pareto-ranked
  - CompressionLevel::libdeflate(level) for byte-identical C parity (0-12)
  - Turbo (effort 1-4): dynamic Huffman + single-entry hash, limited skip updates
  - FastHt (effort 5-7): dynamic Huffman + 2-entry hash, limited skip updates
  - Named presets: none(), fastest(), fast(), balanced(), high(), best()
  - 195 tests + 10 doctests pass


## Compression Speed

All benchmarks: safe = default `forbid(unsafe_code)`, unchecked = `--features unchecked`.
`unchecked` eliminates bounds checks in hot compression loops (+5-12% at L1, +0-6% at L6+).
It does NOT help decompression (safe bounds checks give LLVM information for better codegen).

### Standard Corpus Compression

Canterbury (11 text files, 2.7 MiB), Silesia (11 mixed files, 153 MiB), gb82 photos (5 raw RGB, 4.75 MiB).
Aggregate throughput across all files in each corpus.

| Corpus | Level | Safe | Unchecked | C | vs C (unc) | vs flate2 (unc) |
|--------|-------|------|-----------|---|------------|-----------------|
| Canterbury | L1 | 355 MiB/s | 431 MiB/s | 381 MiB/s | **1.13x** | 0.98x |
| Canterbury | L6 | 295 MiB/s | 347 MiB/s | 131 MiB/s | **2.66x** | **3.21x** |
| Canterbury | L12 | 134 MiB/s | 147 MiB/s | 8 MiB/s | **19.6x** | **9.69x** |
| Silesia | L1 | ~310 MiB/s | 365 MiB/s | 319 MiB/s | **1.14x** | 0.97x |
| Silesia | L6 | ~250 MiB/s | 289 MiB/s | 128 MiB/s | **2.25x** | **3.00x** |
| Silesia | L12 | ~130 MiB/s | 146 MiB/s | 7 MiB/s | **19.7x** | **3.09x** |
| Photos (RGB) | L1 | 202 MiB/s | 222 MiB/s | 193 MiB/s | **1.15x** | **1.24x** |
| Photos (RGB) | L6 | 162 MiB/s | 162 MiB/s | 114 MiB/s | **1.42x** | **2.31x** |
| Photos (RGB) | L12 | 93 MiB/s | 99 MiB/s | 18 MiB/s | **5.53x** | **2.23x** |

Silesia safe values estimated from unchecked (thermal throttling affected the long safe run).
Canterbury and photos safe values measured directly.

Note: flate2 uses zlib-rs backend. At L1, flate2 uses static Huffman + 4K hash table
(faster but worse ratio than zenflate's dynamic Huffman). zenflate L6 is 2-3x faster
than both C and flate2 across all corpus types. L12 gap vs C is huge because zenflate's
near-optimal is a fundamentally different (faster) algorithm.

### Per-file Silesia L6

| File | Unchecked | C | vs C | vs flate2 |
|------|-----------|---|------|-----------|
| dickens (10M text) | 221 MiB/s | 81 MiB/s | **2.73x** | **3.25x** |
| nci (33M chemistry) | 872 MiB/s | 290 MiB/s | **3.01x** | **3.71x** |
| reymont (6.6M text) | 299 MiB/s | 90 MiB/s | **3.34x** | **5.30x** |
| samba (21M source) | 400 MiB/s | 157 MiB/s | **2.55x** | **3.40x** |
| sao (7M binary) | 153 MiB/s | 86 MiB/s | **1.78x** | **2.45x** |
| webster (41M dict) | 286 MiB/s | 114 MiB/s | **2.51x** | **3.32x** |
| x-ray (8.5M image) | 150 MiB/s | 134 MiB/s | **1.12x** | **1.99x** |
| xml (5.3M data) | 595 MiB/s | 221 MiB/s | **2.69x** | **3.33x** |

### Synthetic Photo Bitmap All Levels (3 MiB)

| Level | Safe | Unchecked | C | vs C (unc) |
|-------|------|-----------|---|------------|
| L1 | 606 MiB/s | 679 MiB/s | 582 MiB/s | **1.17x** |
| L2 | 615 MiB/s | 674 MiB/s | 408 MiB/s | **1.65x** |
| L4 | 610 MiB/s | 678 MiB/s | 411 MiB/s | **1.65x** |
| L6 | 468 MiB/s | 471 MiB/s | 403 MiB/s | **1.17x** |
| L9 | 468 MiB/s | 479 MiB/s | 383 MiB/s | **1.25x** |
| L10 | 306 MiB/s | 321 MiB/s | 178 MiB/s | **1.81x** |
| L12 | 295 MiB/s | 309 MiB/s | 150 MiB/s | **2.06x** |

Byte-identical output at every level.

### Ecosystem Comparison (3 MiB photo bitmap)

| Library | Level | Safe | Unchecked |
|---------|-------|------|-----------|
| zenflate | L6 | 468 MiB/s | 471 MiB/s |
| zenflate | L9 | 468 MiB/s | 479 MiB/s |
| zenflate | L12 | 295 MiB/s | 309 MiB/s |
| flate2 (zlib-rs) | L1 | 456 MiB/s | 455 MiB/s |
| miniz_oxide | L9 | 175 MiB/s | 176 MiB/s |

zenflate L6 is ~2.7x faster than flate2/miniz_oxide at comparable ratios.

### Synthetic Data Compression (1 MiB)

| Level | Data | Safe | Unchecked | C | vs C (unc) |
|-------|------|------|-----------|---|------------|
| L1 | mixed | 4.5ms | 4.1ms | 4.7ms | **1.15x** |
| L6 | mixed | 6.0ms | 6.0ms | 6.1ms | **1.02x** |
| L12 | mixed | 8.3ms | 7.9ms | 17.7ms | **2.25x** |
| L1 | photo | 4.9ms | 4.4ms | 5.2ms | **1.17x** |
| L6 | photo | 6.4ms | 6.4ms | 7.5ms | **1.17x** |
| L12 | photo | 10.2ms | 9.7ms | 20.0ms | **2.06x** |

Sequential/zeros omitted — too synthetic (zenflate 5-14x faster than C on ultra-repetitive data).

### `unchecked` Feature Benefit (compression only)

| Level | Data | Speedup |
|-------|------|---------|
| L1 | mixed | +11% |
| L6 | mixed | +0% |
| L12 | mixed | +6% |
| L1 | photo | +12% |
| L6 | photo | +1% |
| L12 | photo | +5% |

`unchecked` helps most at L1 (bounds checks in turbo hash lookups), barely at L6+.
Does NOT help decompression at all — safe is equal or faster.

### Parallel Compression (4 MiB mixed data)

| Level | 1T (safe) | 1T (unc) | 4T (safe) | 4T (unc) | Speedup (4T) |
|-------|-----------|----------|-----------|----------|--------------|
| L1 | 18.4ms | 16.6ms | 5.8ms | 5.3ms | **3.1x** |
| L6 | 24.0ms | 23.9ms | 7.3ms | 7.2ms | **3.3x** |
| L12 | 33.8ms | 32.3ms | 10.1ms | 9.5ms | **3.4x** |

Pigz-style chunking: equal-sized chunks with 32KB dictionary overlap,
sync flush at boundaries, combined CRC-32 via GF(2) matrix.

## Decompression Speed

`unchecked` does NOT help decompression — safe bounds checks give LLVM information
that enables better optimization. All decompression numbers are from safe mode.

### Synthetic Data (1 MiB, compressed at L6)

| Data | zenflate | C | fdeflate | zlib-rs | flate2 (zlib-rs) | miniz_oxide | vs C |
|------|----------|---|----------|---------|------------------|-------------|------|
| Sequential | 46µs | 35µs | 90µs | 49µs | 36µs | 81µs | 0.77x |
| Zeros | 35µs | 54µs | 56µs | 42µs | 29µs | 73µs | **1.57x** |
| Mixed | 1.32ms | 1.25ms | 1.42ms | 1.55ms | 1.54ms | 1.70ms | 0.95x |
| Photo | 1.47ms | 1.39ms | 1.52ms | 1.75ms | 1.73ms | 2.01ms | 0.95x |

flate2 uses zlib-rs backend, which is much faster than old miniz_oxide backend.

### Corpus Decompression (L6, selected files)

| File | zenflate | C | fdeflate | zlib-rs | flate2 | vs C |
|------|----------|---|----------|---------|--------|------|
| dickens (10M) | 746 MiB/s | 1098 MiB/s | 771 MiB/s | 814 MiB/s | 824 MiB/s | 0.68x |
| samba (21M) | 1159 MiB/s | 1669 MiB/s | 1123 MiB/s | 1291 MiB/s | 1338 MiB/s | 0.69x |
| xml (5.3M) | 1717 MiB/s | 2732 MiB/s | 1571 MiB/s | 2020 MiB/s | 2054 MiB/s | 0.63x |
| sao (7M binary) | 656 MiB/s | 801 MiB/s | 572 MiB/s | 627 MiB/s | 645 MiB/s | 0.82x |
| x-ray (8.5M) | 593 MiB/s | 699 MiB/s | 561 MiB/s | 541 MiB/s | 580 MiB/s | 0.85x |
| dog (photo RGB) | 595 MiB/s | 633 MiB/s | 571 MiB/s | 539 MiB/s | 553 MiB/s | 0.94x |

Gap vs C: 0.63-0.94x across real-world data. Largest gap on highly compressible
text (xml, dickens); smallest on binary/photo data.

### Streaming Decompression (1 MiB, compressed at L6)

| Data | whole | stream (64K) | stream (4K) | overhead (64K) | fdeflate |
|------|-------|--------------|-------------|----------------|----------|
| Sequential | 46µs | 55µs | 113µs | 1.19x | 90µs |
| Zeros | 35µs | 54µs | 105µs | 1.57x | 56µs |
| Mixed | 1.31ms | 1.49ms | 1.66ms | 1.14x | 1.41ms |
| Photo | 1.46ms | 1.63ms | 1.82ms | 1.12x | 1.51ms |

## Checksums (1 MiB sequential)

| Algorithm | Safe | Unchecked | C | vs C (unc) |
|-----------|------|-----------|---|------------|
| Adler-32 | 117 GiB/s | 123 GiB/s | 121 GiB/s | **1.01x** |
| CRC-32 | 77 GiB/s | 78 GiB/s | 78 GiB/s | **1.00x** |

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

### Decompression: unchecked hurts, SIMD won't help
- `get_unchecked` in table lookups, literal stores, match copy REGRESSES 5-6% on mixed/photo
- LLVM uses safe bounds checks to prove variable relationships → better codegen
- Assembly confirms: unchecked has fewer lines (2020 vs 2146) and panic sites (18 vs 24), yet slower
- C libdeflate does NOT use explicit SIMD for decompression match copy either
- Only x86-specific decompression opt in C is BMI2 BZHI for bit extraction
- Decompression gap vs C is from register pressure / instruction count, not SIMD

## Known Bugs

(none currently)
